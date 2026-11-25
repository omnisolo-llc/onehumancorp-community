//! Solo-owner merchant Stripe webhooks (OSS single-tenant parity).
//! Uses per-workspace encrypted webhook signing secrets in `ohc_provider_connections`,
//! durable SQLite/PostgreSQL `processed_webhook_events` deduplication (zero Redis),
//! and transactional settlement for proposal deposits, invoices, orders, and subscriptions.

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use serde_json::Value;
use sqlx::{PgConnection, SqliteConnection};
use std::{collections::BTreeMap, sync::Arc};

use crate::api::billing_webhook::{
    CheckoutKind, PostCommitAction, ProcessOutcome, StripeEvent, classify_checkout_kind,
    payment_failure_lookup, valid_event_id,
};
use crate::db::{DB, DbStore};
use crate::integrations::stripe::webhook_signature::{self, SignatureError};
use ::ohc_core::extensions::TenantContextProvider;
use ::server_harness::middleware::{connection_vault::ConnectionVault, usage_ledger::UsageLedger};

pub const MERCHANT_STRIPE_WEBHOOK_PROVIDER: &str = "stripe_webhook";

#[derive(Clone)]
pub struct MerchantWebhookState {
    pub db: Arc<DB>,
}

pub fn router<S: Clone + Send + Sync + 'static>(state: MerchantWebhookState) -> Router<S> {
    Router::new()
        .route(
            "/api/v1/webhooks/merchant/stripe/{tenant_id}",
            post(merchant_stripe_webhook_handler),
        )
        .with_state(state)
}

fn meta_str<'a>(obj: &'a Value, key: &str) -> Option<&'a str> {
    obj.get("metadata")?
        .get(key)?
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn str_field<'a>(obj: &'a Value, key: &str) -> Option<&'a str> {
    obj.get(key)?
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn valid_tenant(tenant_id: &str) -> Option<&str> {
    let t = tenant_id.trim();
    (!t.is_empty() && t.len() <= 255 && !t.chars().any(char::is_control)).then_some(t)
}

fn svc_err<E>(_: E) -> StatusCode {
    StatusCode::SERVICE_UNAVAILABLE
}

