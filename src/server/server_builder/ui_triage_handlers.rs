// SPDX-License-Identifier: Apache-2.0
//! UI triage, inbox action, dashboard metrics, and analytics handlers.

fn dashboard_top_product_query(postgres: bool) -> &'static str {
    if postgres {
        "SELECT p.title FROM products p JOIN order_items oi ON p.id = oi.product_id JOIN orders o ON oi.order_id = o.id WHERE o.tenant_id = $1 AND oi.tenant_id = $1 AND p.tenant_id = $1 AND LOWER(o.status) IN ('paid', 'completed') GROUP BY p.title ORDER BY SUM(oi.quantity) DESC, p.title ASC LIMIT 1"
    } else {
        "SELECT p.title FROM products p JOIN order_items oi ON p.id = oi.product_id JOIN orders o ON oi.order_id = o.id WHERE o.tenant_id = ? AND oi.tenant_id = ? AND p.tenant_id = ? AND LOWER(o.status) IN ('paid', 'completed') GROUP BY p.title ORDER BY SUM(oi.quantity) DESC, p.title ASC LIMIT 1"
    }
}

pub async fn create_ui_triage_item_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::extract::Json(payload): axum::extract::Json<super::ui_caches::CreateTriageItemPayload>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    match &db.store {
        crate::db::DbStore::Postgres => {
            let mut tx = match db.pool.begin().await {
                Ok(tx) => tx,
                Err(e) => {
                    tracing::error!("Failed to begin transaction: {:?}", e);
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(serde_json::json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
            };
            if let Err(e) = ::server_common::auth_utils::set_org_context(&mut *tx, &tenant_id).await
            {
                tracing::error!("Failed to set org context: {:?}", e);
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response();
            }

            let new_id = format!("triage-{}", uuid::Uuid::new_v4());
            let source = payload.source.unwrap_or_else(|| "Unknown".to_string());
            let priority = payload.priority.unwrap_or_else(|| "normal".to_string());
            let context = payload.context.unwrap_or_else(|| "".to_string());

            if let Err(e) = sqlx::query(
            "INSERT INTO triage_items (id, tenant_id, customer_id, source, priority, context, status) VALUES ($1, $2, $3, $4, $5, $6, 'pending')"
        )
        .bind(&new_id)
        .bind(&tenant_id)
        .bind(&payload.customer_id)
        .bind(&source)
        .bind(&priority)
        .bind(&context)
        .execute(&mut *tx).await {
            tracing::error!("Failed to insert triage item: {:?}", e);
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({"error": e.to_string()}))).into_response();
        }

            if let Some(action_type) = payload.action_type {
                let action_id = format!("act-{}", uuid::Uuid::new_v4());
                if let Err(e) = sqlx::query(
                "INSERT INTO triage_proposed_actions (id, triage_item_id, tenant_id, action_type, payload) VALUES ($1, $2, $3, $4, $5)"
            )
            .bind(&action_id)
            .bind(&new_id)
            .bind(&tenant_id)
            .bind(&action_type)
            .bind(&payload.action_payload)
            .execute(&mut *tx).await {
                tracing::error!("Failed to insert triage action: {:?}", e);
                return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({"error": e.to_string()}))).into_response();
            }
            }

            if let Err(e) = tx.commit().await {
                tracing::error!("Failed to commit transaction: {:?}", e);
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response();
            }

            let cache = super::ui_caches::UI_TRIAGE_CACHE.get_or_init(|| {
                ::server_utils::cache::HybridCache::new(crate::distributed_cache())
            });
            cache
                .invalidate(&format!("ui_triage:{}:mobile:false", tenant_id))
                .await;
            cache
                .invalidate(&format!("ui_triage:{}:mobile:true", tenant_id))
                .await;

            (
                axum::http::StatusCode::CREATED,
                axum::Json(serde_json::json!({"id": new_id, "status": "success"})),
            )
                .into_response()
        }
        crate::db::DbStore::Sqlite(sqlite_pool) => {
            let mut tx = match sqlite_pool.begin().await {
                Ok(tx) => tx,
                Err(e) => {
                    tracing::error!("Failed to begin sqlite transaction: {:?}", e);
                    return (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        axum::Json(serde_json::json!({"error": e.to_string()})),
                    )
                        .into_response();
                }
            };

            let new_id = format!("triage-{}", uuid::Uuid::new_v4());
            let source = payload.source.unwrap_or_else(|| "Unknown".to_string());
            let priority = payload.priority.unwrap_or_else(|| "normal".to_string());
            let context = payload.context.unwrap_or_else(|| "".to_string());

            if let Err(e) = sqlx::query(
            "INSERT INTO triage_items (id, tenant_id, customer_id, source, priority, context, status) VALUES (?, ?, ?, ?, ?, ?, 'pending')"
        )
        .bind(&new_id)
        .bind(&tenant_id)
        .bind(&payload.customer_id)
        .bind(&source)
        .bind(&priority)
        .bind(&context)
        .execute(&mut *tx).await {
            tracing::error!("Failed to insert triage item (sqlite): {:?}", e);
            return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({"error": e.to_string()}))).into_response();
        }

            if let Some(action_type) = payload.action_type {
                let action_id = format!("act-{}", uuid::Uuid::new_v4());
                if let Err(e) = sqlx::query(
                "INSERT INTO triage_proposed_actions (id, triage_item_id, tenant_id, action_type, payload) VALUES (?, ?, ?, ?, ?)"
            )
            .bind(&action_id)
            .bind(&new_id)
            .bind(&tenant_id)
            .bind(&action_type)
            .bind(&payload.action_payload)
            .execute(&mut *tx).await {
                tracing::error!("Failed to insert triage action (sqlite): {:?}", e);
                return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({"error": e.to_string()}))).into_response();
            }
            }

            if let Err(e) = tx.commit().await {
                tracing::error!("Failed to commit sqlite transaction: {:?}", e);
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response();
            }

            let cache = super::ui_caches::UI_TRIAGE_CACHE.get_or_init(|| {
                ::server_utils::cache::HybridCache::new(crate::distributed_cache())
            });
            cache
                .invalidate(&format!("ui_triage:{}:mobile:false", tenant_id))
                .await;
            cache
                .invalidate(&format!("ui_triage:{}:mobile:true", tenant_id))
                .await;

            (
                axum::http::StatusCode::CREATED,
                axum::Json(serde_json::json!({"id": new_id, "status": "success"})),
            )
                .into_response()
        }
    }
}

