// SPDX-License-Identifier: Apache-2.0
//! Legacy metrics, AI advisory insight, and draft-reply HTTP handlers.

use super::ui_caches::{
    ACTIVE_ORDERS_CACHE, ADVISORY_INSIGHT_CACHE, METRICS_CACHE, ORG_CACHE_ADVISORY,
};
use axum::{
    Json,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::Arc;

#[derive(serde::Serialize)]
pub struct HttpErrorResponse {
    pub error: String,
}

#[derive(serde::Deserialize)]
pub struct DraftReplyRequest {
    pub customer_message: Option<String>,
}

#[derive(serde::Serialize)]
pub struct DraftReplyResponse {
    pub output: String,
}

#[derive(serde::Deserialize)]
pub struct HttpMetricsRequest {
    pub tenant_id: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct HttpMetricsResponse {
    pub active_customers: i64,
    pub pending_orders: i64,
    pub total_sales: f64,
    pub total_campaigns_sent: i64,
    pub top_product: Option<String>,
}

pub(crate) fn dashboard_sales_query(postgres: bool) -> &'static str {
    if postgres {
        "SELECT CAST(COALESCE(SUM(total_amount), 0.0) AS DOUBLE PRECISION) FROM orders WHERE tenant_id = $1 AND LOWER(status) IN ('paid', 'completed')"
    } else {
        "SELECT CAST(COALESCE(SUM(total_amount), 0.0) AS REAL) FROM orders WHERE tenant_id = ? AND LOWER(status) IN ('paid', 'completed')"
    }
}

pub(crate) fn dashboard_top_product_query(postgres: bool) -> &'static str {
    if postgres {
        "SELECT p.title FROM products p JOIN order_items oi ON p.id = oi.product_id JOIN orders o ON oi.order_id = o.id WHERE o.tenant_id = $1 AND oi.tenant_id = $1 AND p.tenant_id = $1 AND LOWER(o.status) IN ('paid', 'completed') GROUP BY p.title ORDER BY SUM(oi.quantity) DESC, p.title ASC LIMIT 1"
    } else {
        "SELECT p.title FROM products p JOIN order_items oi ON p.id = oi.product_id JOIN orders o ON oi.order_id = o.id WHERE o.tenant_id = ? AND oi.tenant_id = ? AND p.tenant_id = ? AND LOWER(o.status) IN ('paid', 'completed') GROUP BY p.title ORDER BY SUM(oi.quantity) DESC, p.title ASC LIMIT 1"
    }
}

