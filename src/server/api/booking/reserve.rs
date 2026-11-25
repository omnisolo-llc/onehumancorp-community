use crate::db::DB;
use axum::{
    Router,
    extract::{Extension, Json, State},
    http::StatusCode,
    response::IntoResponse,
    routing::post,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReserveRequest {
    pub customer_id: Option<String>,
    pub customer_name: Option<String>,
    pub customer_email: Option<String>,
    pub service_id: String,
    pub start_time: String,
    pub end_time: String,
}

#[derive(Serialize)]
pub struct ReserveResponse {
    pub success: bool,
    pub booking_id: Option<String>,
    pub error: Option<String>,
    pub checkout_url: Option<String>,
}

fn reservation_window(
    start_time: &str,
    end_time: &str,
) -> Option<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)> {
    let start_time = chrono::DateTime::parse_from_rfc3339(start_time)
        .ok()?
        .with_timezone(&chrono::Utc);
    let end_time = chrono::DateTime::parse_from_rfc3339(end_time)
        .ok()?
        .with_timezone(&chrono::Utc);
    (end_time > start_time && end_time - start_time <= chrono::Duration::hours(24))
        .then_some((start_time, end_time))
}

fn valid_customer_name(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.chars().count() <= 200
        && !value
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n'))
}

fn valid_customer_email(value: &str) -> bool {
    let value = value.trim();
    let Some((local, domain)) = value.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && value.len() <= 320
        && !value.chars().any(char::is_whitespace)
        && !value.contains('\0')
}

fn valid_customer_id(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.len() <= 128
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn required_deposit_cents(price_cents: i64, metadata: &serde_json::Value) -> Option<i64> {
    if metadata
        .get("requires_deposit")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return None;
    }
    let amount = metadata
        .get("deposit_amount_cents")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(price_cents);
    (amount > 0).then_some(amount.min(price_cents))
}

fn booking_feed_payloads(
    booking_id: &str,
    service_id: &str,
    start_time: &str,
    end_time: &str,
) -> (serde_json::Value, serde_json::Value) {
    (
        serde_json::json!({
            "booking_id": booking_id,
            "service_id": service_id,
            "start_time": start_time,
            "end_time": end_time,
            "description": "New booking request",
        }),
        serde_json::json!({
            "action_type": "approve_booking",
            "feature_type": "booking_approval",
            "booking_id": booking_id,
        }),
    )
}

fn frontend_url() -> Result<reqwest::Url, String> {
    let value = std::env::var("FRONTEND_URL")
        .map_err(|_| "FRONTEND_URL is required for paid bookings".to_string())?;
    let mut url =
        reqwest::Url::parse(value.trim()).map_err(|_| "invalid FRONTEND_URL".to_string())?;
    let local_http =
        url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "localhost"));
    if (url.scheme() != "https" && !local_http)
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("invalid FRONTEND_URL".to_string());
    }
    url.set_path("");
    Ok(url)
}

fn trusted_stripe_checkout_url(value: &str) -> Option<String> {
    let url = reqwest::Url::parse(value).ok()?;
    (url.scheme() == "https"
        && url.host_str() == Some("checkout.stripe.com")
        && url.username().is_empty()
        && url.password().is_none())
    .then(|| url.to_string())
}