pub async fn list_ui_triage_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::extract::Query(query): axum::extract::Query<crate::common::auth_utils::UiTenantQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let mobile_optimized = query.mobile_optimized.unwrap_or(false);

    let cache_key = format!("ui_triage:{}:mobile:{}", tenant_id, mobile_optimized);
    let cache = super::ui_caches::UI_TRIAGE_CACHE
        .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));

    if query.bypass_cache.unwrap_or(false) {
        cache.invalidate(&cache_key).await;
    }

    let items_opt = cache
        .get_or_fetch_with_swr(&cache_key, std::time::Duration::from_secs(10), {
            let db = db.clone();
            let t = tenant_id.clone();
            move || async move {
                match super::ui_triage_feed_readers::load_ui_triage_from_db(
                    &db,
                    &t,
                    mobile_optimized,
                )
                .await
                {
                    Ok(items) => Some(items),
                    Err(sqlx::Error::RowNotFound) => Some(vec![]),
                    Err(_) => None,
                }
            }
        })
        .await;

    match items_opt {
        Some(items) => {
            let fields = query.fields.as_deref();
            let shaped = ::server_utils::payload_shaper::shape_payload(
                serde_json::to_value(items).unwrap_or_default(),
                fields,
            );
            (axum::http::StatusCode::OK, axum::Json(shaped)).into_response()
        }
        None => {
            tracing::error!("Failed to fetch triage items");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(Vec::<serde_json::Value>::new()),
            )
                .into_response()
        }
    }
}

pub async fn list_ui_omni_inbox_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::extract::Query(query): axum::extract::Query<crate::common::auth_utils::UiTenantQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let tenant_id = match super::ui_caches::strict_ui_claim_tenant(&claims) {
        Some(tenant_id) => tenant_id,
        None => return axum::http::StatusCode::UNAUTHORIZED.into_response(),
    };
    let mobile_optimized = query.mobile_optimized.unwrap_or(false);

    let cache_key = format!("ui_omni_inbox:{}:mobile:{}", tenant_id, mobile_optimized);
    let cache = super::ui_caches::UI_OMNI_INBOX_CACHE
        .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));

    let items_opt = cache
        .get_or_fetch_with_swr(&cache_key, std::time::Duration::from_secs(10), {
            let db = db.clone();
            let t = tenant_id.clone();
            move || async move {
                match super::ui_caches::load_ui_omni_inbox_from_db(&db, &t, mobile_optimized).await
                {
                    Ok(items) => Some(items),
                    Err(sqlx::Error::RowNotFound) => Some(vec![]),
                    Err(_) => None,
                }
            }
        })
        .await;

    match items_opt {
        Some(items) => {
            let fields = query.fields.as_deref();
            let shaped = ::server_utils::payload_shaper::shape_payload(
                serde_json::to_value(items).unwrap_or_default(),
                fields,
            );
            (axum::http::StatusCode::OK, axum::Json(shaped)).into_response()
        }
        None => {
            tracing::error!("Failed to fetch omni inbox items");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(Vec::<serde_json::Value>::new()),
            )
                .into_response()
        }
    }
}

