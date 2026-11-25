//! Manual inventory adjustments: existing tenant product, observed state and a
//! durable identity receipt. This is independent of offline sale reconciliation.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    pub id: String,
    pub payload: Adjustment,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Adjustment {
    pub item_id: String,
    pub quantity_change: i32,
    pub expected_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_sold_out: Option<bool>,
}
impl Mutation {
    pub fn valid(&self) -> bool {
        valid_id(&self.id)
            && valid_product_id(&self.payload.item_id)
            && self.payload.expected_version.len() == 64
            && self
                .payload
                .expected_version
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            && self.payload.quantity_change != i32::MIN
            && (self.payload.quantity_change != 0 || self.payload.is_sold_out.is_some())
            && self.payload.location_id.as_deref().is_none_or(valid_id)
    }
}
fn valid_product_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 512 && id.trim() == id && !id.chars().any(char::is_control)
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
#[derive(Debug)]
pub enum Error {
    Database(sqlx::Error),
    Blocked(&'static str),
    Unconfirmed(&'static str),
}
impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub id: String,
    pub item_id: String,
    pub status: String,
    pub quantity_change: i32,
    pub previous_version: String,
    pub inventory_version: String,
    pub stock: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
struct Level {
    id: String,
    available_count: i32,
    committed_count: i32,
}
fn integer(product: &Value, field: &str) -> Result<i64, Error> {
    product[field]
        .as_i64()
        .filter(|v| *v >= 0 && *v <= i64::from(i32::MAX))
        .ok_or(Error::Blocked("invalid_stored_inventory"))
}
fn available(product: &Value, levels: &[Level]) -> Result<i64, Error> {
    if levels.is_empty() {
        return integer(product, "inventory_count");
    }
    let mut count = 0_i64;
    for level in levels {
        if level.available_count < 0 || level.committed_count < 0 {
            return Err(Error::Blocked("invalid_stored_inventory"));
        }
        count = count
            .checked_add(i64::from(level.available_count))
            .ok_or(Error::Blocked("inventory_overflow"))?;
    }
    if count > i64::from(i32::MAX) {
        return Err(Error::Blocked("inventory_overflow"));
    }
    Ok(count)
}
fn version(tenant: &str, id: &str, product: &Value, levels: &[Level]) -> String {
    format!(
        "{:x}",
        Sha256::digest(json!([tenant, id, product, levels]).to_string().as_bytes())
    )
}
struct Plan {
    stock: i64,
    total: i64,
    available: i64,
    level_changes: Vec<(String, i32)>,
    counters: Option<(i64, i64)>,
}
fn plan(product: &Value, levels: &[Level], delta: i32) -> Result<Plan, Error> {
    let current = available(product, levels)?;
    let committed = if levels.is_empty() {
        product["locked_quantity"].as_i64().unwrap_or(0)
    } else {
        levels
            .iter()
            .try_fold(0_i64, |sum, level| {
                sum.checked_add(i64::from(level.committed_count))
            })
            .ok_or(Error::Blocked("inventory_overflow"))?
    };
    if committed < 0 {
        return Err(Error::Blocked("invalid_stored_inventory"));
    }
    let available_now = if levels.is_empty() {
        current
            .checked_sub(committed)
            .ok_or(Error::Blocked("invalid_stored_inventory"))?
    } else {
        current
    };
    if available_now < 0 {
        return Err(Error::Blocked("invalid_stored_inventory"));
    }
    let remaining = available_now
        .checked_add(i64::from(delta))
        .filter(|v| *v >= 0 && *v <= i64::from(i32::MAX))
        .ok_or(Error::Blocked("insufficient_or_overflowing_stock"))?;
    let total = remaining
        .checked_add(committed)
        .filter(|v| *v <= i64::from(i32::MAX))
        .ok_or(Error::Blocked("inventory_overflow"))?;
    let mut level_changes = Vec::new();
    // Locations are one shared pool, matching InventoryService. A deduction is
    // distributed once; replenishment goes to the first existing pool row. Do
    // not create locations, require a new location policy, or multiply a delta.
    if delta > 0 {
        if let Some(first) = levels.first() {
            first
                .available_count
                .checked_add(delta)
                .ok_or(Error::Blocked("inventory_overflow"))?;
            level_changes.push((first.id.clone(), delta));
        }
    } else {
        let mut remaining_delta = -i64::from(delta);
        for level in levels {
            let take = remaining_delta.min(i64::from(level.available_count));
            if take > 0 {
                level_changes.push((level.id.clone(), -(take as i32)));
                remaining_delta -= take;
            }
        }
    }
    let counters = match (
        product.get("pn_counter_p").filter(|value| !value.is_null()),
        product.get("pn_counter_n").filter(|value| !value.is_null()),
    ) {
        (Some(p), Some(n)) => {
            let p = p
                .as_i64()
                .ok_or(Error::Blocked("invalid_stored_inventory"))?;
            let n = n
                .as_i64()
                .ok_or(Error::Blocked("invalid_stored_inventory"))?;
            let old_total = total - i64::from(delta);
            if p < n {
                return Err(Error::Blocked("inventory_debt_requires_reconciliation"));
            }
            if p < 0 || n < 0 || p.checked_sub(n) != Some(old_total) {
                return Err(Error::Blocked("inventory_counters_require_review"));
            }
            Some((
                p.checked_add(i64::from(delta.max(0)))
                    .ok_or(Error::Blocked("inventory_overflow"))?,
                n.checked_add((-i64::from(delta)).max(0))
                    .ok_or(Error::Blocked("inventory_overflow"))?,
            ))
        }
        (None, None) => None,
        _ => return Err(Error::Blocked("inventory_counters_require_review")),
    };
    Ok(Plan {
        stock: if levels.is_empty() { total } else { remaining },
        total,
        available: remaining,
        level_changes,
        counters,
    })
}
fn make_receipt(m: &Mutation, inventory_version: String, stock: i64) -> Receipt {
    Receipt {
        id: m.id.clone(),
        item_id: m.payload.item_id.clone(),
        status: "acknowledged".into(),
        quantity_change: m.payload.quantity_change,
        previous_version: m.payload.expected_version.clone(),
        inventory_version,
        stock,
    }
}
fn replay(saved: (String, String), identity: &str) -> Result<Receipt, Error> {
    if saved.0 != identity {
        return Err(Error::Unconfirmed("request_identity_changed"));
    }
    serde_json::from_str(&saved.1).map_err(|_| Error::Unconfirmed("invalid_saved_receipt"))
}
const PG_PRODUCT: &str =
    "SELECT to_jsonb(p) FROM products p WHERE id=$1 AND tenant_id=$2 FOR UPDATE";
const PG_LEVELS: &str = "SELECT id,available_count,committed_count FROM inventory_levels WHERE variant_id=$1 AND tenant_id=$2 ORDER BY id FOR UPDATE";
// This portable CAS SQL is also executed against SQLite in the source/SQL gate.
pub(super) const UPDATE_PRODUCT: &str = "UPDATE products SET inventory_count=$1,available_quantity=$2,is_sold_out=COALESCE($3,is_sold_out),updated_at=$4 WHERE id=$5 AND tenant_id=$6 AND inventory_count=$7";
pub(super) const UPDATE_LEVEL: &str = "UPDATE inventory_levels SET available_count=available_count+$1,updated_at=$2 WHERE id=$3 AND tenant_id=$4 AND available_count+$1 >= 0 AND available_count+$1 <= 2147483647";
pub(super) const INSERT_RECEIPT: &str = "INSERT INTO inventory_adjustment_receipts (tenant_id,client_mutation_id,item_id,request_identity,receipt_json) VALUES ($1,$2,$3,$4,$5)";
pub async fn apply_postgres(pool: &PgPool, tenant: &str, m: &Mutation) -> Result<Receipt, Error> {
    if !m.valid() {
        return Err(Error::Blocked("invalid_adjustment"));
    }
    let mut tx = pool.begin().await?;
    server_common::auth_utils::set_org_context(&mut *tx, tenant).await?;
    let identity = serde_json::to_string(m).map_err(|_| Error::Blocked("invalid_adjustment"))?;
    let saved: Option<(String, String)> = sqlx::query_as("SELECT request_identity,receipt_json FROM inventory_adjustment_receipts WHERE tenant_id=$1 AND client_mutation_id=$2 FOR UPDATE")
        .bind(tenant).bind(&m.id).fetch_optional(&mut *tx).await?;
    if let Some(saved) = saved {
        let result = replay(saved, &identity)?;
        tx.commit().await?;
        return Ok(result);
    }
    let product: Option<Value> = sqlx::query_scalar(PG_PRODUCT)
        .bind(&m.payload.item_id)
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
    // A concurrent identical request may have committed while this row lock waited.
    let saved: Option<(String, String)> = sqlx::query_as("SELECT request_identity,receipt_json FROM inventory_adjustment_receipts WHERE tenant_id=$1 AND client_mutation_id=$2 FOR UPDATE")
        .bind(tenant).bind(&m.id).fetch_optional(&mut *tx).await?;
    if let Some(saved) = saved {
        let result = replay(saved, &identity)?;
        tx.commit().await?;
        return Ok(result);
    }
    let product = product.ok_or(Error::Blocked("product_not_found"))?;
    let legacy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM applied_client_mutations WHERE tenant_id=$1 AND client_mutation_id=$2)").bind(tenant).bind(&m.id).fetch_one(&mut *tx).await?;
    if legacy {
        return Err(Error::Unconfirmed("legacy_adjustment_requires_review"));
    }
    let levels = sqlx::query_as::<_, Level>(PG_LEVELS)
        .bind(&m.payload.item_id)
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await?;
    if version(tenant, &m.payload.item_id, &product, &levels) != m.payload.expected_version {
        return Err(Error::Blocked("inventory_version_changed"));
    }
    let planned = plan(&product, &levels, m.payload.quantity_change)?;
    let now = chrono::Utc::now();
    let changed = sqlx::query(UPDATE_PRODUCT)
        .bind(planned.total)
        .bind(planned.available)
        .bind(m.payload.is_sold_out)
        .bind(now)
        .bind(&m.payload.item_id)
        .bind(tenant)
        .bind(integer(&product, "inventory_count")?)
        .execute(&mut *tx)
        .await?;
    if changed.rows_affected() != 1 {
        return Err(Error::Blocked("inventory_version_changed"));
    }
    if let Some((p, n)) = planned.counters {
        sqlx::query(
            "UPDATE products SET pn_counter_p=$1,pn_counter_n=$2 WHERE id=$3 AND tenant_id=$4",
        )
        .bind(p)
        .bind(n)
        .bind(&m.payload.item_id)
        .bind(tenant)
        .execute(&mut *tx)
        .await?;
    }
    for (id, delta) in &planned.level_changes {
        if sqlx::query(UPDATE_LEVEL)
            .bind(delta)
            .bind(now)
            .bind(id)
            .bind(tenant)
            .execute(&mut *tx)
            .await?
            .rows_affected()
            != 1
        {
            return Err(Error::Blocked("inventory_version_changed"));
        }
        sqlx::query("INSERT INTO inventory_transactions(id,tenant_id,inventory_level_id,type,quantity_change) VALUES($1,$2,$3,'adjustment',$4)").bind(uuid::Uuid::new_v4().to_string()).bind(tenant).bind(id).bind(delta).execute(&mut *tx).await?;
    }
    // Retain the historical cross-route marker, but never call it a receipt.
    sqlx::query("INSERT INTO applied_client_mutations(client_mutation_id,tenant_id) VALUES($1,$2)")
        .bind(&m.id)
        .bind(tenant)
        .execute(&mut *tx)
        .await?;
    let after: Value = sqlx::query_scalar(PG_PRODUCT)
        .bind(&m.payload.item_id)
        .bind(tenant)
        .fetch_one(&mut *tx)
        .await?;
    let after_levels = sqlx::query_as::<_, Level>(PG_LEVELS)
        .bind(&m.payload.item_id)
        .bind(tenant)
        .fetch_all(&mut *tx)
        .await?;
    let receipt = make_receipt(
        m,
        version(tenant, &m.payload.item_id, &after, &after_levels),
        planned.stock,
    );
    let encoded = serde_json::to_string(&receipt).map_err(|_| Error::Blocked("invalid_receipt"))?;
    sqlx::query(INSERT_RECEIPT)
        .bind(tenant)
        .bind(&m.id)
        .bind(&m.payload.item_id)
        .bind(identity)
        .bind(encoded)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(receipt)
}

pub async fn read_postgres(pool: &PgPool, tenant: &str) -> Result<Vec<Value>, Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    server_common::auth_utils::set_org_context(&mut *tx, tenant).await?;
    let products: Vec<Value> =
        sqlx::query_scalar("SELECT to_jsonb(p) FROM products p WHERE tenant_id=$1 ORDER BY id")
            .bind(tenant)
            .fetch_all(&mut *tx)
            .await?;
    let mut result = Vec::with_capacity(products.len());
    for product in products {
        let id = product["id"]
            .as_str()
            .ok_or(Error::Blocked("invalid_stored_inventory"))?;
        let levels = sqlx::query_as::<_, Level>("SELECT id,available_count,committed_count FROM inventory_levels WHERE variant_id=$1 AND tenant_id=$2 ORDER BY id").bind(id).bind(tenant).fetch_all(&mut *tx).await?;
        result.push(json!({"id":id,"name":product["title"].as_str().or_else(||product["name"].as_str()).unwrap_or(""),"description":product["description"],"price_cents":product["price_cents"],"currency":product["currency"],"stock":available(&product,&levels)?,"inventory_version":version(tenant,id,&product,&levels),"is_sold_out":product["is_sold_out"],"updated_at":product["updated_at"],"is_subscribable":product["is_subscribable"].as_bool().unwrap_or(false),"subscription_discount_percent":product["subscription_discount_percent"].as_i64().unwrap_or(0),"subscription_frequency":product["subscription_frequency"].as_str().unwrap_or("")}));
    }
    tx.commit().await?;
    Ok(result)
}