async fn create_booking_checkout(
    db: &DB,
    booking_id: &str,
    tenant_id: &str,
    service_id: &str,
    customer_id: &str,
    amount_cents: i64,
) -> Result<String, String> {
    let api_key = match crate::api::tool_integrations::stripe_key_for_tenant(db, tenant_id).await {
        Ok(key) => key,
        Err(_) if std::env::var_os("OMNISOLO_CONNECTION_KEYS").is_none() => {
            std::env::var("STRIPE_API_KEY").map_err(|_| "Stripe is not configured".to_string())?
        }
        Err(_) => return Err("Stripe is not configured".to_string()),
    };
    let api_key = api_key.trim();
    if api_key.is_empty() || matches!(api_key, "sk_test" | "sk_test_123" | "sk_test_mock") {
        return Err("Stripe is not configured".to_string());
    }
    if amount_cents <= 0 {
        return Err("booking amount must be positive".to_string());
    }
    let base = frontend_url()?;
    let mut success = base.join("booking").map_err(|error| error.to_string())?;
    success
        .query_pairs_mut()
        .append_pair("tenant", tenant_id)
        .append_pair("service_id", service_id)
        .append_pair("booking_id", booking_id)
        .append_pair("payment", "success");
    let mut cancel = base.join("booking").map_err(|error| error.to_string())?;
    cancel
        .query_pairs_mut()
        .append_pair("tenant", tenant_id)
        .append_pair("service_id", service_id)
        .append_pair("booking_id", booking_id)
        .append_pair("payment", "cancelled");

    let form = [
        ("mode", "payment".to_string()),
        ("success_url", success.to_string()),
        ("cancel_url", cancel.to_string()),
        ("client_reference_id", customer_id.to_string()),
        ("line_items[0][price_data][currency]", "usd".to_string()),
        (
            "line_items[0][price_data][unit_amount]",
            amount_cents.to_string(),
        ),
        (
            "line_items[0][price_data][product_data][name]",
            "Booking deposit".to_string(),
        ),
        ("line_items[0][quantity]", "1".to_string()),
        ("metadata[booking_id]", booking_id.to_string()),
        ("metadata[tenant_id]", tenant_id.to_string()),
        ("metadata[service_id]", service_id.to_string()),
    ];
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|error| error.to_string())?
        .post("https://api.stripe.com/v1/checkout/sessions")
        .basic_auth(api_key, Some(""))
        .form(&form)
        .send()
        .await
        .map_err(|error| format!("Stripe checkout request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("Stripe checkout returned {}", response.status()));
    }
    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "Stripe returned an invalid response".to_string())?;
    payload
        .get("url")
        .and_then(serde_json::Value::as_str)
        .and_then(trusted_stripe_checkout_url)
        .ok_or_else(|| "Stripe returned an invalid checkout URL".to_string())
}

fn parse_sqlite_datetime(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let trimmed = raw.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%#z") {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    if let Ok(dt) = chrono::DateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f%#z") {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S") {
        return Some(naive.and_utc());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(naive.and_utc());
    }
    None
}

struct ValidatedCustomer<'a> {
    supplied_customer_id: Option<String>,
    customer_name: Option<&'a str>,
    customer_email: Option<&'a str>,
}