pub async fn update_ui_omni_inbox_action_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::extract::Json(payload): axum::extract::Json<super::ui_caches::OmniInboxActionPayload>,
) -> axum::response::Response {
    use crate::orchestration::departments::message_delivery::{Error, Store, manual_inbox};
    use axum::response::IntoResponse;
    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let result = manual_inbox::apply(
        &Store::from_db(db.as_ref()),
        &tenant_id,
        &claims.sub,
        &payload,
    )
    .await;
    // A lost response/finalization may still have a durable unknown claim.
    super::ui_caches::invalidate_ui_omni_inbox_cache(&tenant_id).await;
    match result {
        Ok(receipt) => (
            axum::http::StatusCode::OK,
            [("cache-control", "private, no-store")],
            axum::Json(receipt),
        )
            .into_response(),
        Err(error) => {
            let status = match &error {
                Error::Storage => axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Error::Invalid(_) => axum::http::StatusCode::CONFLICT,
            };
            (
                status,
                [("cache-control", "private, no-store")],
                axum::Json(serde_json::json!({"error":error.to_string()})),
            )
                .into_response()
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualInboxReadQuery {
    pub message_id: String,
    pub request_id: Option<String>,
}
pub async fn read_ui_omni_inbox_action_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::extract::Query(query): axum::extract::Query<ManualInboxReadQuery>,
) -> axum::response::Response {
    use crate::orchestration::departments::message_delivery::{Store, manual_inbox};
    use axum::response::IntoResponse;
    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    match manual_inbox::read(&Store::from_db(db.as_ref()), &tenant_id, &claims.sub, &query.message_id, query.request_id.as_deref()).await {
        Ok(Some(receipt)) => (axum::http::StatusCode::OK, [("cache-control", "private, no-store")], axum::Json(receipt)).into_response(),
        Ok(None) => axum::http::StatusCode::NOT_FOUND.into_response(),
        Err(_) => (axum::http::StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({"error":"Saved reply status unavailable; do not resend until reconciled"}))).into_response(),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct UiDashboardMetrics {
    active_customers: i64,
    pending_orders: i64,
    total_sales: f64,
    total_campaigns_sent: i64,
    auto_replied: i64,
    top_product: Option<String>,
}

pub(crate) async fn load_ui_dashboard_metrics(
    db: &crate::db::DB,
    tenant_id: &str,
    _mobile_optimized: bool,
) -> Result<UiDashboardMetrics, sqlx::Error> {
    match &db.store {
        crate::db::DbStore::Postgres => {
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

            let (c_res, orders_res, cs_res, ar_res, top_product_res) = tokio::join!(
                tokio::spawn(async move {
                    sqlx::query_scalar::<_, i64>(
                        "SELECT COUNT(*) FROM customers WHERE tenant_id = $1",
                    )
                    .bind(&t_id1)
                    .fetch_one(&pool1)
                    .await
                }),
                tokio::spawn(async move {
                    sqlx::query_as::<_, (Option<i64>, Option<f64>)>("SELECT CAST(SUM(CASE WHEN LOWER(status) = 'pending' THEN 1 ELSE 0 END) AS BIGINT), CAST(COALESCE(SUM(CASE WHEN LOWER(status) IN ('paid', 'completed') THEN total_amount ELSE 0 END), 0.0) AS DOUBLE PRECISION) FROM orders WHERE tenant_id = $1").bind(&t_id2).fetch_one(&pool2).await
                }),
                tokio::spawn(async move {
                    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_actions WHERE tenant_id = $1 AND action_type = 'growth.campaign_sent'").bind(&t_id3).fetch_one(&pool3).await
                }),
                tokio::spawn(async move {
                    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM inbox_messages WHERE tenant_id = $1 AND status = 'auto_replied'").bind(&t_id4).fetch_one(&pool4).await
                }),
                tokio::spawn(async move {
                    sqlx::query_scalar::<_, String>(dashboard_top_product_query(true))
                        .bind(&t_id5)
                        .fetch_optional(&pool5)
                        .await
                })
            );
            let customers = c_res.map_err(|_| sqlx::Error::RowNotFound)??;
            let orders = orders_res.map_err(|_| sqlx::Error::RowNotFound)??;
            let campaigns = cs_res.map_err(|_| sqlx::Error::RowNotFound)??;
            let auto_replied = ar_res.map_err(|_| sqlx::Error::RowNotFound)??;
            let top_product = top_product_res.map_err(|_| sqlx::Error::RowNotFound)??;
            Ok(UiDashboardMetrics {
                active_customers: customers,
                pending_orders: orders.0.unwrap_or(0),
                total_sales: orders.1.unwrap_or(0.0),
                total_campaigns_sent: campaigns,
                auto_replied,
                top_product,
            })
        }
        crate::db::DbStore::Sqlite(pool) => {
            let pool1 = pool.clone();
            let pool2 = pool.clone();
            let pool3 = pool.clone();
            let pool4 = pool.clone();
            let pool5 = pool.clone();

            let t_id1 = tenant_id.to_string();
            let t_id2 = tenant_id.to_string();
            let t_id3 = tenant_id.to_string();
            let t_id4 = tenant_id.to_string();
            let t_id5 = tenant_id.to_string();

            let (c_res, orders_res, cs_res, ar_res, top_product_res) = tokio::join!(
                tokio::spawn(async move {
                    sqlx::query_scalar::<_, i64>(
                        "SELECT COUNT(*) FROM customers WHERE tenant_id = ?",
                    )
                    .bind(&t_id1)
                    .fetch_one(&pool1)
                    .await
                }),
                tokio::spawn(async move {
                    sqlx::query_as::<_, (Option<i64>, Option<f64>)>("SELECT CAST(SUM(CASE WHEN LOWER(status) = 'pending' THEN 1 ELSE 0 END) AS INTEGER), CAST(COALESCE(SUM(CASE WHEN LOWER(status) IN ('paid', 'completed') THEN total_amount ELSE 0 END), 0.0) AS REAL) FROM orders WHERE tenant_id = ?").bind(&t_id2).fetch_one(&pool2).await
                }),
                tokio::spawn(async move {
                    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM agent_actions WHERE tenant_id = ? AND action_type = 'growth.campaign_sent'").bind(&t_id3).fetch_one(&pool3).await
                }),
                tokio::spawn(async move {
                    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM inbox_messages WHERE tenant_id = ? AND status = 'auto_replied'").bind(&t_id4).fetch_one(&pool4).await
                }),
                tokio::spawn(async move {
                    sqlx::query_scalar::<_, String>(dashboard_top_product_query(false))
                        .bind(&t_id5)
                        .bind(&t_id5)
                        .bind(&t_id5)
                        .fetch_optional(&pool5)
                        .await
                })
            );
            let customers = c_res.map_err(|_| sqlx::Error::RowNotFound)??;
            let orders = orders_res.map_err(|_| sqlx::Error::RowNotFound)??;
            let campaigns = cs_res.map_err(|_| sqlx::Error::RowNotFound)??;
            let auto_replied = ar_res.map_err(|_| sqlx::Error::RowNotFound)??;
            let top_product = top_product_res.map_err(|_| sqlx::Error::RowNotFound)??;
            Ok(UiDashboardMetrics {
                active_customers: customers,
                pending_orders: orders.0.unwrap_or(0),
                total_sales: orders.1.unwrap_or(0.0),
                total_campaigns_sent: campaigns,
                auto_replied,
                top_product,
            })
        }
    }
}

pub(crate) async fn ui_dashboard_analytics_briefing_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::extract::Query(query): axum::extract::Query<crate::common::auth_utils::UiTenantQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let mobile_optimized = query.mobile_optimized.unwrap_or(false);

    let cache_key = format!(
        "ui_analytics_briefing:{}:mobile:{}",
        tenant_id, mobile_optimized
    );
    let cache = super::ui_caches::UI_ANALYTICS_BRIEFING_CACHE
        .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));

    let result_opt = cache
        .get_or_fetch_with_swr(&cache_key, std::time::Duration::from_secs(60), {
            let db = db.clone();
            let t = tenant_id.clone();
            move || async move {
                let db1 = db.clone();
                let db2 = db.clone();
                let tenant_id1 = t.clone();
                let tenant_id2 = t.clone();

                let (metrics_res, inbox_res) = tokio::join!(
                    tokio::spawn(async move {
                        load_ui_dashboard_metrics(&db1, &tenant_id1, mobile_optimized).await
                    }),
                    tokio::spawn(async move {
                        super::ui_inbox_ledger_readers::load_ui_inbox_from_db(
                            &db2,
                            &tenant_id2,
                            mobile_optimized,
                        )
                        .await
                    })
                );

                let metrics = metrics_res
                    .unwrap_or_else(|_| Err(sqlx::Error::RowNotFound))
                    .unwrap_or(UiDashboardMetrics {
                        active_customers: 0,
                        pending_orders: 0,
                        total_sales: 0.0,
                        total_campaigns_sent: 0,
                        auto_replied: 0,
                        top_product: None,
                    });
                let inbox_messages = inbox_res
                    .unwrap_or_else(|_| Err(sqlx::Error::RowNotFound))
                    .unwrap_or_default();
                let unanswered_dms = inbox_messages
                    .iter()
                    .filter(|m| m.get("status").and_then(|s| s.as_str()).unwrap_or("") != "closed")
                    .count();

                let total_sales_formatted = format!("${:.2}", metrics.total_sales);
                let summary = format!(
                    "Good morning. You have {} pending orders totaling {}, and {} unanswered DMs.",
                    metrics.pending_orders, total_sales_formatted, unanswered_dms
                );

                Some(serde_json::json!({ "briefing": summary }))
            }
        })
        .await;

    let result = result_opt.unwrap_or_else(
        || serde_json::json!({ "briefing": "Good morning. Unable to fetch your briefing." }),
    );
    let fields = query.fields.as_deref();
    let shaped = ::server_utils::payload_shaper::shape_payload(result, fields);

    (axum::http::StatusCode::OK, axum::Json(shaped)).into_response()
}