pub async fn http_metrics_handler(
    db: Arc<crate::db::DB>,
    store: Arc<crate::auth::Store>,
    headers: HeaderMap,
    Json(payload): Json<HttpMetricsRequest>,
) -> Response {
    let auth_header = match headers.get("authorization").and_then(|v| v.to_str().ok()) {
        Some(h) => h,
        None => return (StatusCode::UNAUTHORIZED, "Missing authorization header").into_response(),
    };

    let token = if auth_header.to_lowercase().starts_with("bearer ") {
        &auth_header[7..]
    } else {
        auth_header
    };

    let claims = match store.validate_token(token).await {
        Ok(claims) => claims,
        Err(_) => return (StatusCode::UNAUTHORIZED, "Invalid token").into_response(),
    };

    let tenant_id = payload.tenant_id;
    if tenant_id == "system" {
        return (
            StatusCode::FORBIDDEN,
            "Querying system tenant is not allowed",
        )
            .into_response();
    }
    if claims.organization_id.as_deref() != Some(&tenant_id)
        && !claims.roles.contains(&crate::auth::ROLE_ADMIN.to_string())
    {
        return (
            StatusCode::FORBIDDEN,
            "Tenant ID does not match authorization context",
        )
            .into_response();
    }

    let cache_key = format!("metrics:{}", tenant_id);
    let cache = METRICS_CACHE
        .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));
    if let Some(metrics) = cache.get(&cache_key).await {
        return (StatusCode::OK, Json(metrics)).into_response();
    }

    let pool1 = db.pool.clone();
    let pool2 = db.pool.clone();
    let pool3 = db.pool.clone();
    let pool4 = db.pool.clone();
    let pool5 = db.pool.clone();

    let t_id1 = tenant_id.to_string();
    let t_id2 = tenant_id.to_string();
    let t_id3 = tenant_id.to_string();
    let t_id4 = tenant_id.to_string();
    let t_id5 = tenant_id.to_string();

    let is_pg = matches!(db.store, crate::db::DbStore::Postgres);

    let (active_customers_res, pending_orders_res, sales_res, campaigns_res, top_product_res) = tokio::join!(
        tokio::spawn(async move {
            let query = if is_pg {
                "SELECT COUNT(*) FROM users WHERE tenant_id = $1"
            } else {
                "SELECT COUNT(*) FROM users WHERE tenant_id = ?"
            };
            sqlx::query_scalar::<_, i64>(query)
                .bind(&t_id1)
                .fetch_one(&pool1)
                .await
        }),
        tokio::spawn(async move {
            let query = if is_pg {
                "SELECT COUNT(*) FROM orders WHERE tenant_id = $1 AND status = 'pending'"
            } else {
                "SELECT COUNT(*) FROM orders WHERE tenant_id = ? AND status = 'pending'"
            };
            sqlx::query_scalar::<_, i64>(query)
                .bind(&t_id2)
                .fetch_one(&pool2)
                .await
        }),
        tokio::spawn(async move {
            let query = dashboard_sales_query(is_pg);
            sqlx::query_scalar::<_, f64>(query)
                .bind(&t_id3)
                .fetch_one(&pool3)
                .await
        }),
        tokio::spawn(async move {
            let query = if is_pg {
                "SELECT COUNT(*) FROM agent_actions WHERE tenant_id = $1 AND action_type = 'growth.campaign_sent'"
            } else {
                "SELECT COUNT(*) FROM agent_actions WHERE tenant_id = ? AND action_type = 'growth.campaign_sent'"
            };
            sqlx::query_scalar::<_, i64>(query)
                .bind(&t_id4)
                .fetch_one(&pool4)
                .await
        }),
        tokio::spawn(async move {
            let query = dashboard_top_product_query(is_pg);
            if is_pg {
                sqlx::query_scalar::<_, String>(query)
                    .bind(&t_id5)
                    .fetch_optional(&pool5)
                    .await
            } else {
                sqlx::query_scalar::<_, String>(query)
                    .bind(&t_id5)
                    .bind(&t_id5)
                    .bind(&t_id5)
                    .fetch_optional(&pool5)
                    .await
            }
        })
    );

    let metrics = HttpMetricsResponse {
        active_customers: active_customers_res.unwrap_or(Ok(0)).unwrap_or(0),
        pending_orders: pending_orders_res.unwrap_or(Ok(0)).unwrap_or(0),
        total_sales: sales_res.unwrap_or(Ok(0.0)).unwrap_or(0.0),
        total_campaigns_sent: campaigns_res.unwrap_or(Ok(0)).unwrap_or(0),
        top_product: top_product_res.unwrap_or(Ok(None)).unwrap_or(None),
    };
    cache
        .set(
            &cache_key,
            metrics.clone(),
            std::time::Duration::from_secs(60),
        )
        .await;

    (StatusCode::OK, Json(metrics)).into_response()
}