async fn reserve_sqlite(
    db: &DB,
    sqlite_pool: &sqlx::SqlitePool,
    tenant_id: &str,
    payload: &ReserveRequest,
    window: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>),
    customer: ValidatedCustomer<'_>,
) -> axum::response::Response {
    let (st, et) = window;
    let ValidatedCustomer {
        supplied_customer_id,
        customer_name,
        customer_email,
    } = customer;
    // BEGIN IMMEDIATE acquires SQLite's RESERVED write lock up front so
    // concurrent reservation attempts serialize atomically without SQLITE_BUSY
    // upgrade deadlocks or double-claiming the same availability block.
    let mut tx = match sqlite_pool.begin_with("BEGIN IMMEDIATE").await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!("failed to begin SQLite immediate tx: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ReserveResponse {
                    success: false,
                    booking_id: None,
                    error: Some("internal error".to_string()),
                    checkout_url: None,
                }),
            )
                .into_response();
        }
    };

    let c_id = if let Some(customer_id) = supplied_customer_id {
        customer_id
    } else {
        let email = customer_email.expect("validated customer email");
        match sqlx::query_scalar::<_, String>(
            "SELECT id FROM customers WHERE tenant_id = ? AND lower(email) = lower(?) ORDER BY created_at ASC LIMIT 1",
        )
        .bind(tenant_id)
        .bind(email)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(Some(customer_id)) => customer_id,
            Ok(None) => {
                let customer_id = uuid::Uuid::new_v4().to_string();
                if let Err(error) = sqlx::query(
                    "INSERT INTO customers (id, tenant_id, name, email) VALUES (?, ?, ?, ?)",
                )
                .bind(&customer_id)
                .bind(tenant_id)
                .bind(customer_name.expect("validated customer name"))
                .bind(email)
                .execute(&mut *tx)
                .await
                {
                    tracing::error!("failed to create SQLite booking customer: {error}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": "internal error"})),
                    )
                        .into_response();
                }
                customer_id
            }
            Err(error) => {
                tracing::error!("failed to resolve SQLite booking customer: {error}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "internal error"})),
                )
                    .into_response();
            }
        }
    };

    let product = match sqlx::query(
        "SELECT price_cents, metadata FROM products WHERE id = ? AND tenant_id = ?",
    )
    .bind(&payload.service_id)
    .bind(tenant_id)
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(Some(product)) => product,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "service not found"})),
            )
                .into_response();
        }
        Err(error) => {
            tracing::error!("failed to load SQLite reservation service: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal error"})),
            )
                .into_response();
        }
    };
    let price_cents: i64 = product.try_get("price_cents").unwrap_or(0);
    let metadata: serde_json::Value = product
        .try_get::<Option<String>, _>("metadata")
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let deposit_cents = required_deposit_cents(price_cents, &metadata);

    let candidates = match sqlx::query(
        "SELECT id, CAST(start_time AS TEXT) AS start_time, CAST(end_time AS TEXT) AS end_time \
         FROM availability_blocks WHERE tenant_id = ? AND service_id = ? AND is_available = 1",
    )
    .bind(tenant_id)
    .bind(&payload.service_id)
    .fetch_all(&mut *tx)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!("failed to query SQLite availability blocks: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal error"})),
            )
                .into_response();
        }
    };

    let matching_block_id = candidates.into_iter().find_map(|row| {
        let block_id: String = row.try_get("id").ok()?;
        let row_st: String = row.try_get("start_time").ok()?;
        let row_et: String = row.try_get("end_time").ok()?;
        let parsed_st = parse_sqlite_datetime(&row_st)?;
        let parsed_et = parse_sqlite_datetime(&row_et)?;
        (parsed_st == st && parsed_et == et).then_some(block_id)
    });
    let Some(block_id) = matching_block_id else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "slot unavailable"})),
        )
            .into_response();
    };

    let claimed = match sqlx::query(
        "UPDATE availability_blocks SET is_available = 0, updated_at = CURRENT_TIMESTAMP \
         WHERE id = ? AND tenant_id = ? AND is_available = 1",
    )
    .bind(&block_id)
    .bind(tenant_id)
    .execute(&mut *tx)
    .await
    {
        Ok(res) => res.rows_affected(),
        Err(error) => {
            tracing::error!("failed to claim SQLite reservation slot: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal error"})),
            )
                .into_response();
        }
    };
    if claimed != 1 {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "slot unavailable"})),
        )
            .into_response();
    }

    let booking_id = uuid::Uuid::new_v4().to_string();
    let booking_status = if deposit_cents.is_some() {
        "pending_payment"
    } else {
        "pending"
    };
    let start_iso = st.to_rfc3339();
    let end_iso = et.to_rfc3339();
    if let Err(e) = sqlx::query(
        "INSERT INTO bookings (id, tenant_id, customer_id, service_id, product_id, start_time, end_time, status) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&booking_id)
    .bind(tenant_id)
    .bind(&c_id)
    .bind(&payload.service_id)
    .bind(&payload.service_id)
    .bind(&start_iso)
    .bind(&end_iso)
    .bind(booking_status)
    .execute(&mut *tx)
    .await
    {
        tracing::error!("failed to insert SQLite booking: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ReserveResponse {
                success: false,
                booking_id: None,
                error: Some("failed to create booking".to_string()),
                checkout_url: None,
            }),
        )
            .into_response();
    }

    let feed_id = uuid::Uuid::new_v4().to_string();
    let (feed_context, proposed_action) = booking_feed_payloads(
        &booking_id,
        &payload.service_id,
        &payload.start_time,
        &payload.end_time,
    );
    if let Err(error) = sqlx::query(
        "INSERT INTO agent_feed_items (id, tenant_id, event_source, lifecycle_state, context_payload, proposed_action) \
         VALUES (?, ?, 'booking_request', 'PENDING_APPROVAL', ?, ?)",
    )
    .bind(&feed_id)
    .bind(tenant_id)
    .bind(feed_context.to_string())
    .bind(proposed_action.to_string())
    .execute(&mut *tx)
    .await
    {
        tracing::error!("failed to create SQLite booking feed item: {error}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal error"})),
        )
            .into_response();
    }

    let checkout_url = if let Some(deposit_cents) = deposit_cents {
        match create_booking_checkout(
            db,
            &booking_id,
            tenant_id,
            &payload.service_id,
            &c_id,
            deposit_cents,
        )
        .await
        {
            Ok(url) => Some(url),
            Err(error) => {
                tracing::error!("failed to create booking checkout: {error}");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": "deposit checkout is unavailable"})),
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    if let Err(error) = tx.commit().await {
        tracing::error!("failed to commit SQLite reservation: {error}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal error"})),
        )
            .into_response();
    }

    crate::invalidate_agent_feed_caches(tenant_id).await;

    (
        StatusCode::OK,
        Json(ReserveResponse {
            success: true,
            booking_id: Some(booking_id),
            error: None,
            checkout_url,
        }),
    )
        .into_response()
}