#[derive(serde::Deserialize)]
pub(crate) struct AnalyticsChatRequest {
    message: String,
}

pub(crate) async fn ui_dashboard_analytics_chat_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::extract::Query(query): axum::extract::Query<crate::common::auth_utils::UiTenantQuery>,
    axum::Json(payload): axum::Json<AnalyticsChatRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let mobile_optimized = query.mobile_optimized.unwrap_or(false);
    let text = payload.message.to_lowercase();

    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    let text_hash = hasher.finish();

    let cache_key = format!(
        "ui_analytics_chat:{}:mobile:{}:{}",
        tenant_id, mobile_optimized, text_hash
    );
    let cache = super::ui_caches::UI_ANALYTICS_CHAT_CACHE
        .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));

    let result_opt = cache.get_or_fetch_with_swr(&cache_key, std::time::Duration::from_secs(60), {
    let db = db.clone();
    let t = tenant_id.clone();
    let text_bg = text.clone();
    move || async move {
        let db1 = db.clone(); let db2 = db.clone();
        let tenant_id1 = t.clone(); let tenant_id2 = t.clone();

        let (inbox_res_handle, metrics_res_handle) = tokio::join!(
            tokio::spawn(async move { super::ui_inbox_ledger_readers::load_ui_inbox_from_db(&db1, &tenant_id1, mobile_optimized).await }),
            tokio::spawn(async move { load_ui_dashboard_metrics(&db2, &tenant_id2, mobile_optimized).await })
        );

        let inbox_res = inbox_res_handle.unwrap_or_else(|_| Err(sqlx::Error::RowNotFound));
        let metrics_res = metrics_res_handle.unwrap_or_else(|_| Err(sqlx::Error::RowNotFound));

        let response_text = if text_bg.contains("dm") || text_bg.contains("message") {
            let inbox_messages = inbox_res.unwrap_or_default();
            let senders: Vec<String> = inbox_messages.iter().take(3).filter_map(|m| m.get("source").and_then(|s| s.as_str()).map(|s| s.to_string())).collect();
            if senders.is_empty() {
                "You have no recent messages.".to_string()
            } else {
                format!("Your latest messages are from: {}.", senders.join(", "))
            }
        } else if text_bg.contains("order") || text_bg.contains("booking") || text_bg.contains("revenue") || text_bg.contains("sale") {
            let metrics = metrics_res.unwrap_or(UiDashboardMetrics { active_customers: 0, pending_orders: 0, total_sales: 0.0, total_campaigns_sent: 0, auto_replied: 0, top_product: None });
            format!("You have {} pending orders. Total sales are ${:.2}.", metrics.pending_orders, metrics.total_sales)
        } else {
            "I am your Decision Assistant. I can help you check orders, messages, and revenue.".to_string()
        };

        Some(serde_json::json!({ "reply": response_text }))
    }
}).await;

    let result = result_opt.unwrap_or_else(
        || serde_json::json!({ "reply": "I'm having trouble retrieving your analytics." }),
    );
    let fields = query.fields.as_deref();
    let shaped = ::server_utils::payload_shaper::shape_payload(result, fields);

    (axum::http::StatusCode::OK, axum::Json(shaped)).into_response()
}
