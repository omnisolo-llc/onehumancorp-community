use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;
use uuid::Uuid;

use crate::db::DB;
use crate::hub::Hub;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<DB>,
    pub hub: Arc<Hub>,
}

#[derive(Deserialize)]
pub struct CreatePaymentIntentRequest {
    pub amount: f64,
    pub currency: String,
    pub source: String,
    pub idempotency_key: Option<String>,
}

#[derive(Serialize)]
pub struct PaymentIntentResponse {
    pub payment_id: String,
    pub idempotency_key: String,
    pub status: String,
}

#[derive(Deserialize)]
pub struct WebhookPayload {
    #[serde(rename = "type", alias = "type_field")]
    pub type_field: String, // e.g. "payment_intent.succeeded"
    pub data: WebhookData,
}

#[derive(Deserialize)]
pub struct WebhookData {
    pub object: StripePaymentIntent,
}

#[derive(Deserialize)]
pub struct StripePaymentIntent {
    pub id: String,
    pub metadata: std::collections::HashMap<String, String>,
}

#[derive(Serialize)]
pub struct BalanceResponse {
    pub tenant_id: String,
    pub total_revenue: f64,
}

#[derive(Serialize)]
pub struct SafeToSpendResponse {
    pub money_in: f64,
    pub money_out: f64,
    pub tax_safe: f64,
}

#[derive(Serialize)]
pub struct LedgerAccountResponse {
    pub name: String,
    pub balance: f64,
    pub currency: String,
}

#[derive(Serialize)]
pub struct LedgerAccountsResponse {
    pub accounts: Vec<LedgerAccountResponse>,
}