pub fn router<S>(db: Arc<DB>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/", post(handle_reserve))
        .with_state(db)
}

async fn handle_reserve(
    State(db): State<Arc<DB>>,
    claims: Option<Extension<::server_common::Claims>>,
    Json(payload): Json<ReserveRequest>,
) -> impl IntoResponse {
    let tenant_id = match claims
        .as_ref()
        .and_then(|Extension(claims)| ::server_common::auth_utils::signed_tenant_id(claims))
    {
        Some(tenant_id) => tenant_id,
        _ => {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({"error": "unauthorized"})),
            )
                .into_response();
        }
    };
    if payload.service_id.trim().is_empty() || payload.service_id.chars().count() > 128 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid service_id"})),
        )
            .into_response();
    }
    let Some((st, et)) = reservation_window(&payload.start_time, &payload.end_time) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid reservation window"})),
        )
            .into_response();
    };
    let supplied_customer_id = match payload.customer_id.as_deref() {
        Some(customer_id) => {
            if valid_customer_id(customer_id) {
                Some(customer_id.trim().to_string())
            } else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "invalid customer_id"})),
                )
                    .into_response();
            }
        }
        None => None,
    };
    let customer_name = payload.customer_name.as_deref().map(str::trim);
    let customer_email = payload.customer_email.as_deref().map(str::trim);
    if supplied_customer_id.is_none()
        && (!customer_name.is_some_and(valid_customer_name)
            || !customer_email.is_some_and(valid_customer_email))
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "valid customer contact is required"})),
        )
            .into_response();
    }

    if let crate::db::DbStore::Sqlite(sqlite_pool) = &db.store {
        return reserve_sqlite(
            &db,
            sqlite_pool,
            &tenant_id,
            &payload,
            (st, et),
            ValidatedCustomer {
                supplied_customer_id,
                customer_name,
                customer_email,
            },
        )
        .await;
    }

    let pool = db.pool.clone();

    let mut tx = match pool.begin().await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("failed to begin tx: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ReserveResponse {
                    success: false,
                    booking_id: None,
                    error: Some("internal error".to_string()),
                    checkout_url: None,
                }),
            )
                .into_response();
        }
    };

    if let Err(error) = crate::common::auth_utils::set_org_context(&mut *tx, &tenant_id).await {
        tracing::error!("failed to bind reservation tenant context: {error}"); // pii-safe
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal error"})),
        )
            .into_response();
    }

    let c_id = if let Some(customer_id) = supplied_customer_id {
        Some(customer_id)
    } else {
        let email = customer_email.expect("validated customer email");
        match sqlx::query_scalar::<_, String>(
            "SELECT id FROM customers WHERE tenant_id = $1 AND lower(email) = lower($2) ORDER BY created_at ASC LIMIT 1",
        )
        .bind(&tenant_id)
        .bind(email)
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(Some(customer_id)) => Some(customer_id),
            Ok(None) => {
                let customer_id = uuid::Uuid::new_v4().to_string();
                if let Err(error) = sqlx::query(
                    "INSERT INTO customers (id, tenant_id, name, email) VALUES ($1, $2, $3, $4)",
                )
                .bind(&customer_id)
                .bind(&tenant_id)
                .bind(customer_name.expect("validated customer name"))
                .bind(email)
                .execute(&mut *tx)
                .await
                {
                    tracing::error!("failed to create booking customer: {error}");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": "internal error"})),
                    )
                        .into_response();
                }
                Some(customer_id)
            }
            Err(error) => {
                tracing::error!("failed to resolve booking customer: {error}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "internal error"})),
                )
                    .into_response();
            }
        }
    };

    let booking_id = uuid::Uuid::new_v4().to_string();

    let product = match sqlx::query(
        "SELECT price_cents, metadata FROM products WHERE id = $1 AND tenant_id = $2",
    )
    .bind(&payload.service_id)
    .bind(&tenant_id)
    .fetch_optional(&mut *tx)
    .await
    {
        Ok(Some(product)) => product,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "service not found"})),
            )
                .into_response();
        }
        Err(error) => {
            tracing::error!("failed to load reservation service: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal error"})),
            )
                .into_response();
        }
    };
    let price_cents: i64 = product.try_get("price_cents").unwrap_or(0);
    let metadata: serde_json::Value = product
        .try_get("metadata")
        .unwrap_or_else(|_| serde_json::json!({}));
    let deposit_cents = required_deposit_cents(price_cents, &metadata);

    let claimed_slot = sqlx::query(
        "UPDATE availability_blocks SET is_available = false WHERE tenant_id = $1 AND service_id = $2 AND start_time = $3 AND end_time = $4 AND is_available = true RETURNING id",
    )
    .bind(&tenant_id)
    .bind(&payload.service_id)
    .bind(st)
    .bind(et)
    .fetch_optional(&mut *tx)
    .await;
    match claimed_slot {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": "slot unavailable"})),
            )
                .into_response();
        }
        Err(error) => {
            tracing::error!("failed to claim reservation slot: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal error"})),
            )
                .into_response();
        }
    }

    let booking_status = if deposit_cents.is_some() {
        "pending_payment"
    } else {
        "pending"
    };
    let res = sqlx::query(
        r#"
        INSERT INTO bookings (id, tenant_id, customer_id, product_id, start_time, end_time, status)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(&booking_id)
    .bind(&tenant_id)
    .bind(&c_id)
    .bind(&payload.service_id)
    .bind(st)
    .bind(et)
    .bind(booking_status)
    .execute(&mut *tx)
    .await;

    if let Err(e) = res {
        tracing::error!("failed to insert booking: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ReserveResponse {
                success: false,
                booking_id: None,
                error: Some("failed to create booking".to_string()),
                checkout_url: None,
            }),
        )
            .into_response();
    }

    // Add feed item within the same transaction as the reservation.
    let feed_id = uuid::Uuid::new_v4().to_string();
    let (feed_context, proposed_action) = booking_feed_payloads(
        &booking_id,
        &payload.service_id,
        &payload.start_time,
        &payload.end_time,
    );
    let feed_result = sqlx::query(
        r#"
        INSERT INTO agent_feed_items (id, tenant_id, event_source, lifecycle_state, context_payload, proposed_action)
        VALUES ($1, $2, 'booking_request', 'PENDING_APPROVAL', $3, $4)
        "#,
    )
    .bind(&feed_id)
    .bind(&tenant_id)
    .bind(feed_context)
    .bind(proposed_action)
    .execute(&mut *tx)
    .await;
    if let Err(error) = feed_result {
        tracing::error!("failed to create booking feed item: {error}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal error"})),
        )
            .into_response();
    }
    let checkout_url = if let Some(deposit_cents) = deposit_cents {
        match create_booking_checkout(
            &db,
            &booking_id,
            &tenant_id,
            &payload.service_id,
            &c_id
                .expect("booking customer is always resolved")
                .to_string(),
            deposit_cents,
        )
        .await
        {
            Ok(url) => Some(url),
            Err(error) => {
                tracing::error!("failed to create booking checkout: {error}");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({"error": "deposit checkout is unavailable"})),
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    if let Err(error) = tx.commit().await {
        tracing::error!("failed to commit reservation: {error}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal error"})),
        )
            .into_response();
    }

    // Only publish the new feed state after the reservation and its approval
    // item are durable. Invalidate every cached page for this tenant.
    crate::invalidate_agent_feed_caches(&tenant_id).await;

    (
        StatusCode::OK,
        Json(ReserveResponse {
            success: true,
            booking_id: Some(booking_id),
            error: None,
            checkout_url,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_window_rejects_inverted_and_oversized_ranges() {
        assert!(reservation_window("2026-07-15T10:00:00Z", "2026-07-15T11:00:00Z",).is_some());
        assert!(reservation_window("2026-07-15T11:00:00Z", "2026-07-15T10:00:00Z",).is_none());
        assert!(reservation_window("2026-07-15T10:00:00Z", "2026-07-17T10:00:01Z",).is_none());
    }

    #[test]
    fn customer_contact_validation_rejects_non_email_ids_and_control_characters() {
        assert!(valid_customer_name("Jane Doe"));
        assert!(!valid_customer_name("Jane\nDoe"));
        assert!(valid_customer_email("jane@example.com"));
        assert!(!valid_customer_email("jane.example.com"));
        assert!(!valid_customer_email("jane@localhost"));
        assert!(valid_customer_id("e2e-customer-bakery"));
        assert!(!valid_customer_id("../../other-tenant"));
    }

    #[test]
    fn checkout_is_required_only_when_product_metadata_requires_a_deposit() {
        assert_eq!(required_deposit_cents(7_500, &serde_json::json!({})), None);
        assert_eq!(
            required_deposit_cents(
                7_500,
                &serde_json::json!({"requires_deposit": true, "deposit_amount_cents": 2_500}),
            ),
            Some(2_500),
        );
        assert_eq!(
            required_deposit_cents(
                7_500,
                &serde_json::json!({"requires_deposit": true, "deposit_amount_cents": 20_000}),
            ),
            Some(7_500),
        );
    }

    #[test]
    fn booking_feed_item_contains_an_actionable_approval() {
        let (context, action) = booking_feed_payloads(
            "booking-1",
            "service-1",
            "2026-08-10T09:00:00Z",
            "2026-08-10T10:00:00Z",
        );
        assert_eq!(context["booking_id"], "booking-1");
        assert_eq!(action["action_type"], "approve_booking");
        assert_eq!(action["feature_type"], "booking_approval");
        assert_eq!(action["booking_id"], "booking-1");
    }

    #[test]
    fn checkout_urls_are_restricted_to_stripe() {
        assert!(
            trusted_stripe_checkout_url("https://checkout.stripe.com/c/pay/cs_live_123").is_some()
        );
        assert!(trusted_stripe_checkout_url("javascript:alert(1)").is_none());
        assert!(
            trusted_stripe_checkout_url("https://checkout.stripe.com.attacker.test/pay").is_none()
        );
        assert!(trusted_stripe_checkout_url("https://user:pass@checkout.stripe.com/pay").is_none());
    }

    #[tokio::test]
    async fn sqlite_booking_reserve_enforces_atomic_slot_conflict_protection() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use std::str::FromStr;
        use tower::ServiceExt;

        let temp_dir = tempfile::tempdir().expect("tempdir for SQLite file");
        let db_path = temp_dir.path().join("booking_atomicity.db");
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&format!(
            "sqlite://{}",
            db_path.display()
        ))
        .unwrap()
        .create_if_missing(true)
        .busy_timeout(std::time::Duration::from_secs(5))
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
        let sqlite = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .expect("SQLite pool");
        let db = Arc::new(DB::from_sqlite_pool(sqlite.clone()).unwrap());
        db.run_migrations().await.expect("SQLite migrations");

        sqlx::query("INSERT INTO tenants (id, name) VALUES ('tenant-bk', 'Booking Tenant') ON CONFLICT (id) DO NOTHING")
            .execute(&sqlite)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO products (id, tenant_id, name, title, price_cents, metadata) \
             VALUES ('svc-haircut', 'tenant-bk', 'Haircut', 'Haircut', 6500, '{}')",
        )
        .execute(&sqlite)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO availability_blocks (id, tenant_id, service_id, start_time, end_time, is_available) \
             VALUES ('slot-1', 'tenant-bk', 'svc-haircut', '2026-09-15T14:00:00Z', '2026-09-15T15:00:00Z', 1)",
        )
        .execute(&sqlite)
        .await
        .unwrap();

        let claims = ::server_common::Claims {
            sub: "owner-bk".to_string(),
            exp: i64::MAX,
            iat: 0,
            organization_id: Some("tenant-bk".to_string()),
            username: "owner".to_string(),
            email: "owner@example.com".to_string(),
            roles: vec!["owner".to_string()],
            session_id: None,
            jti: "jti-bk".to_string(),
        };
        let app = router(db.clone()).layer(Extension(claims));

        let mut futs = Vec::new();
        for idx in 0..5 {
            let app_clone = app.clone();
            futs.push(async move {
                let body = serde_json::json!({
                    "customer_name": format!("Guest {idx}"),
                    "customer_email": format!("guest{idx}@example.com"),
                    "service_id": "svc-haircut",
                    "start_time": "2026-09-15T14:00:00Z",
                    "end_time": "2026-09-15T15:00:00Z"
                })
                .to_string();
                let resp = app_clone
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/")
                            .header("content-type", "application/json")
                            .body(Body::from(body))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let status = resp.status();
                let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
                let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                (status, payload)
            });
        }

        let results = futures::future::join_all(futs).await;
        let ok_count = results
            .iter()
            .filter(|(status, payload)| {
                *status == StatusCode::OK
                    && payload["success"] == true
                    && payload["booking_id"].is_string()
            })
            .count();
        let conflict_count = results
            .iter()
            .filter(|(status, payload)| {
                *status == StatusCode::CONFLICT && payload["error"] == "slot unavailable"
            })
            .count();
        assert_eq!(
            ok_count, 1,
            "exactly one concurrent reservation must win: {results:?}"
        );
        assert_eq!(
            conflict_count, 4,
            "all losing reservations must return 409 Conflict: {results:?}"
        );

        let bookings_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM bookings WHERE tenant_id = 'tenant-bk'")
                .fetch_one(&sqlite)
                .await
                .unwrap();
        assert_eq!(bookings_count, 1);

        let feed_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_feed_items WHERE tenant_id = 'tenant-bk' AND event_source = 'booking_request'",
        )
        .fetch_one(&sqlite)
        .await
        .unwrap();
        assert_eq!(feed_count, 1);

        let is_available: i64 =
            sqlx::query_scalar("SELECT is_available FROM availability_blocks WHERE id = 'slot-1'")
                .fetch_one(&sqlite)
                .await
                .unwrap();
        assert_eq!(is_available, 0);
    }
}