fn vault_keys_from_env() -> Result<(BTreeMap<String, Vec<u8>>, String), StatusCode> {
    let encoded = std::env::var("OMNISOLO_CONNECTION_KEYS").map_err(svc_err)?;
    let raw: BTreeMap<String, String> = serde_json::from_str(&encoded).map_err(svc_err)?;
    let active = std::env::var("OMNISOLO_CONNECTION_ACTIVE_KEY").map_err(svc_err)?;
    let mut keys = BTreeMap::new();
    for (id, hex_val) in raw {
        let bytes = hex::decode(hex_val).map_err(svc_err)?;
        if bytes.len() != 32 {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        keys.insert(id, bytes);
    }
    keys.contains_key(&active)
        .then_some((keys, active))
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

fn usage_ledger_for_db(db: &DB) -> UsageLedger {
    match &db.store {
        DbStore::Sqlite(pool) => UsageLedger::Sqlite(pool.clone()),
        DbStore::Postgres => UsageLedger::Postgres(db.pool.clone()),
    }
}

/// Encrypts and persists a per-workspace Stripe webhook signing secret (`whsec_*`)
/// in `ohc_provider_connections` using the `ConnectionVault` AES-256-GCM envelope.
pub async fn store_merchant_webhook_secret(
    db: &DB,
    tenant_id: &str,
    secret: &str,
) -> Result<(), String> {
    let tenant = valid_tenant(tenant_id).ok_or("Invalid tenant_id")?;
    let trimmed = secret.trim();
    if !trimmed.starts_with("whsec_")
        || !(12..=4096).contains(&trimmed.len())
        || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err("A valid Stripe webhook signing secret (whsec_*) is required".into());
    }
    let (keys, active) = vault_keys_from_env().map_err(|_| "Connection vault unconfigured")?;
    ConnectionVault::initialize_schema(&usage_ledger_for_db(db))
        .await
        .map_err(|e| e.to_string())?;
    let rev = uuid::Uuid::new_v4().to_string();
    let aad = serde_json::to_vec(&(tenant, MERCHANT_STRIPE_WEBHOOK_PROVIDER, &rev, &active))
        .map_err(|_| "AAD encoding failed")?;
    let cipher = Aes256Gcm::new_from_slice(keys[&active].as_slice()).map_err(|_| "Invalid key")?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let payload = Payload {
        msg: trimmed.as_bytes(),
        aad: &aad,
    };
    let ct = cipher
        .encrypt(&nonce, payload)
        .map_err(|_| "Encryption failed")?;
    let (nonce_hex, ct_hex) = (hex::encode(nonce), hex::encode(ct));
    let now = chrono::Utc::now().timestamp();
    const UPSERT: &str = "INSERT INTO ohc_provider_connections \
         (tenant_id, provider, revision, state, key_id, nonce_hex, ciphertext_hex, verified_at) \
         VALUES ($1, $2, $3, 'verified', $4, $5, $6, $7) \
         ON CONFLICT (tenant_id, provider) DO UPDATE SET \
         revision = excluded.revision, state = 'verified', key_id = excluded.key_id, \
         nonce_hex = excluded.nonce_hex, ciphertext_hex = excluded.ciphertext_hex, \
         verified_at = excluded.verified_at";
    macro_rules! run_upsert {
        ($exec:expr) => {
            sqlx::query(UPSERT)
                .bind(tenant)
                .bind(MERCHANT_STRIPE_WEBHOOK_PROVIDER)
                .bind(&rev)
                .bind(&active)
                .bind(&nonce_hex)
                .bind(&ct_hex)
                .bind(now)
                .execute($exec)
                .await
                .map_err(|e| e.to_string())?
        };
    }
    match &db.store {
        DbStore::Sqlite(pool) => {
            run_upsert!(pool);
        }
        DbStore::Postgres => {
            let mut tx = db.pool.begin().await.map_err(|e| e.to_string())?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant)
                .await
                .map_err(|e| e.to_string())?;
            run_upsert!(&mut *tx);
            tx.commit().await.map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Resolves the workspace's Stripe webhook signing secret from `ConnectionVault`
/// (`ohc_provider_connections`), failing closed when unconfigured or revoked.
pub async fn resolve_merchant_webhook_secret(
    db: &DB,
    tenant_id: &str,
) -> Result<String, StatusCode> {
    let tenant = valid_tenant(tenant_id).ok_or(StatusCode::BAD_REQUEST)?;
    if std::env::var_os("OMNISOLO_CONNECTION_KEYS").is_some() {
        let (keys, _) = vault_keys_from_env()?;
        const SELECT: &str = "SELECT revision, key_id, nonce_hex, ciphertext_hex \
             FROM ohc_provider_connections WHERE tenant_id = $1 AND provider = $2 AND state = 'verified'";
        let row: Option<(String, String, String, String)> = match &db.store {
            DbStore::Sqlite(pool) => sqlx::query_as(SELECT)
                .bind(tenant)
                .bind(MERCHANT_STRIPE_WEBHOOK_PROVIDER)
                .fetch_optional(pool)
                .await
                .map_err(svc_err)?,
            DbStore::Postgres => {
                let mut tx = db.pool.begin().await.map_err(svc_err)?;
                ::server_common::auth_utils::set_org_context(&mut *tx, tenant)
                    .await
                    .map_err(svc_err)?;
                let rec = sqlx::query_as(SELECT)
                    .bind(tenant)
                    .bind(MERCHANT_STRIPE_WEBHOOK_PROVIDER)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(svc_err)?;
                let _ = tx.commit().await;
                rec
            }
        };
        let (rev, key_id, nonce_hex, ct_hex) = row.ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let key = keys.get(&key_id).ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let nonce_bytes = hex::decode(&nonce_hex).map_err(svc_err)?;
        if nonce_bytes.len() != 12 {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        let ct_bytes = hex::decode(&ct_hex).map_err(svc_err)?;
        let aad = serde_json::to_vec(&(tenant, MERCHANT_STRIPE_WEBHOOK_PROVIDER, &rev, &key_id))
            .map_err(svc_err)?;
        let cipher = Aes256Gcm::new_from_slice(key.as_slice()).map_err(svc_err)?;
        let payload = Payload {
            msg: &ct_bytes,
            aad: &aad,
        };
        let plain = cipher
            .decrypt(Nonce::from_slice(&nonce_bytes), payload)
            .map_err(svc_err)?;
        return String::from_utf8(plain).map_err(svc_err);
    }
    if crate::is_standalone_runtime() {
        let bytes = ::server_common::secret_source::load_optional_secret(
            "STRIPE_MERCHANT_WEBHOOK_SECRET",
            "STRIPE_MERCHANT_WEBHOOK_SECRET_FILE",
        )
        .map_err(svc_err)?
        .or_else(|| {
            ::server_common::secret_source::load_optional_secret(
                "STRIPE_WEBHOOK_SECRET",
                "STRIPE_WEBHOOK_SECRET_FILE",
            )
            .ok()
            .flatten()
        })
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        return String::from_utf8(bytes).map_err(svc_err);
    }
    Err(StatusCode::SERVICE_UNAVAILABLE)
}

fn extract_stripe_signature(headers: &HeaderMap) -> Result<&str, StatusCode> {
    let mut values = headers.get_all("Stripe-Signature").iter();
    let value = values.next().ok_or(StatusCode::UNAUTHORIZED)?;
    if values.next().is_some() {
        return Err(StatusCode::UNAUTHORIZED);
    }
    value.to_str().map_err(|_| StatusCode::UNAUTHORIZED)
}

pub async fn merchant_stripe_webhook_handler(
    State(state): State<MerchantWebhookState>,
    Path(tenant_id): Path<String>,
    req: Request<Body>,
) -> impl IntoResponse {
    let signature = match extract_stripe_signature(req.headers()) {
        Ok(sig) => sig.to_owned(),
        Err(status) => return status.into_response(),
    };
    let secret = match resolve_merchant_webhook_secret(&state.db, &tenant_id).await {
        Ok(secret) => secret,
        Err(status) => return status.into_response(),
    };
    let bytes = match to_bytes(req.into_body(), webhook_signature::MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    if let Err(err) = webhook_signature::verify_now(&bytes, &signature, secret.as_bytes()) {
        return match err {
            SignatureError::MissingSecret => StatusCode::SERVICE_UNAVAILABLE,
            SignatureError::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            _ => StatusCode::UNAUTHORIZED,
        }
        .into_response();
    }
    let event: StripeEvent = match serde_json::from_slice(&bytes) {
        Ok(event) => event,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if !valid_event_id(&event.id) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if meta_str(&event.data.object, "tenant_id").is_some_and(|t| t != tenant_id) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match process_merchant_stripe_event(&state.db, &event, Some(&tenant_id), None).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(status) => status.into_response(),
    }
}

/// Returns true for solo-owner merchant Stripe settlement events (proposal deposits,
/// customer invoices, storefront orders, and merchant product subscriptions).
pub fn is_merchant_stripe_event(event: &StripeEvent) -> bool {
    let obj = &event.data.object;
    match event.r#type.as_str() {
        "checkout.session.completed" => matches!(
            classify_checkout_kind(obj),
            CheckoutKind::ProposalDeposit
                | CheckoutKind::InvoicePayment
                | CheckoutKind::MerchantOrder
        ),
        "invoice.payment_succeeded" | "invoice.paid" | "invoice.payment_failed" => true,
        "customer.subscription.deleted" => meta_str(obj, "product_id").is_some(),
        _ => false,
    }
}

fn invoice_statuses(total_cents: i64, paid_cents: i64) -> (&'static str, &'static str) {
    if total_cents > 0 && paid_cents >= total_cents {
        ("paid", "paid")
    } else if paid_cents > 0 {
        ("open", "partially_paid")
    } else {
        ("open", "unpaid")
    }
}

fn proposal_next_status(amount: i64, total_c: i64, dep_c: i64) -> &'static str {
    if amount >= total_c && total_c > 0 {
        "PAID"
    } else if dep_c > 0 && amount >= dep_c {
        "DEPOSIT_PAID"
    } else {
        "PARTIALLY_PAID"
    }
}

fn resolve_invoice_cents(tot_opt: Option<i32>, tot_f: f64) -> i64 {
    tot_opt
        .map(i64::from)
        .unwrap_or_else(|| (tot_f * 100.0).round() as i64)
}

fn push_dept(acts: &mut Vec<PostCommitAction>, tid: &str, ev: &str, payload: Value) {
    acts.push(PostCommitAction::DispatchDepartmentEvent {
        tenant_id: tid.to_string(),
        event_type: ev.to_string(),
        payload,
    });
}

async fn ensure_sqlite_tables(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    for ddl in [
        "CREATE TABLE IF NOT EXISTS processed_webhook_events (\
         event_id TEXT PRIMARY KEY, provider TEXT NOT NULL DEFAULT 'stripe', \
         event_type TEXT NOT NULL, tenant_id TEXT, status TEXT NOT NULL DEFAULT 'processed', \
         processed_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP)",
        "CREATE TABLE IF NOT EXISTS stripe_payment_settlements (\
         tenant_id TEXT NOT NULL, settlement_key TEXT NOT NULL, event_id TEXT NOT NULL, \
         kind TEXT NOT NULL, amount_cents INTEGER NOT NULL, currency TEXT NOT NULL, \
         created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, PRIMARY KEY (tenant_id, settlement_key))",
    ] {
        sqlx::query(ddl).execute(&mut *conn).await?;
    }
    Ok(())
}

async fn ensure_pg_dedup_table(conn: &mut PgConnection) {
    let _ = sqlx::query(
        "CREATE TABLE IF NOT EXISTS processed_webhook_events (\
         event_id TEXT PRIMARY KEY, provider TEXT NOT NULL DEFAULT 'stripe', \
         event_type TEXT NOT NULL, tenant_id TEXT, status TEXT NOT NULL DEFAULT 'processed', \
         processed_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP)",
    )
    .execute(&mut *conn)
    .await;
}

macro_rules! q_exec {
    ($tx:expr, $sql:expr $(, $bind:expr)*) => {{
        sqlx::query($sql)$(.bind($bind))*.execute(&mut *$tx).await.map_err(svc_err)?
    }};
}

macro_rules! q_opt {
    ($tx:expr, $ty:ty, $sql:expr $(, $bind:expr)*) => {{
        sqlx::query_as::<_, $ty>($sql)$(.bind($bind))*.fetch_optional(&mut *$tx).await.map_err(svc_err)?
    }};
}

macro_rules! q_all {
    ($tx:expr, $ty:ty, $sql:expr $(, $bind:expr)*) => {{
        sqlx::query_as::<_, $ty>($sql)$(.bind($bind))*.fetch_all(&mut *$tx).await.map_err(svc_err)?
    }};
}

macro_rules! fence_settlement {
    ($tx:expr, $tenant:expr, $key:expr, $eid:expr, $kind:expr, $cents:expr, $cur:expr) => {{
        q_exec!(
            $tx,
            "INSERT INTO stripe_payment_settlements (tenant_id, settlement_key, event_id, kind, amount_cents, currency) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
            $tenant, $key, $eid, $kind, $cents, $cur
        )
        .rows_affected()
            == 1
    }};
}

macro_rules! settle_merchant_tx {
    ($tx:expr, $event:expr, $scoped_tenant:expr) => {{
        let obj = &$event.data.object;
        let hint_tenant = $scoped_tenant
            .map(ToString::to_string)
            .or_else(|| meta_str(obj, "tenant_id").map(ToString::to_string))
            .or_else(|| str_field(obj, "client_reference_id").map(ToString::to_string));
        let claimed = q_exec!(
            $tx,
            "INSERT INTO processed_webhook_events (event_id, provider, event_type, tenant_id, status) \
             VALUES ($1, 'stripe', $2, $3, 'processed') ON CONFLICT (event_id) DO NOTHING",
            &$event.id, &$event.r#type, hint_tenant.as_deref()
        )
        .rows_affected();
        if claimed == 0 {
            let _ = $tx.rollback().await;
            return Ok((ProcessOutcome::Duplicate, Vec::new()));
        }

        let mut actions: Vec<PostCommitAction> = Vec::new();
        match $event.r#type.as_str() {
            "checkout.session.completed" => match classify_checkout_kind(obj) {
                CheckoutKind::ProposalDeposit | CheckoutKind::InvoicePayment => {
                    let sid = str_field(obj, "id").unwrap_or("");
                    let amount = obj.get("amount_total").and_then(Value::as_i64).unwrap_or(0);
                    let cur = str_field(obj, "currency").unwrap_or("usd");
                    let mut tid = $scoped_tenant.map(ToString::to_string)
                        .or_else(|| meta_str(obj, "tenant_id").map(ToString::to_string));
                    let mut pid = meta_str(obj, "proposal_id").map(ToString::to_string);
                    let mut iid = meta_str(obj, "invoice_id").map(ToString::to_string);
                    if (pid.is_none() || tid.is_none()) && !sid.is_empty()
                        && let Some((p, t)) = q_opt!($tx, (String, String),
                            "SELECT id, tenant_id FROM proposals WHERE stripe_checkout_session_id = $1 LIMIT 1", sid)
                    {
                        pid.get_or_insert(p);
                        tid.get_or_insert(t);
                    }
                    if (iid.is_none() || tid.is_none()) && !sid.is_empty()
                        && let Some((i, t)) = q_opt!($tx, (String, String),
                            "SELECT id, tenant_id FROM invoices WHERE stripe_checkout_session_id = $1 LIMIT 1", sid)
                    {
                        iid.get_or_insert(i);
                        tid.get_or_insert(t);
                    }
                    if let Some(tid) = tid {
                        let key = if !sid.is_empty() { format!("checkout_session:{sid}") }
                            else { format!("checkout_event:{}", $event.id) };
                        if fence_settlement!($tx, &tid, &key, &$event.id, "payment_credit", amount, cur) {
                            if let Some(p) = pid.as_deref()
                                && let Some((total_c, dep_c)) = q_opt!($tx, (i64, i64),
                                    "SELECT total_amount_cents, required_deposit_cents FROM proposals WHERE id = $1 AND tenant_id = $2", p, &tid)
                            {
                                let next = proposal_next_status(amount, total_c, dep_c);
                                q_exec!($tx,
                                    "UPDATE proposals SET status = $1, stripe_checkout_session_id = COALESCE(NULLIF($2, ''), stripe_checkout_session_id), \
                                     updated_at = CURRENT_TIMESTAMP WHERE id = $3 AND tenant_id = $4",
                                    next, sid, p, &tid);
                            }
                            let inv = if let Some(i) = iid.as_deref() {
                                q_opt!($tx, (String, Option<i32>, Option<i32>, f64),
                                    "SELECT id, total_amount_cents, amount_paid_cents, total_amount FROM invoices WHERE id = $1 AND tenant_id = $2", i, &tid)
                            } else if !sid.is_empty() || pid.is_some() {
                                q_opt!($tx, (String, Option<i32>, Option<i32>, f64),
                                    "SELECT id, total_amount_cents, amount_paid_cents, total_amount FROM invoices WHERE tenant_id = $1 \
                                     AND (($2 <> '' AND stripe_checkout_session_id = $2) OR ($3 IS NOT NULL AND proposal_id = $3)) LIMIT 1",
                                    &tid, sid, pid.as_deref())
                            } else {
                                None
                            };
                            if let Some((i, tot_opt, paid_opt, tot_f)) = inv {
                                let tot_c = resolve_invoice_cents(tot_opt, tot_f);
                                let new_paid = i64::from(paid_opt.unwrap_or(0)).saturating_add(amount);
                                let (st, pst) = invoice_statuses(tot_c, new_paid);
                                q_exec!($tx,
                                    "UPDATE invoices SET amount_paid_cents = $1, payment_status = $2, status = $3, \
                                     stripe_checkout_session_id = COALESCE(NULLIF($4, ''), stripe_checkout_session_id), \
                                     updated_at = CURRENT_TIMESTAMP WHERE id = $5 AND tenant_id = $6",
                                    i32::try_from(new_paid).unwrap_or(i32::MAX), pst, st, sid, &i, &tid);
                                if st == "paid" {
                                    actions.push(PostCommitAction::ResolveInvoiceTriage { invoice_id: i });
                                }
                            }
                            push_dept(&mut actions, &tid, "payment.captured", obj.clone());
                        }
                    }
                }
                CheckoutKind::MerchantOrder => {
                    let tid_opt = $scoped_tenant.or_else(|| meta_str(obj, "tenant_id"));
                    if let (Some(tid), Some(oid)) = (tid_opt, meta_str(obj, "order_id")) {
                        q_exec!($tx, "UPDATE orders SET status = 'Paid' WHERE id = $1 AND tenant_id = $2", oid, tid);
                    }
                    if let (Some(tid), Some(prod_id)) = (tid_opt, meta_str(obj, "product_id")) {
                        let qty = meta_str(obj, "quantity").and_then(|q| q.parse::<i32>().ok()).unwrap_or(1);
                        actions.push(PostCommitAction::CommitInventory {
                            tenant_id: tid.to_string(),
                            product_id: prod_id.to_string(),
                            quantity: qty,
                            lock_id: meta_str(obj, "inventory_lock_id").unwrap_or("").to_string(),
                        });
                        let cid = str_field(obj, "customer").or_else(|| str_field(obj, "client_reference_id"));
                        if let Some(cid) = cid && str_field(obj, "mode") == Some("subscription") {
                            let plan_id = meta_str(obj, "subscription_plan_id")
                                .map(ToString::to_string).unwrap_or_else(|| format!("plan-{prod_id}"));
                            let sub_id = uuid::Uuid::new_v4().to_string();
                            let stripe_sub = str_field(obj, "subscription").unwrap_or("");
                            let amt = obj.get("amount_total").and_then(Value::as_i64).unwrap_or(0);
                            let period_end = chrono::Utc::now() + chrono::Duration::days(30);
                            let _ = sqlx::query(
                                "INSERT INTO subscription_plans (id, tenant_id, name, price_cents, currency, frequency) \
                                 VALUES ($1, $2, $3, $4, 'USD', 'monthly') ON CONFLICT (id) DO NOTHING",
                            )
                            .bind(&plan_id).bind(tid).bind(prod_id).bind(i32::try_from(amt).unwrap_or(i32::MAX))
                            .execute(&mut *$tx).await;
                            q_exec!($tx, "INSERT INTO subscriptions (id, tenant_id, customer_id, plan_id, status, current_period_end) VALUES ($1, $2, $3, $4, 'active', $5)",
                                &sub_id, tid, cid, &plan_id, period_end);
                            q_exec!($tx, "INSERT INTO subscribers (id, tenant_id, subscription_plan_id, customer_id, stripe_subscription_id, status) VALUES ($1, $2, $3, $4, $5, 'ACTIVE')",
                                &sub_id, tid, &plan_id, cid, stripe_sub);
                            let oid = uuid::Uuid::new_v4().to_string();
                            q_exec!($tx, "INSERT INTO orders (id, tenant_id, customer_id, total_amount, status) VALUES ($1, $2, $3, $4, 'paid')",
                                &oid, tid, cid, amt as f64 / 100.0);
                            push_dept(&mut actions, tid, "tenant.order.created",
                                serde_json::json!({ "order_id": oid, "customer_id": cid, "subscription_id": sub_id }));
                        }
                    } else {
                        actions.push(PostCommitAction::ReleaseInventoryLocks(obj.clone()));
                    }
                    if let Some(tid) = tid_opt {
                        push_dept(&mut actions, tid, "payment.captured", obj.clone());
                    }
                }
                _ => {}
            },
            "invoice.payment_succeeded" | "invoice.paid" => {
                if let Some(inv_ref) = str_field(obj, "id") {
                    let amt = obj.get("amount_paid").and_then(Value::as_i64);
                    let cur = str_field(obj, "currency").unwrap_or("usd");
                    if let Some((id, tid, tot_opt, paid_opt, tot_f)) = q_opt!(
                        $tx, (String, String, Option<i32>, Option<i32>, f64),
                        "SELECT id, tenant_id, total_amount_cents, amount_paid_cents, total_amount FROM invoices \
                         WHERE (stripe_invoice_id = $1 OR id = $1) AND ($2 IS NULL OR tenant_id = $2) LIMIT 1",
                        inv_ref, $scoped_tenant
                    ) {
                        let tot_c = resolve_invoice_cents(tot_opt, tot_f);
                        let new_paid = i64::from(paid_opt.unwrap_or(0)).max(amt.unwrap_or(tot_c).max(0));
                        let (st, pst) = invoice_statuses(tot_c, new_paid);
                        if fence_settlement!($tx, &tid, format!("invoice_paid:{inv_ref}:{new_paid}"), &$event.id, "payment_credit", new_paid, cur) {
                            q_exec!($tx,
                                "UPDATE invoices SET amount_paid_cents = $1, payment_status = $2, status = $3, \
                                 updated_at = CURRENT_TIMESTAMP WHERE id = $4 AND tenant_id = $5",
                                i32::try_from(new_paid).unwrap_or(i32::MAX), pst, st, &id, &tid);
                            if st == "paid" {
                                actions.push(PostCommitAction::ResolveInvoiceTriage { invoice_id: id });
                            }
                        }
                    }
                }
            }
            "invoice.payment_failed" => {
                let lookup = payment_failure_lookup(obj);
                let sub = lookup.stripe_subscription_id.as_deref().unwrap_or("");
                let cus = lookup.customer_id.as_deref().unwrap_or("");
                if !sub.is_empty() || !cus.is_empty() {
                    let rows = q_all!(
                        $tx, (String, String),
                        "SELECT id, tenant_id FROM subscribers \
                         WHERE (($1 <> '' AND stripe_subscription_id = $1 AND ($2 = '' OR customer_id = $2)) \
                            OR ($1 = '' AND $2 <> '' AND customer_id = $2)) AND ($3 IS NULL OR tenant_id = $3) LIMIT 2",
                        sub, cus, $scoped_tenant
                    );
                    if rows.len() > 1 {
                        return Err(StatusCode::INTERNAL_SERVER_ERROR);
                    }
                    if let Some((subscriber_id, tenant_id)) = rows.into_iter().next() {
                        q_exec!($tx, "UPDATE subscribers SET status = 'PAST_DUE' WHERE id = $1 AND tenant_id = $2", &subscriber_id, &tenant_id);
                        actions.push(PostCommitAction::SendFailedPaymentDunning {
                            tenant_id,
                            subscriber_id,
                            business_name: meta_str(obj, "business_name").unwrap_or("Your business").to_string(),
                            event_id: format!("stripe:{}", $event.id),
                        });
                    }
                }
            }
            "customer.subscription.deleted" => {
                if let Some(sub_id) = str_field(obj, "id") {
                    let _ = sqlx::query(
                        "UPDATE subscribers SET status = 'CANCELED', updated_at = CURRENT_TIMESTAMP \
                         WHERE stripe_subscription_id = $1 AND ($2 IS NULL OR tenant_id = $2)",
                    )
                    .bind(sub_id).bind($scoped_tenant).execute(&mut *$tx).await;
                }
            }
            _ => {}
        }

        $tx.commit().await.map_err(svc_err)?;
        (ProcessOutcome::Processed, actions)
    }};
}

/// Processes a verified solo-owner merchant Stripe event atomically against SQLite
/// or PostgreSQL using `processed_webhook_events` primary-key deduplication.
pub async fn process_merchant_stripe_event(
    db: &DB,
    event: &StripeEvent,
    scoped_tenant: Option<&str>,
    tenant_ctx: Option<&dyn TenantContextProvider>,
) -> Result<(ProcessOutcome, Vec<PostCommitAction>), StatusCode> {
    if !valid_event_id(&event.id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let result = match &db.store {
        DbStore::Sqlite(pool) => {
            let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.map_err(svc_err)?;
            ensure_sqlite_tables(&mut tx).await.map_err(svc_err)?;
            settle_merchant_tx!(tx, event, scoped_tenant)
        }
        DbStore::Postgres => {
            let mut tx = db.pool.begin().await.map_err(svc_err)?;
            if let Some(ctx) = tenant_ctx {
                ctx.bind_pg_system_transaction(&mut tx)
                    .await
                    .map_err(svc_err)?;
            } else if let Some(tenant) = scoped_tenant {
                ::server_common::auth_utils::set_org_context(&mut *tx, tenant)
                    .await
                    .map_err(svc_err)?;
            }
            ensure_pg_dedup_table(&mut tx).await;
            settle_merchant_tx!(tx, event, scoped_tenant)
        }
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use serde_json::json;
    use tower::ServiceExt;

    fn sign(secret: &str, payload: &[u8], ts: i64) -> String {
        use hmac::Mac;
        let mut mac =
            <hmac::Hmac<sha2::Sha256> as hmac::Mac>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(format!("{ts}.").as_bytes());
        mac.update(payload);
        format!("t={ts},v1={}", hex::encode(mac.finalize().into_bytes()))
    }

    async fn post_signed(
        app: &Router,
        tenant: &str,
        secret: &str,
        ts: i64,
        val: &Value,
    ) -> StatusCode {
        let body = serde_json::to_vec(val).unwrap();
        let req = HttpRequest::builder()
            .method("POST")
            .uri(format!("/api/v1/webhooks/merchant/stripe/{tenant}"))
            .header("Stripe-Signature", sign(secret, &body, ts))
            .body(Body::from(body))
            .unwrap();
        app.clone().oneshot(req).await.unwrap().status()
    }

    async fn query_str(pool: &sqlx::SqlitePool, sql: &str) -> String {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    #[tokio::test]
    async fn sqlite_merchant_webhook_verifies_vault_secret_dedups_and_settles() {
        let _guard = crate::domain::invoice::VAULT_ENV_LOCK.lock().await;
        let prev = ["OMNISOLO_CONNECTION_KEYS", "OMNISOLO_CONNECTION_ACTIVE_KEY"]
            .map(|k| (k, std::env::var_os(k)));
        unsafe {
            std::env::set_var(
                "OMNISOLO_CONNECTION_KEYS",
                format!(r#"{{"v1":"{}"}}"#, "77".repeat(32)),
            );
            std::env::set_var("OMNISOLO_CONNECTION_ACTIVE_KEY", "v1");
        }
        let sqlite = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(2)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let db = Arc::new(DB::from_sqlite_pool(sqlite.clone()).unwrap());
        db.run_migrations().await.unwrap();
        sqlx::query("INSERT INTO tenants (id, name) VALUES ('tenant-m1', 'Merchant Tenant') ON CONFLICT (id) DO NOTHING")
            .execute(&sqlite)
            .await
            .unwrap();
        let secret = "whsec_merchant_workspace_alpha_999";
        store_merchant_webhook_secret(&db, "tenant-m1", secret)
            .await
            .unwrap();
        for stmt in [
            "INSERT INTO proposals (id, tenant_id, customer_id, status, total_amount_cents, required_deposit_cents, stripe_checkout_session_id) VALUES ('prop-m1', 'tenant-m1', 'cust-1', 'ACCEPTED', 100000, 25000, 'cs_m1_dep')",
            "INSERT INTO invoices (id, tenant_id, client_id, client_name, status, due_date, currency, total_amount, total_amount_cents, amount_paid_cents, payment_status, stripe_invoice_id, stripe_checkout_session_id, proposal_id) VALUES ('inv-m1', 'tenant-m1', 'cust-1', 'Buyer', 'open', 1900000000, 'USD', 1000.0, 100000, 0, 'unpaid', 'in_m1', 'cs_m1_dep', 'prop-m1')",
            "INSERT INTO orders (id, tenant_id, customer_id, total_amount, status) VALUES ('ord-m1', 'tenant-m1', 'cust-1', 49.0, 'Pending')",
        ] {
            sqlx::query(stmt).execute(&sqlite).await.unwrap();
        }
        let app = router(MerchantWebhookState { db: db.clone() });
        let now = chrono::Utc::now().timestamp();
        let dep_evt = json!({
            "id": "evt_merch_dep_1", "type": "checkout.session.completed",
            "data": { "object": { "id": "cs_m1_dep", "amount_total": 25000, "currency": "usd",
                "metadata": { "checkout_kind": "proposal_deposit", "tenant_id": "tenant-m1", "proposal_id": "prop-m1" } } }
        });
        let cases = [
            (
                "whsec_wrong_secret",
                now,
                dep_evt.clone(),
                StatusCode::UNAUTHORIZED,
            ),
            (secret, now - 600, dep_evt.clone(), StatusCode::UNAUTHORIZED),
            (
                secret,
                now,
                json!({ "id": "evt_cross_1", "type": "checkout.session.completed",
                "data": { "object": { "id": "cs_x", "metadata": { "checkout_kind": "merchant_order", "tenant_id": "tenant-other" } } } }),
                StatusCode::FORBIDDEN,
            ),
            (secret, now, dep_evt.clone(), StatusCode::OK),
            (secret, now, dep_evt, StatusCode::OK), // replay dedup via processed_webhook_events
        ];
        for (sec, ts, evt, expected) in cases {
            assert_eq!(
                post_signed(&app, "tenant-m1", sec, ts, &evt).await,
                expected
            );
        }
        assert_eq!(
            query_str(&sqlite, "SELECT status FROM proposals WHERE id = 'prop-m1'").await,
            "DEPOSIT_PAID"
        );
        let (paid_c, pay_st): (i32, String) = sqlx::query_as(
            "SELECT amount_paid_cents, payment_status FROM invoices WHERE id = 'inv-m1'",
        )
        .fetch_one(&sqlite)
        .await
        .unwrap();
        assert_eq!((paid_c, pay_st.as_str()), (25000, "partially_paid"));

        let followups = [
            json!({ "id": "evt_merch_inv_1", "type": "invoice.payment_succeeded", "data": { "object": { "id": "in_m1", "amount_paid": 100000, "currency": "usd" } } }),
            json!({ "id": "evt_merch_sub_1", "type": "checkout.session.completed",
                "data": { "object": { "id": "cs_m1_sub", "mode": "subscription", "customer": "cus_m1", "subscription": "sub_merch_1", "amount_total": 4900,
                    "metadata": { "checkout_kind": "merchant_order", "tenant_id": "tenant-m1", "product_id": "prod-box", "order_id": "ord-m1" } } } }),
            json!({ "id": "evt_merch_sub_cancel_1", "type": "customer.subscription.deleted", "data": { "object": { "id": "sub_merch_1", "metadata": { "product_id": "prod-box" } } } }),
        ];
        for evt in followups {
            assert_eq!(
                post_signed(&app, "tenant-m1", secret, now, &evt).await,
                StatusCode::OK
            );
        }
        for (k, v) in prev {
            unsafe {
                if let Some(val) = v {
                    std::env::set_var(k, val);
                } else {
                    std::env::remove_var(k);
                }
            }
        }
        let dedup_cnt: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM processed_webhook_events")
            .fetch_one(&sqlite)
            .await
            .unwrap();
        assert_eq!(
            (
                query_str(&sqlite, "SELECT status FROM invoices WHERE id = 'inv-m1'")
                    .await
                    .as_str(),
                query_str(&sqlite, "SELECT status FROM orders WHERE id = 'ord-m1'")
                    .await
                    .as_str(),
                query_str(
                    &sqlite,
                    "SELECT status FROM subscribers WHERE stripe_subscription_id = 'sub_merch_1'"
                )
                .await
                .as_str(),
                dedup_cnt,
            ),
            ("paid", "Paid", "CANCELED", 4)
        );
    }
}