/// A lookup never dispatches or reconstructs an adjustment. A missing receipt
/// leaves the original result unknown, including while another request commits.
pub async fn receipt_postgres(
    pool: &PgPool,
    tenant: &str,
    id: &str,
) -> Result<Option<Receipt>, Error> {
    if !valid_id(id) {
        return Err(Error::Blocked("invalid_adjustment_identity"));
    }
    let mut tx = pool.begin().await?;
    server_common::auth_utils::set_org_context(&mut *tx, tenant).await?;
    let raw: Option<String> = sqlx::query_scalar("SELECT receipt_json FROM inventory_adjustment_receipts WHERE tenant_id=$1 AND client_mutation_id=$2")
        .bind(tenant).bind(id).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    raw.map(|value| {
        serde_json::from_str(&value).map_err(|_| Error::Unconfirmed("invalid_saved_receipt"))
    })
    .transpose()
}

fn sqlite_product_json(row: &sqlx::sqlite::SqliteRow) -> Result<Value, Error> {
    use sqlx::Row;
    Ok(json!({
        "id": row.try_get::<String, _>("id")?,
        "title": row.try_get::<Option<String>, _>("title")?,
        "name": row.try_get::<Option<String>, _>("name")?,
        "description": row.try_get::<Option<String>, _>("description")?,
        "price_cents": row.try_get::<i64, _>("price_cents")?,
        "currency": row.try_get::<Option<String>, _>("currency")?,
        "inventory_count": row.try_get::<i64, _>("inventory_count")?,
        "locked_quantity": row.try_get::<i64, _>("locked_quantity")?,
        "available_quantity": row.try_get::<i64, _>("available_quantity")?,
        "is_sold_out": row.try_get::<bool, _>("is_sold_out")?,
        "is_subscribable": row.try_get::<bool, _>("is_subscribable")?,
        "subscription_discount_percent": row.try_get::<i64, _>("subscription_discount_percent")?,
        "subscription_frequency": row.try_get::<Option<String>, _>("subscription_frequency")?,
        "updated_at": row.try_get::<Option<String>, _>("updated_at")?,
    }))
}