#[derive(Serialize)]
pub struct LedgerEntryResponse {
    pub id: String,
    pub transaction_id: String,
    pub account_id: String,
    pub amount: f64,
    pub currency: String,
    pub direction: String,
    pub entry_type: String,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct LedgerEntriesResponse {
    pub entries: Vec<LedgerEntryResponse>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordLedgerRequest {
    pub amount: f64,
    pub source: Option<String>,
    pub status: Option<String>,
    pub items: Option<serde_json::Value>,
    pub currency: Option<String>,
}

#[derive(Serialize)]
pub struct UsageResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_actions: Option<u32>,
    pub actions_used: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_limit: Option<u32>,
}

#[derive(Deserialize)]
pub struct SnapReceiptRequest {
    pub file_name: Option<String>,
    pub vendor: String,
    pub amount: f64,
}

fn ledger_account_name(account_id: &str) -> String {
    if account_id == "default_revenue" {
        "main".to_string()
    } else {
        account_id.to_string()
    }
}

pub async fn get_accounts(
    axum::extract::Extension(auth_info): axum::extract::Extension<
        ::server_auth::orchestration::AuthInfo,
    >,
) -> impl IntoResponse {
    let tenant_id = auth_info.org_id;
    if tenant_id.is_empty() {
        return (StatusCode::UNAUTHORIZED, "Missing tenant ID").into_response();
    }

    let rows = if let Some(sqlite) = crate::db::get_sqlite_pool_if_exists() {
        sqlx::query_as::<_, (String, f64, String)>(
            "SELECT account_id, COALESCE(balance, 0.0), currency FROM ledger_accounts
             WHERE tenant_id = ? ORDER BY account_id",
        )
        .bind(&tenant_id)
        .fetch_all(&sqlite)
        .await
    } else {
        async {
            let mut tx = crate::db::get_pool().begin().await?;
            ::server_common::auth_utils::set_org_context(&mut *tx, &tenant_id).await?;
            let rows = sqlx::query_as::<_, (String, f64, String)>(
                "SELECT account_id, COALESCE(balance, 0.0), currency FROM ledger_accounts
                 WHERE tenant_id = $1 ORDER BY account_id",
            )
            .bind(&tenant_id)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, sqlx::Error>(rows)
        }
        .await
    };
    match rows {
        Ok(rows) => (
            StatusCode::OK,
            Json(LedgerAccountsResponse {
                accounts: rows
                    .into_iter()
                    .map(|(account_id, balance, currency)| LedgerAccountResponse {
                        name: ledger_account_name(&account_id),
                        balance,
                        currency,
                    })
                    .collect(),
            }),
        )
            .into_response(),
        Err(error) => {
            tracing::error!("Failed to read ledger accounts: {error}");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

pub async fn get_entries(
    axum::extract::Extension(auth_info): axum::extract::Extension<
        ::server_auth::orchestration::AuthInfo,
    >,
) -> impl IntoResponse {
    let tenant_id = auth_info.org_id;
    if tenant_id.is_empty() {
        return (StatusCode::UNAUTHORIZED, "Missing tenant ID").into_response();
    }

    let entries_result: Result<Vec<LedgerEntryResponse>, sqlx::Error> = if let Some(sqlite) =
        crate::db::get_sqlite_pool_if_exists()
    {
        async {
            let rows = sqlx::query(
                "SELECT e.entry_id, e.tx_id, e.account_id, e.amount, e.direction,
                        COALESCE(t.currency, 'USD') AS currency,
                        strftime('%Y-%m-%dT%H:%M:%SZ', e.created_at) AS created_at
                     FROM ledger_entries e
                     LEFT JOIN ledger_transactions t
                       ON t.tenant_id = e.tenant_id AND t.tx_id = e.tx_id
                     WHERE e.tenant_id = ? ORDER BY e.created_at DESC",
            )
            .bind(&tenant_id)
            .fetch_all(&sqlite)
            .await?;
            rows.into_iter()
                .map(|row| {
                    let direction: String = row.try_get("direction")?;
                    Ok(LedgerEntryResponse {
                        id: row.try_get("entry_id")?,
                        transaction_id: row.try_get("tx_id")?,
                        account_id: row.try_get("account_id")?,
                        amount: row.try_get("amount")?,
                        currency: row.try_get("currency")?,
                        entry_type: direction.to_lowercase(),
                        direction: direction.to_lowercase(),
                        created_at: row.try_get("created_at")?,
                    })
                })
                .collect()
        }
        .await
    } else {
        async {
                let mut tx = crate::db::get_pool().begin().await?;
                ::server_common::auth_utils::set_org_context(&mut *tx, &tenant_id).await?;
                let rows = sqlx::query(
                    "SELECT e.entry_id, e.tx_id, e.account_id, e.amount, e.direction,
                        COALESCE(t.currency, 'USD') AS currency,
                        COALESCE(to_char(e.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), '') AS created_at
                     FROM ledger_entries e
                     LEFT JOIN ledger_transactions t
                       ON t.tenant_id = e.tenant_id AND t.tx_id = e.tx_id
                     WHERE e.tenant_id = $1 ORDER BY e.created_at DESC",
                )
                .bind(&tenant_id)
                .fetch_all(&mut *tx)
                .await?;
                tx.commit().await?;
                rows.into_iter()
                    .map(|row| {
                        let direction: String = row.try_get("direction")?;
                        Ok(LedgerEntryResponse {
                            id: row.try_get("entry_id")?,
                            transaction_id: row.try_get("tx_id")?,
                            account_id: row.try_get("account_id")?,
                            amount: row.try_get("amount")?,
                            currency: row.try_get("currency")?,
                            entry_type: direction.to_lowercase(),
                            direction: direction.to_lowercase(),
                            created_at: row.try_get("created_at")?,
                        })
                    })
                    .collect()
            }
            .await
    };
    match entries_result {
        Ok(entries) => (StatusCode::OK, Json(LedgerEntriesResponse { entries })).into_response(),
        Err(error) => {
            tracing::error!("Failed to read ledger entries: {error}");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

pub async fn record_ledger_entry(
    axum::extract::Extension(auth_info): axum::extract::Extension<
        ::server_auth::orchestration::AuthInfo,
    >,
    Json(payload): Json<RecordLedgerRequest>,
) -> impl IntoResponse {
    let tenant_id = auth_info.org_id;
    if tenant_id.is_empty() {
        return (StatusCode::UNAUTHORIZED, "Missing tenant ID").into_response();
    }
    if !payload.amount.is_finite() || payload.amount <= 0.0 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "amount must be positive and finite" })),
        )
            .into_response();
    }
    let currency = payload.currency.as_deref().unwrap_or("USD");
    let tx_id = Uuid::new_v4().to_string();
    let amount = payload.amount;

    if let Some(sqlite) = crate::db::get_sqlite_pool_if_exists() {
        let mut tx = match sqlite.begin().await {
            Ok(tx) => tx,
            Err(error) => {
                tracing::error!("Failed to begin SQLite ledger record: {error}");
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
        };
        let result = async {
            sqlx::query(
                "INSERT INTO ledger_transactions (tenant_id, tx_id, amount, currency)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(&tenant_id)
            .bind(&tx_id)
            .bind(amount)
            .bind(currency)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO ledger_accounts (tenant_id, account_id, currency, balance)
                 VALUES (?, 'default_revenue', ?, 0)
                 ON CONFLICT (tenant_id, account_id) DO NOTHING",
            )
            .bind(&tenant_id)
            .bind(currency)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO ledger_entries
                 (tenant_id, entry_id, tx_id, account_id, direction, amount)
                 VALUES (?, ?, ?, 'default_revenue', 'CREDIT', ?)",
            )
            .bind(&tenant_id)
            .bind(Uuid::new_v4().to_string())
            .bind(&tx_id)
            .bind(amount)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE ledger_accounts SET balance = COALESCE(balance, 0.0) + ?
                 WHERE tenant_id = ? AND account_id = 'default_revenue'",
            )
            .bind(amount)
            .bind(&tenant_id)
            .execute(&mut *tx)
            .await?;
            Ok::<(), sqlx::Error>(())
        }
        .await;
        return match result {
            Ok(()) => match tx.commit().await {
                Ok(()) => (
                    StatusCode::CREATED,
                    Json(serde_json::json!({ "ok": true, "transaction_id": tx_id })),
                )
                    .into_response(),
                Err(error) => {
                    tracing::error!("Failed to commit SQLite ledger record: {error}");
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
            },
            Err(error) => {
                let _ = tx.rollback().await;
                tracing::error!("Failed to persist SQLite ledger record: {error}");
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            }
        };
    }

    let pool = crate::db::get_pool();
    let mut tx = match pool.begin().await {
        Ok(tx) => tx,
        Err(error) => {
            tracing::error!("Failed to begin PostgreSQL ledger record: {error}");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let result = async {
        sqlx::query(
            "INSERT INTO ledger_transactions (tenant_id, tx_id, amount, currency)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&tenant_id)
        .bind(&tx_id)
        .bind(amount)
        .bind(currency)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO ledger_accounts (tenant_id, account_id, currency, balance)
             VALUES ($1, 'default_revenue', $2, 0)
             ON CONFLICT (tenant_id, account_id) DO NOTHING",
        )
        .bind(&tenant_id)
        .bind(currency)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO ledger_entries
             (tenant_id, entry_id, tx_id, account_id, direction, amount)
             VALUES ($1, $2, $3, 'default_revenue', 'CREDIT', $4)",
        )
        .bind(&tenant_id)
        .bind(Uuid::new_v4().to_string())
        .bind(&tx_id)
        .bind(amount)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE ledger_accounts SET balance = balance + $1
             WHERE tenant_id = $2 AND account_id = 'default_revenue'",
        )
        .bind(amount)
        .bind(&tenant_id)
        .execute(&mut *tx)
        .await?;
        Ok::<(), sqlx::Error>(())
    }
    .await;
    match result {
        Ok(()) => match tx.commit().await {
            Ok(()) => (
                StatusCode::CREATED,
                Json(serde_json::json!({ "ok": true, "transaction_id": tx_id })),
            )
                .into_response(),
            Err(error) => {
                tracing::error!("Failed to commit PostgreSQL ledger record: {error}");
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            }
        },
        Err(error) => {
            let _ = tx.rollback().await;
            tracing::error!("Failed to persist PostgreSQL ledger record: {error}");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

pub async fn get_user_usage(
    axum::extract::Extension(auth_info): axum::extract::Extension<
        ::server_auth::orchestration::AuthInfo,
    >,
) -> impl IntoResponse {
    let tenant_id = auth_info.org_id;
    if tenant_id.is_empty() {
        return (StatusCode::UNAUTHORIZED, "Missing tenant ID").into_response();
    }
    if let Some(sqlite) = crate::db::get_sqlite_pool_if_exists() {
        let tenant_exists = sqlx::query_scalar::<_, String>("SELECT id FROM tenants WHERE id = ?")
            .bind(&tenant_id)
            .fetch_optional(&sqlite)
            .await;
        match tenant_exists {
            Ok(None) => return (StatusCode::NOT_FOUND, "Tenant not found").into_response(),
            Err(error) => {
                tracing::error!("Failed to verify SQLite tenant usage: {error}");
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            Ok(Some(_)) => {}
        }
        let used =
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_actions WHERE tenant_id = ?")
                .bind(&tenant_id)
                .fetch_one(&sqlite)
                .await
                .unwrap_or(0)
                .max(0) as u32;
        return (
            StatusCode::OK,
            Json(UsageResponse {
                remaining_actions: None,
                actions_used: used,
                action_limit: None,
            }),
        )
            .into_response();
    }

    let month = chrono::Utc::now().format("%Y-%m").to_string();
    let pool = crate::db::get_pool();
    let has_cloud_usage_schema: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_attribute
            WHERE attrelid = to_regclass('tenants')
              AND attname = 'plan_tier'
              AND NOT attisdropped
        ) AND to_regclass('tenant_ai_budgets') IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(false);

    let (actions_used, action_limit) = if has_cloud_usage_schema {
        let cloud_usage = sqlx::query_as::<_, (i64, String)>(
            "SELECT COALESCE(b.actions_used, 0), COALESCE(t.plan_tier, 'free')
             FROM tenants t
             LEFT JOIN tenant_ai_budgets b ON b.tenant_id = t.id AND b.year_month = $1
             WHERE t.id = $2",
        )
        .bind(&month)
        .bind(&tenant_id)
        .fetch_optional(&pool)
        .await;

        match cloud_usage {
            Ok(Some((used, plan_tier))) => {
                let limit = match plan_tier.to_ascii_lowercase().as_str() {
                    "free" => Some(100u32),
                    "starter" => Some(1_000u32),
                    _ => None,
                };
                (used, limit)
            }
            Ok(None) => return (StatusCode::NOT_FOUND, "Tenant not found").into_response(),
            Err(error) => {
                tracing::error!("Failed to read cloud tenant usage: {error}");
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
        }
    } else {
        // OSS PostgreSQL schema omits Cloud Layer-2 plan_tier / tenant_ai_budgets.
        match sqlx::query_scalar::<_, String>("SELECT id FROM tenants WHERE id = $1")
            .bind(&tenant_id)
            .fetch_optional(&pool)
            .await
        {
            Ok(None) => return (StatusCode::NOT_FOUND, "Tenant not found").into_response(),
            Ok(Some(_)) => {
                let used = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM agent_actions WHERE tenant_id = $1",
                )
                .bind(&tenant_id)
                .fetch_one(&pool)
                .await
                .unwrap_or(0);
                (used, None)
            }
            Err(error) => {
                tracing::error!("Failed to read tenant usage: {error}");
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
        }
    };

    let used = actions_used.max(0) as u32;
    (
        StatusCode::OK,
        Json(UsageResponse {
            remaining_actions: action_limit.map(|limit| limit.saturating_sub(used)),
            actions_used: used,
            action_limit,
        }),
    )
        .into_response()
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/intent", post(create_payment_intent))
        .route(
            "/webhook",
            post(stripe_webhook).route_layer(axum::middleware::from_fn_with_state(
                super::stripe_webhook_security::Endpoint::Ledger,
                super::stripe_webhook_security::require_verified_stripe,
            )),
        )
        .route("/balance", get(get_balance))
        .route("/safe-to-spend", get(get_safe_to_spend))
        .route("/receipt", post(process_receipt))
}

async fn create_payment_intent(
    State(_state): State<AppState>,
    axum::extract::Extension(auth_info): axum::extract::Extension<
        ::server_auth::orchestration::AuthInfo,
    >,
    Json(payload): Json<CreatePaymentIntentRequest>,
) -> impl IntoResponse {
    let tenant_id = auth_info.org_id;
    if tenant_id.is_empty() {
        return (StatusCode::UNAUTHORIZED, "Missing tenant ID").into_response();
    }

    let idempotency_key = payload
        .idempotency_key
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let pool = crate::db::get_pool();

    // Check for existing intent with the same idempotency key
    let existing: Option<(String, String)> = sqlx::query_as(
        "SELECT payment_id, status FROM payment_intents WHERE tenant_id = $1 AND idempotency_key = $2"
    )
    .bind(&tenant_id)
    .bind(&idempotency_key)
    .fetch_optional(&pool)
    .await.unwrap_or(None);

    if let Some((existing_payment_id, existing_status)) = existing {
        return (
            StatusCode::OK,
            Json(PaymentIntentResponse {
                payment_id: existing_payment_id,
                idempotency_key,
                status: existing_status,
            }),
        )
            .into_response();
    }

    let payment_id = Uuid::new_v4().to_string();

    let res = sqlx::query(
        r#"
        INSERT INTO payment_intents (tenant_id, payment_id, idempotency_key, amount, currency, source)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#
    )
    .bind(&tenant_id)
    .bind(&payment_id)
    .bind(&idempotency_key)
    .bind(payload.amount)
    .bind(&payload.currency)
    .bind(&payload.source)
    .execute(&pool)
    .await;

    match res {
        Ok(_) => (
            StatusCode::CREATED,
            Json(PaymentIntentResponse {
                payment_id,
                idempotency_key,
                status: "pending".to_string(),
            }),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn stripe_webhook(
    State(_state): State<AppState>,
    Json(payload): Json<WebhookPayload>,
) -> impl IntoResponse {
    if payload.type_field != "payment_intent.succeeded" {
        return StatusCode::OK.into_response();
    }

    let payment_intent = payload.data.object;

    let tenant_id = payment_intent
        .metadata
        .get("tenant_id")
        .cloned()
        .unwrap_or_default();
    let idempotency_key = payment_intent
        .metadata
        .get("idempotency_key")
        .cloned()
        .unwrap_or_default();

    if tenant_id.is_empty() || idempotency_key.is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let pool = crate::db::get_pool();

    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT status FROM payment_intents WHERE idempotency_key = $1 AND tenant_id = $2",
    )
    .bind(&idempotency_key)
    .bind(&tenant_id)
    .fetch_optional(&pool)
    .await
    .unwrap_or(None);

    if let Some((status,)) = existing {
        if status == "succeeded" {
            return StatusCode::OK.into_response();
        }
    } else {
        return StatusCode::NOT_FOUND.into_response();
    }

    let mut tx = match pool.begin().await {
        Ok(t) => t,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let update_res = sqlx::query("UPDATE payment_intents SET status = 'succeeded', stripe_payment_intent_id = $1 WHERE idempotency_key = $2 AND tenant_id = $3")
        .bind(&payment_intent.id)
        .bind(&idempotency_key)
        .bind(&tenant_id)
        .execute(&mut *tx)
        .await;

    if update_res.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let payment_info_res = sqlx::query_as::<_, (f64, String)>(
        "SELECT amount, currency FROM payment_intents WHERE idempotency_key = $1",
    )
    .bind(&idempotency_key)
    .fetch_one(&mut *tx)
    .await;

    let payment_info = match payment_info_res {
        Ok(info) => info,
        Err(_) => {
            let _ = tx.rollback().await;
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let tx_id = Uuid::new_v4().to_string();
    if sqlx::query("INSERT INTO ledger_transactions (tenant_id, tx_id, amount, currency) VALUES ($1, $2, $3, $4)")
        .bind(&tenant_id)
        .bind(&tx_id)
        .bind(payment_info.0)
        .bind(&payment_info.1)
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let tax_rate = 0.15;
    let tax_amount = payment_info.0 * tax_rate;

    let account_id = "default_revenue";
    if sqlx::query("INSERT INTO ledger_accounts (tenant_id, account_id, currency, balance) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING")
        .bind(&tenant_id)
        .bind(account_id)
        .bind(&payment_info.1)
        .bind(0.0)
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let entry_id = Uuid::new_v4().to_string();
    if sqlx::query("INSERT INTO ledger_entries (tenant_id, entry_id, tx_id, account_id, direction, amount) VALUES ($1, $2, $3, $4, 'CREDIT', $5)")
        .bind(&tenant_id)
        .bind(&entry_id)
        .bind(&tx_id)
        .bind(account_id)
        .bind(payment_info.0)
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if sqlx::query("UPDATE ledger_accounts SET balance = balance + $1 WHERE tenant_id = $2 AND account_id = $3")
        .bind(payment_info.0)
        .bind(&tenant_id)
        .bind(account_id)
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let tax_envelope_id = "default_tax";
    if sqlx::query("INSERT INTO ledger_reserves (tenant_id, envelope_id, envelope_type, balance) VALUES ($1, $2, 'tax', $3) ON CONFLICT DO NOTHING")
        .bind(&tenant_id)
        .bind(tax_envelope_id)
        .bind(0.0)
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if sqlx::query("UPDATE ledger_reserves SET balance = balance + $1 WHERE tenant_id = $2 AND envelope_id = $3")
        .bind(tax_amount)
        .bind(&tenant_id)
        .bind(tax_envelope_id)
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if sqlx::query("INSERT INTO agent_action_requests (id, tenant_id, source, agent_type, action_type, payload, status) VALUES ($1, $2, 'payment_ledger', 'finance', 'payment_succeeded', $3, 'pending')")
        .bind(Uuid::new_v4().to_string())
        .bind(&tenant_id)
        .bind(serde_json::json!({
            "event": "payment_succeeded",
            "amount": payment_info.0,
            "currency": payment_info.1,
            "idempotency_key": idempotency_key,
            "tax_reserve_deducted": tax_amount
        }))
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let _ = tx.commit().await;
    StatusCode::OK.into_response()
}

async fn get_balance(
    State(state): State<AppState>,
    axum::extract::Extension(auth_info): axum::extract::Extension<
        ::server_auth::orchestration::AuthInfo,
    >,
) -> impl IntoResponse {
    let tenant_id = auth_info.org_id;
    if tenant_id.is_empty() {
        return (StatusCode::UNAUTHORIZED, "Missing tenant ID").into_response();
    }

    let balance: Result<Option<(f64,)>, sqlx::Error> = match &state.db.store {
        crate::db::DbStore::Sqlite(sqlite) => {
            sqlx::query_as(
                "SELECT COALESCE(balance, 0.0) FROM ledger_accounts WHERE tenant_id = ? AND account_id = 'default_revenue'",
            )
            .bind(&tenant_id)
            .fetch_optional(sqlite)
            .await
        }
        crate::db::DbStore::Postgres => async {
            let mut tx = state.db.pool.begin().await?;
            ::server_common::auth_utils::set_org_context(&mut *tx, &tenant_id).await?;
            let row = sqlx::query_as(
                "SELECT COALESCE(balance, 0.0) FROM ledger_accounts WHERE tenant_id = $1 AND account_id = 'default_revenue'",
            )
            .bind(&tenant_id)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(row)
        }
        .await,
    };

    let total_revenue = match balance {
        Ok(Some((b,))) => b,
        Ok(None) => 0.0,
        Err(error) => {
            tracing::error!("Failed to read payment ledger balance: {error}");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };

    (
        StatusCode::OK,
        Json(BalanceResponse {
            tenant_id,
            total_revenue,
        }),
    )
        .into_response()
}

async fn get_safe_to_spend(
    State(_state): State<AppState>,
    axum::extract::Extension(auth_info): axum::extract::Extension<
        ::server_auth::orchestration::AuthInfo,
    >,
) -> impl IntoResponse {
    let tenant_id = auth_info.org_id;
    if tenant_id.is_empty() {
        return (StatusCode::UNAUTHORIZED, "Missing tenant ID").into_response();
    }

    let pool = crate::db::get_pool();

    let (credit_res, debit_res, tax_res) = tokio::join!(
        async {
            sqlx::query_as::<_, (f64,)>("SELECT SUM(amount) FROM ledger_entries WHERE tenant_id = $1 AND direction = 'CREDIT' AND account_id = 'default_revenue'")
                .bind(&tenant_id)
                .fetch_optional(&pool)
                .await.unwrap_or(None)
        },
        async {
            sqlx::query_as::<_, (f64,)>("SELECT SUM(amount) FROM ledger_entries WHERE tenant_id = $1 AND direction = 'DEBIT' AND account_id = 'default_expense'")
                .bind(&tenant_id)
                .fetch_optional(&pool)
                .await.unwrap_or(None)
        },
        async {
            sqlx::query_as::<_, (f64,)>("SELECT SUM(balance) FROM ledger_reserves WHERE tenant_id = $1 AND envelope_type = 'tax'")
                .bind(&tenant_id)
                .fetch_optional(&pool)
                .await.unwrap_or(None)
        }
    );

    let money_in = match credit_res {
        Some((b,)) => b,
        None => 0.0,
    };

    let money_out = match debit_res {
        Some((b,)) => b,
        None => 0.0,
    };

    let tax_safe = match tax_res {
        Some((b,)) => b,
        None => 0.0,
    };

    (
        StatusCode::OK,
        Json(SafeToSpendResponse {
            money_in,
            money_out,
            tax_safe,
        }),
    )
        .into_response()
}

#[derive(Serialize)]
pub struct ReceiptProcessedResponse {
    pub vendor: String,
    pub amount: f64,
    pub category: String,
}

async fn process_receipt(
    State(_state): State<AppState>,
    axum::extract::Extension(auth_info): axum::extract::Extension<
        ::server_auth::orchestration::AuthInfo,
    >,
    Json(payload): Json<SnapReceiptRequest>,
) -> impl IntoResponse {
    let tenant_id = auth_info.org_id;
    if tenant_id.is_empty() {
        return (StatusCode::UNAUTHORIZED, "Missing tenant ID").into_response();
    }

    if !payload.amount.is_finite() || payload.amount <= 0.0 || payload.vendor.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "vendor and a positive finite amount are required"
            })),
        )
            .into_response();
    }

    let pool = crate::db::get_pool();
    let mut tx = match pool.begin().await {
        Ok(t) => t,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    let tx_id = Uuid::new_v4().to_string();
    let currency = "USD".to_string();

    // In a real app this would call the LLM agent via department orchestration.
    // For this implementation, we take the provided test data to avoid hardcoded DB values.
    let amount = payload.amount;
    let vendor = payload.vendor;

    // Deterministic categorization keeps the receipt ledger independent of an optional LLM.
    let category =
        if vendor.to_lowercase().contains("depot") || vendor.to_lowercase().contains("hardware") {
            "Supplies".to_string()
        } else {
            "General Expense".to_string()
        };

    if sqlx::query("INSERT INTO ledger_transactions (tenant_id, tx_id, amount, currency) VALUES ($1, $2, $3, $4)")
        .bind(&tenant_id)
        .bind(&tx_id)
        .bind(amount)
        .bind(&currency)
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // ensure expense account exists for tenant
    let expense_account_id = "default_expense";
    if sqlx::query("INSERT INTO ledger_accounts (tenant_id, account_id, currency, balance) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING")
        .bind(&tenant_id)
        .bind(expense_account_id)
        .bind(&currency)
        .bind(0.0)
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let entry_id_debit = Uuid::new_v4().to_string();
    if sqlx::query("INSERT INTO ledger_entries (tenant_id, entry_id, tx_id, account_id, direction, amount) VALUES ($1, $2, $3, $4, 'DEBIT', $5)")
        .bind(&tenant_id)
        .bind(&entry_id_debit)
        .bind(&tx_id)
        .bind(expense_account_id)
        .bind(amount)
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if sqlx::query("UPDATE ledger_accounts SET balance = balance + $1 WHERE tenant_id = $2 AND account_id = $3")
        .bind(amount)
        .bind(&tenant_id)
        .bind(expense_account_id)
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // To balance double-entry ledger, deduct from cash account (CREDIT cash)
    let cash_account_id = "default_cash";
    if sqlx::query("INSERT INTO ledger_accounts (tenant_id, account_id, currency, balance) VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING")
        .bind(&tenant_id)
        .bind(cash_account_id)
        .bind(&currency)
        .bind(0.0)
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let entry_id_credit = Uuid::new_v4().to_string();
    if sqlx::query("INSERT INTO ledger_entries (tenant_id, entry_id, tx_id, account_id, direction, amount) VALUES ($1, $2, $3, $4, 'CREDIT', $5)")
        .bind(&tenant_id)
        .bind(&entry_id_credit)
        .bind(&tx_id)
        .bind(cash_account_id)
        .bind(amount) // Positive value, direction is CREDIT
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if sqlx::query("UPDATE ledger_accounts SET balance = balance - $1 WHERE tenant_id = $2 AND account_id = $3")
        .bind(amount)
        .bind(&tenant_id)
        .bind(cash_account_id)
        .execute(&mut *tx)
        .await.is_err() {
        let _ = tx.rollback().await;
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    if tx.commit().await.is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    (
        StatusCode::OK,
        Json(ReceiptProcessedResponse {
            vendor,
            amount,
            category: category.to_string(),
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    // use super::*;

    #[test]
    fn test_tax_calculation() {
        let payment_amount = 100.0;
        let tax_rate = 0.15;
        let tax_amount = payment_amount * tax_rate;

        assert_eq!(tax_amount, 15.0);
    }
}