pub async fn advisory_insights_handler(
    db: Arc<crate::db::DB>,
    store: Arc<crate::auth::Store>,
    headers: HeaderMap,
) -> Response {
    let auth_header = match headers.get("authorization").and_then(|v| v.to_str().ok()) {
        Some(h) => h,
        None => return (StatusCode::UNAUTHORIZED, "Missing authorization header").into_response(),
    };

    let token = if auth_header.to_lowercase().starts_with("bearer ") {
        &auth_header[7..]
    } else {
        auth_header
    };

    let claims = match store.validate_token(token).await {
        Ok(c) => c,
        Err(_) => return (StatusCode::UNAUTHORIZED, "Invalid token").into_response(),
    };

    let tenant_id = match claims.organization_id {
        Some(id) if !id.trim().is_empty() => id,
        _ => return (StatusCode::FORBIDDEN, "Tenant ID not found in claims").into_response(),
    };

    let api_key = match std::env::var("MINIMAX_API_KEY") {
        Ok(key) if !key.trim().is_empty() => key,
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(HttpErrorResponse {
                    error: "MINIMAX_API_KEY is required".to_string(),
                }),
            )
                .into_response();
        }
    };

    let db1 = db.clone();
    let db2 = db.clone();
    let tenant_id1 = tenant_id.clone();
    let tenant_id2 = tenant_id.clone();

    let (org_res_handle, active_orders_res_handle) = tokio::join!(
        tokio::spawn(async move {
            let cache_key = format!("advisory:org:{}", tenant_id1);
            let cache = ORG_CACHE_ADVISORY.get_or_init(|| {
                ::server_utils::cache::HybridCache::new(crate::distributed_cache())
            });
            if let Some(org) = cache.get(&cache_key).await {
                return Ok(org);
            }

            let result = match &db1.store {
                crate::db::DbStore::Postgres => {
                    sqlx::query_as::<_, (String, String)>(
                        "SELECT name, COALESCE(industry, '') FROM tenants WHERE id = $1",
                    )
                    .bind(&tenant_id1)
                    .fetch_optional(&db1.pool)
                    .await
                }
                crate::db::DbStore::Sqlite(pool) => {
                    sqlx::query_as::<_, (String, String)>(
                        "SELECT name, COALESCE(industry, '') FROM tenants WHERE id = ?",
                    )
                    .bind(&tenant_id1)
                    .fetch_optional(pool)
                    .await
                }
            };

            if let Ok(ref org) = result {
                cache
                    .set(
                        &cache_key,
                        org.clone(),
                        std::time::Duration::from_secs(3600),
                    )
                    .await;
            }
            result
        }),
        tokio::spawn(async move {
            let cache_key = format!("advisory:orders:{}", tenant_id2);
            let cache = ACTIVE_ORDERS_CACHE.get_or_init(|| {
                ::server_utils::cache::HybridCache::new(crate::distributed_cache())
            });
            if let Some(orders) = cache.get(&cache_key).await {
                return Ok(orders);
            }

            let result = match &db2.store {
                crate::db::DbStore::Postgres => sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM orders WHERE tenant_id = $1 AND status != 'delivered'",
                )
                .bind(&tenant_id2)
                .fetch_one(&db2.pool)
                .await,
                crate::db::DbStore::Sqlite(pool) => sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM orders WHERE tenant_id = $1 AND status != 'delivered'",
                )
                .bind(&tenant_id2)
                .fetch_one(pool)
                .await,
            };

            if let Ok(orders) = result {
                cache
                    .set(&cache_key, orders, std::time::Duration::from_secs(5))
                    .await;
            }
            result
        })
    );

    let org_data = org_res_handle.unwrap_or_else(|_| Err(sqlx::Error::RowNotFound));
    let orders_data = active_orders_res_handle.unwrap_or_else(|_| Err(sqlx::Error::RowNotFound));

    let (business_name, industry) = org_data
        .unwrap_or(None)
        .unwrap_or_else(|| ("A business".to_string(), "".to_string()));

    let active_orders = orders_data.unwrap_or(0);

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(format!("{}:{}:{}", business_name, industry, active_orders).as_bytes());
    let stats_hash = format!("{:x}", hasher.finalize());
    let insight_cache_key = format!("advisory:insight:{}:{}", tenant_id, stats_hash);

    let insight_cache = ADVISORY_INSIGHT_CACHE
        .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));
    if let Some(insight) = insight_cache.get(&insight_cache_key).await {
        return (
            StatusCode::OK,
            Json(serde_json::json!({ "summary": insight })),
        )
            .into_response();
    }

    let prompt = format!(
        "You are a business advisory agent. Business context: A {} business named {}. The business currently has {} active orders to fulfill. Provide a short, plain language insight (about 2 sentences) summarizing this performance and suggesting an actionable next step, like running a promo or checking the inbox. Make it warm and accessible.",
        industry, business_name, active_orders
    );
    let compressed_prompt = ::ohc_core::prompt::compression::reduce_tokens(&prompt);

    let client = crate::minimax::MinimaxClient::new(api_key);
    match client.reason(&compressed_prompt).await {
        Ok(output) => {
            insight_cache
                .set(
                    &insight_cache_key,
                    output.clone(),
                    std::time::Duration::from_secs(300),
                )
                .await;
            (
                StatusCode::OK,
                Json(serde_json::json!({ "summary": output })),
            )
                .into_response()
        }
        Err(e) => {
            ::server_telemetry::record_error_signal("[bug] MiniMax advisory insights failed");
            tracing::error!("MiniMax advisory insights failed: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(HttpErrorResponse {
                    error: "AI advisory generation failed".to_string(),
                }),
            )
                .into_response()
        }
    }
}