pub async fn apply_sqlite(
    pool: &sqlx::SqlitePool,
    tenant: &str,
    m: &Mutation,
) -> Result<Receipt, Error> {
    if !m.valid() {
        return Err(Error::Blocked("invalid_adjustment"));
    }
    let mut tx = pool.begin().await?;
    let identity = serde_json::to_string(m).map_err(|_| Error::Blocked("invalid_adjustment"))?;
    let saved: Option<(String, String)> = sqlx::query_as(
        "SELECT request_identity, receipt_json FROM inventory_adjustment_receipts WHERE tenant_id = ? AND client_mutation_id = ?",
    )
    .bind(tenant)
    .bind(&m.id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(saved) = saved {
        let result = replay(saved, &identity)?;
        tx.commit().await?;
        return Ok(result);
    }
    let product_row = sqlx::query(
        "SELECT id, title, name, description, price_cents, currency, inventory_count, locked_quantity, available_quantity, is_sold_out, is_subscribable, subscription_discount_percent, subscription_frequency, CAST(updated_at AS TEXT) AS updated_at FROM products WHERE id = ? AND tenant_id = ?",
    )
    .bind(&m.payload.item_id)
    .bind(tenant)
    .fetch_optional(&mut *tx)
    .await?;
    let product = product_row
        .as_ref()
        .map(sqlite_product_json)
        .transpose()?
        .ok_or(Error::Blocked("product_not_found"))?;
    let legacy: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM applied_client_mutations WHERE tenant_id = ? AND client_mutation_id = ?)",
    )
    .bind(tenant)
    .bind(&m.id)
    .fetch_one(&mut *tx)
    .await?;
    if legacy {
        return Err(Error::Unconfirmed("legacy_adjustment_requires_review"));
    }
    let levels = sqlx::query_as::<_, Level>(
        "SELECT id, available_count, committed_count FROM inventory_levels WHERE variant_id = ? AND tenant_id = ? ORDER BY id",
    )
    .bind(&m.payload.item_id)
    .bind(tenant)
    .fetch_all(&mut *tx)
    .await?;
    if version(tenant, &m.payload.item_id, &product, &levels) != m.payload.expected_version {
        return Err(Error::Blocked("inventory_version_changed"));
    }
    let planned = plan(&product, &levels, m.payload.quantity_change)?;
    let now = chrono::Utc::now().to_rfc3339();
    let changed = sqlx::query(
        "UPDATE products SET inventory_count = ?, available_quantity = ?, is_sold_out = COALESCE(?, is_sold_out), updated_at = ? WHERE id = ? AND tenant_id = ? AND inventory_count = ?",
    )
    .bind(planned.total)
    .bind(planned.available)
    .bind(m.payload.is_sold_out)
    .bind(&now)
    .bind(&m.payload.item_id)
    .bind(tenant)
    .bind(integer(&product, "inventory_count")?)
    .execute(&mut *tx)
    .await?;
    if changed.rows_affected() != 1 {
        return Err(Error::Blocked("inventory_version_changed"));
    }
    for (id, delta) in &planned.level_changes {
        if sqlx::query(
            "UPDATE inventory_levels SET available_count = available_count + ?, updated_at = ? WHERE id = ? AND tenant_id = ? AND available_count + ? >= 0 AND available_count + ? <= 2147483647",
        )
        .bind(delta)
        .bind(&now)
        .bind(id)
        .bind(tenant)
        .bind(delta)
        .bind(delta)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            != 1
        {
            return Err(Error::Blocked("inventory_version_changed"));
        }
        sqlx::query(
            "INSERT INTO inventory_transactions (id, tenant_id, inventory_level_id, type, quantity_change) VALUES (?, ?, ?, 'adjustment', ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(tenant)
        .bind(id)
        .bind(delta)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO applied_client_mutations (client_mutation_id, tenant_id) VALUES (?, ?)",
    )
    .bind(&m.id)
    .bind(tenant)
    .execute(&mut *tx)
    .await?;
    let after_row = sqlx::query(
        "SELECT id, title, name, description, price_cents, currency, inventory_count, locked_quantity, available_quantity, is_sold_out, is_subscribable, subscription_discount_percent, subscription_frequency, CAST(updated_at AS TEXT) AS updated_at FROM products WHERE id = ? AND tenant_id = ?",
    )
    .bind(&m.payload.item_id)
    .bind(tenant)
    .fetch_one(&mut *tx)
    .await?;
    let after = sqlite_product_json(&after_row)?;
    let after_levels = sqlx::query_as::<_, Level>(
        "SELECT id, available_count, committed_count FROM inventory_levels WHERE variant_id = ? AND tenant_id = ? ORDER BY id",
    )
    .bind(&m.payload.item_id)
    .bind(tenant)
    .fetch_all(&mut *tx)
    .await?;
    let receipt = make_receipt(
        m,
        version(tenant, &m.payload.item_id, &after, &after_levels),
        planned.stock,
    );
    let encoded = serde_json::to_string(&receipt).map_err(|_| Error::Blocked("invalid_receipt"))?;
    sqlx::query(
        "INSERT INTO inventory_adjustment_receipts (tenant_id, client_mutation_id, item_id, request_identity, receipt_json) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(tenant)
    .bind(&m.id)
    .bind(&m.payload.item_id)
    .bind(identity)
    .bind(encoded)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(receipt)
}

pub async fn read_sqlite(pool: &sqlx::SqlitePool, tenant: &str) -> Result<Vec<Value>, Error> {
    let rows = sqlx::query(
        "SELECT id, title, name, description, price_cents, currency, inventory_count, locked_quantity, available_quantity, is_sold_out, is_subscribable, subscription_discount_percent, subscription_frequency, CAST(updated_at AS TEXT) AS updated_at FROM products WHERE tenant_id = ? ORDER BY id",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await?;
    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let product = sqlite_product_json(&row)?;
        let id = product["id"]
            .as_str()
            .ok_or(Error::Blocked("invalid_stored_inventory"))?;
        let levels = sqlx::query_as::<_, Level>(
            "SELECT id, available_count, committed_count FROM inventory_levels WHERE variant_id = ? AND tenant_id = ? ORDER BY id",
        )
        .bind(id)
        .bind(tenant)
        .fetch_all(pool)
        .await?;
        result.push(json!({"id":id,"name":product["title"].as_str().or_else(||product["name"].as_str()).unwrap_or(""),"description":product["description"],"price_cents":product["price_cents"],"currency":product["currency"],"stock":available(&product,&levels)?,"inventory_version":version(tenant,id,&product,&levels),"is_sold_out":product["is_sold_out"],"updated_at":product["updated_at"],"is_subscribable":product["is_subscribable"].as_bool().unwrap_or(false),"subscription_discount_percent":product["subscription_discount_percent"].as_i64().unwrap_or(0),"subscription_frequency":product["subscription_frequency"].as_str().unwrap_or("")}));
    }
    Ok(result)
}

pub async fn receipt_sqlite(
    pool: &sqlx::SqlitePool,
    tenant: &str,
    id: &str,
) -> Result<Option<Receipt>, Error> {
    if !valid_id(id) {
        return Err(Error::Blocked("invalid_adjustment_identity"));
    }
    let raw: Option<String> = sqlx::query_scalar(
        "SELECT receipt_json FROM inventory_adjustment_receipts WHERE tenant_id = ? AND client_mutation_id = ?",
    )
    .bind(tenant)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    raw.map(|value| {
        serde_json::from_str(&value).map_err(|_| Error::Unconfirmed("invalid_saved_receipt"))
    })
    .transpose()
}

#[cfg(test)]
#[path = "pos_inventory_test.rs"]
mod tests;