pub async fn draft_reply_handler(
    db: Arc<crate::db::DB>,
    store: Arc<crate::auth::Store>,
    headers: HeaderMap,
    payload: DraftReplyRequest,
) -> Response {
    let auth_header = match headers.get("authorization").and_then(|v| v.to_str().ok()) {
        Some(h) => h,
        None => return (StatusCode::UNAUTHORIZED, "Missing authorization header").into_response(),
    };

    let token = if auth_header.to_lowercase().starts_with("bearer ") {
        &auth_header[7..]
    } else {
        auth_header
    };

    let claims = match store.validate_token(token).await {
        Ok(claims) => claims,
        Err(_) => return (StatusCode::UNAUTHORIZED, "Invalid token").into_response(),
    };

    let tenant_id = match claims.organization_id {
        Some(id) if !id.trim().is_empty() => id,
        _ => return (StatusCode::FORBIDDEN, "Tenant ID not found in claims").into_response(),
    };

    let api_key = match std::env::var("MINIMAX_API_KEY") {
        Ok(key) if !key.trim().is_empty() => key,
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(HttpErrorResponse {
                    error: "MINIMAX_API_KEY is required".to_string(),
                }),
            )
                .into_response();
        }
    };

    let (business_name, industry): (String, String) =
        sqlx::query_as("SELECT name, COALESCE(industry, '') FROM tenants WHERE id = $1")
            .bind(&tenant_id)
            .fetch_optional(&db.pool)
            .await
            .unwrap_or(None)
            .unwrap_or_else(|| ("A business".to_string(), "".to_string()));

    let customer_message = payload
        .customer_message
        .unwrap_or_else(|| "Do you have vegan options for birthday cakes?".to_string());

    let business_context = if industry.is_empty() {
        format!("A business named {}", business_name)
    } else {
        format!("A {} business named {}", industry, business_name)
    };

    let prompt = format!(
        "Write one concise, warm customer-service reply. Business context: {} Customer message: {}",
        business_context, customer_message
    );
    let compressed_prompt = ::ohc_core::prompt::compression::reduce_tokens(&prompt);

    let client = crate::minimax::MinimaxClient::new(api_key);
    match client.reason(&compressed_prompt).await {
        Ok(output) => (StatusCode::OK, Json(DraftReplyResponse { output })).into_response(),
        Err(e) => {
            ::server_telemetry::record_error_signal("[bug] MiniMax draft reply failed");
            tracing::error!("MiniMax draft reply failed: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                Json(HttpErrorResponse {
                    error: "AI draft generation failed".to_string(),
                }),
            )
                .into_response()
        }
    }
}
