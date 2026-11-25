// SPDX-License-Identifier: Apache-2.0
//! UI priority tasks, bookings, inbox, metrics, and supply handlers.

use sqlx::Row;

pub async fn list_ui_priority_tasks_handler(
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
        "ui_priority_tasks:{}:mobile:{}",
        tenant_id, mobile_optimized
    );
    let cache = super::ui_caches::UI_PRIORITY_TASKS_CACHE
        .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));

    let items_opt = cache
        .get_or_fetch_with_swr(&cache_key, std::time::Duration::from_secs(10), {
            let db = db.clone();
            let t = tenant_id.clone();
            move || async move {
                super::ui_triage_feed_readers::load_ui_priority_tasks_from_db(
                    &db,
                    &t,
                    mobile_optimized,
                )
                .await
                .ok()
            }
        })
        .await;

    match items_opt {
        Some(tasks) => {
            let fields = query.fields.as_deref();
            let shaped = ::server_utils::payload_shaper::shape_payload(
                serde_json::to_value(tasks).unwrap_or_default(),
                fields,
            );
            (axum::http::StatusCode::OK, axum::Json(shaped)).into_response()
        }
        None => {
            tracing::error!("Failed to fetch UI priority tasks");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!([])),
            )
                .into_response()
        }
    }
}

pub(crate) async fn load_ui_invoices_from_db(
    db: &crate::db::DB,
    tenant_id: &str,
    mobile_optimized: bool,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    match &db.store {
        crate::db::DbStore::Postgres => {
            let mut tx = db.pool.begin().await?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id).await?;
            let res = if mobile_optimized {
                sqlx::query("SELECT id, CAST(COALESCE(total_amount, 0.0) AS DOUBLE PRECISION) AS total_amount, COALESCE(status, '') AS status FROM invoices WHERE tenant_id = $1 AND status != 'paid' ORDER BY created_at DESC LIMIT 50")
                .bind(tenant_id)
                .fetch_all(&mut *tx)
                .await.map(|rows| rows.into_iter().map(|row| {
                    use sqlx::Row;
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "total_amount": row.get::<f64, _>("total_amount"),
                        "status": row.get::<String, _>("status"),
                    })
                }).collect())
            } else {
                sqlx::query("SELECT id, COALESCE(client_name, '') AS customer_name, CAST(COALESCE(total_amount, 0.0) AS DOUBLE PRECISION) AS total_amount, COALESCE(status, '') AS status, COALESCE(created_at::text, '') AS created_at FROM invoices WHERE tenant_id = $1 AND status != 'paid' ORDER BY created_at DESC LIMIT 50")
                .bind(tenant_id)
                .fetch_all(&mut *tx)
                .await.map(|rows| rows.into_iter().map(|row| {
                    use sqlx::Row;
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "customer_name": row.get::<String, _>("customer_name"),
                        "total_amount": row.get::<f64, _>("total_amount"),
                        "status": row.get::<String, _>("status"),
                        "created_at": row.get::<String, _>("created_at")
                    })
                }).collect())
            };
            tx.commit().await?;
            res
        }
        crate::db::DbStore::Sqlite(pool) => {
            if mobile_optimized {
                sqlx::query("SELECT id, CAST(COALESCE(total_amount, 0.0) AS REAL) AS total_amount, COALESCE(status, '') AS status FROM invoices WHERE tenant_id = ? AND status != 'paid' ORDER BY created_at DESC LIMIT 50")
                .bind(tenant_id)
                .fetch_all(pool)
                .await.map(|rows| rows.into_iter().map(|row| {
                    use sqlx::Row;
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "total_amount": row.get::<f64, _>("total_amount"),
                        "status": row.get::<String, _>("status"),
                    })
                }).collect())
            } else {
                sqlx::query("SELECT id, COALESCE(client_name, '') AS customer_name, CAST(COALESCE(total_amount, 0.0) AS REAL) AS total_amount, COALESCE(status, '') AS status, CAST(created_at AS TEXT) AS created_at FROM invoices WHERE tenant_id = ? AND status != 'paid' ORDER BY created_at DESC LIMIT 50")
                .bind(tenant_id)
                .fetch_all(pool)
                .await.map(|rows| rows.into_iter().map(|row| {
                    use sqlx::Row;
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "customer_name": row.get::<String, _>("customer_name"),
                        "total_amount": row.get::<f64, _>("total_amount"),
                        "status": row.get::<String, _>("status"),
                        "created_at": row.get::<String, _>("created_at")
                    })
                }).collect())
            }
        }
    }
}

pub(crate) async fn load_ui_bookings_from_db(
    db: &crate::db::DB,
    tenant_id: &str,
    mobile_optimized: bool,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    match &db.store {
        crate::db::DbStore::Postgres => {
            let mut tx = db.pool.begin().await?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id).await?;
            let query_str = if mobile_optimized {
                "SELECT b.id, COALESCE(p.title, '') as product_title, b.start_time, COALESCE(b.status, '') AS status \
             FROM bookings b \
             LEFT JOIN products p ON p.id = b.service_id AND p.tenant_id = b.tenant_id \
             WHERE b.tenant_id = $1 ORDER BY b.start_time ASC LIMIT 50"
            } else {
                "SELECT b.id, COALESCE(c.name, '') AS customer_name, COALESCE(b.service_id, '') AS service_id, COALESCE(p.title, '') as product_title, b.start_time, b.end_time, COALESCE(b.status, '') AS status \
             FROM bookings b LEFT JOIN customers c ON c.id = b.customer_id AND c.tenant_id = b.tenant_id \
             LEFT JOIN products p ON p.id = b.service_id AND p.tenant_id = b.tenant_id \
             WHERE b.tenant_id = $1 ORDER BY b.start_time ASC LIMIT 50"
            };
            let res = match sqlx::query(query_str)
                .bind(tenant_id)
                .fetch_all(&mut *tx)
                .await {
                    Ok(rows) => Ok(rows.into_iter().map(|row| {
                        let ai_summary = format!("AI Brief: Upcoming {} session. Previous interaction noted.", row.get::<String, _>("product_title"));
                        if mobile_optimized {
                            serde_json::json!({
                                "id": row.get::<String, _>("id"),
                                "product_title": row.get::<String, _>("product_title"),
                                "start_time": row.try_get::<chrono::DateTime<chrono::Utc>, _>("start_time").map(|d| d.to_rfc3339()).unwrap_or_default(),
                                "status": row.get::<String, _>("status"),
                                "ai_summary": ai_summary,
                            })
                        } else {
                            serde_json::json!({
                                "id": row.get::<String, _>("id"),
                                "customer_name": row.get::<String, _>("customer_name"),
                                "service_id": row.get::<String, _>("service_id"),
                                "product_title": row.get::<String, _>("product_title"),
                                "start_time": row.try_get::<chrono::DateTime<chrono::Utc>, _>("start_time").map(|d| d.to_rfc3339()).unwrap_or_default(),
                                "end_time": row.try_get::<chrono::DateTime<chrono::Utc>, _>("end_time").map(|d| d.to_rfc3339()).unwrap_or_default(),
                                "status": row.get::<String, _>("status"),
                                "ai_summary": ai_summary,
                            })
                        }
                    }).collect::<Vec<_>>()),
                    Err(e) => Err(e),
                };
            tx.commit().await?;
            res
        }
        crate::db::DbStore::Sqlite(pool) => {
            let query_str = if mobile_optimized {
                "SELECT b.id, COALESCE(p.title, '') as product_title, b.start_time, COALESCE(b.status, '') AS status \
             FROM bookings b \
             LEFT JOIN products p ON p.id = b.service_id AND p.tenant_id = b.tenant_id \
             WHERE b.tenant_id = ? ORDER BY b.start_time ASC LIMIT 50"
            } else {
                "SELECT b.id, COALESCE(c.name, '') AS customer_name, COALESCE(b.service_id, '') AS service_id, COALESCE(p.title, '') as product_title, b.start_time, b.end_time, COALESCE(b.status, '') AS status \
             FROM bookings b LEFT JOIN customers c ON c.id = b.customer_id AND c.tenant_id = b.tenant_id \
             LEFT JOIN products p ON p.id = b.service_id AND p.tenant_id = b.tenant_id \
             WHERE b.tenant_id = ? ORDER BY b.start_time ASC LIMIT 50"
            };
            match sqlx::query(query_str)
        .bind(tenant_id)
        .fetch_all(pool)
        .await {
            Ok(rows) => Ok(rows.into_iter().map(|row| {
                let ai_summary = format!("AI Brief: Upcoming {} session. Previous interaction noted.", row.get::<String, _>("product_title"));
                if mobile_optimized {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "product_title": row.get::<String, _>("product_title"),
                        "start_time": row.try_get::<String, _>("start_time").unwrap_or_default(),
                        "status": row.get::<String, _>("status"),
                        "ai_summary": ai_summary,
                    })
                } else {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "customer_name": row.get::<String, _>("customer_name"),
                        "service_id": row.get::<String, _>("service_id"),
                        "product_title": row.get::<String, _>("product_title"),
                        "start_time": row.try_get::<String, _>("start_time").unwrap_or_default(),
                        "end_time": row.try_get::<String, _>("end_time").unwrap_or_default(),
                        "status": row.get::<String, _>("status"),
                    })
                }
            }).collect::<Vec<_>>()),
            Err(e) => Err(e),
        }
        }
    }
}

pub(crate) async fn list_ui_bookings_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::extract::Query(query): axum::extract::Query<crate::common::auth_utils::UiTenantQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let mobile_optimized = query.mobile_optimized.unwrap_or(false);

    let cache_key = format!("ui_bookings:{}:mobile:{}", tenant_id, mobile_optimized);
    let cache = super::ui_caches::UI_BOOKINGS_CACHE
        .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));

    let items_opt = cache
        .get_or_fetch_with_swr(&cache_key, std::time::Duration::from_secs(60), {
            let db = db.clone();
            let t = tenant_id.clone();
            move || async move {
                match load_ui_bookings_from_db(&db, &t, mobile_optimized).await {
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
            tracing::error!("Failed to fetch ui bookings");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": "Failed to fetch ui bookings"})),
            )
                .into_response()
        }
    }
}

pub(crate) async fn list_ui_inbox_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::extract::Query(query): axum::extract::Query<crate::common::auth_utils::UiTenantQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let mobile_optimized = query.mobile_optimized.unwrap_or(false);

    let cache_key = format!("ui_inbox:{}:mobile:{}", tenant_id, mobile_optimized);
    let cache = super::ui_caches::UI_INBOX_CACHE
        .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));

    let items_opt = cache
        .get_or_fetch_with_swr(&cache_key, std::time::Duration::from_secs(5), {
            let db = db.clone();
            let t = tenant_id.clone();
            move || async move {
                match super::ui_inbox_ledger_readers::load_ui_inbox_from_db(
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
            ::server_telemetry::record_error_signal("[bug] Failed to fetch UI inbox messages");
            tracing::error!("Failed to fetch UI inbox messages");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!([])),
            )
                .into_response()
        }
    }
}

pub(crate) async fn ui_dashboard_metrics_handler(
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
        "ui_dashboard_metrics:{}:mobile:{}",
        tenant_id, mobile_optimized
    );
    let cache = super::ui_caches::UI_DASHBOARD_METRICS_CACHE
        .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));
    if let Some((cached, is_stale)) = cache.get_with_swr(&cache_key).await {
        if !is_stale {
            let fields = query.fields.as_deref();
            let shaped = ::server_utils::payload_shaper::shape_payload(
                serde_json::to_value(cached).unwrap_or_default(),
                fields,
            );
            return (axum::http::StatusCode::OK, axum::Json(shaped)).into_response();
        }

        let db = db.clone();
        let t = tenant_id.clone();
        let cache_key_bg = cache_key.clone();
        tokio::spawn(async move {
            if let Ok(metrics) =
                super::ui_triage_handlers::load_ui_dashboard_metrics(&db, &t, mobile_optimized)
                    .await
                && let Some(c) = super::ui_caches::UI_DASHBOARD_METRICS_CACHE.get()
            {
                let res = serde_json::to_value(metrics).unwrap_or_else(|_| serde_json::json!({}));
                c.set(&cache_key_bg, res, std::time::Duration::from_secs(10))
                    .await;
            }
        });
        let fields = query.fields.as_deref();
        let shaped = ::server_utils::payload_shaper::shape_payload(
            serde_json::to_value(cached).unwrap_or_default(),
            fields,
        );
        return (axum::http::StatusCode::OK, axum::Json(shaped)).into_response();
    }

    let metrics =
        super::ui_triage_handlers::load_ui_dashboard_metrics(&db, &tenant_id, mobile_optimized)
            .await;

    match metrics {
        Ok(metrics) => {
            let res = serde_json::to_value(metrics).unwrap_or_else(|_| serde_json::json!({}));
            cache
                .set(&cache_key, res.clone(), std::time::Duration::from_secs(10))
                .await;
            (axum::http::StatusCode::OK, axum::Json(res)).into_response()
        }
        Err(e) => {
            ::server_telemetry::record_error_signal("[bug] Failed to fetch UI dashboard metrics");
            tracing::error!("Failed to fetch UI dashboard metrics: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({
                    "active_customers": 0,
                    "pending_orders": 0,
                    "total_sales": 0.0,
                    "total_campaigns_sent": 0,
                    "auto_replied": 0,
                    "top_product": null,
                })),
            )
                .into_response()
        }
    }
}

pub(crate) async fn list_ui_supply_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::extract::Query(query): axum::extract::Query<crate::common::auth_utils::UiTenantQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let mobile_optimized = query.mobile_optimized.unwrap_or(false);

    // An explicit reload must not return a stale-while-revalidate snapshot.
    match super::ui_inbox_ledger_readers::load_ui_supply_from_db(&db, &tenant_id, mobile_optimized)
        .await
    {
        Ok(item) => {
            let fields = query.fields.as_deref();
            let shaped = ::server_utils::payload_shaper::shape_payload(item, fields);
            (axum::http::StatusCode::OK, axum::Json(shaped)).into_response()
        }
        Err(error) => {
            tracing::error!("Failed to fetch UI supply: {}", error);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": "supply_unavailable", "success": false})),
            )
                .into_response()
        }
    }
}

pub(crate) async fn create_ui_supply_vendor_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "name is required"})),
        )
            .into_response();
    }
    let contact_info = payload
        .get("contact_info")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let id = uuid::Uuid::new_v4().to_string();
    let result = match &db.store {
        crate::db::DbStore::Postgres => sqlx::query(
            "INSERT INTO vendors (id, tenant_id, name, contact_info) VALUES ($1, $2, $3, $4)",
        )
        .bind(&id)
        .bind(&tenant_id)
        .bind(name)
        .bind(contact_info)
        .execute(&db.pool)
        .await
        .map(|_| ()),
        crate::db::DbStore::Sqlite(pool) => sqlx::query(
            "INSERT INTO vendors (id, tenant_id, name, contact_info) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&tenant_id)
        .bind(name)
        .bind(contact_info)
        .execute(pool)
        .await
        .map(|_| ()),
    };
    match result {
        Ok(_) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({"id": id, "name": name, "contact_info": contact_info})),
        )
            .into_response(),
        Err(e) => {
            ::server_telemetry::record_error_signal("[bug] Failed to create UI supply vendor");
            tracing::error!("Failed to create UI supply vendor: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(serde_json::json!({"error": "database write failed"})),
            )
                .into_response()
        }
    }
}

pub(crate) async fn create_ui_raw_material_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let current_quantity = payload
        .get("current_quantity")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let reorder_threshold = payload
        .get("reorder_threshold")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    if name.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "name is required"})),
        )
            .into_response();
    }
    let id = uuid::Uuid::new_v4().to_string();
    let result = match &db.store {
    crate::db::DbStore::Postgres => sqlx::query("INSERT INTO raw_materials (id, tenant_id, name, current_quantity, reorder_threshold) VALUES ($1, $2, $3, $4, $5)").bind(&id).bind(&tenant_id).bind(name).bind(current_quantity).bind(reorder_threshold).execute(&db.pool).await.map(|_| ()),
    crate::db::DbStore::Sqlite(pool) => sqlx::query("INSERT INTO raw_materials (id, tenant_id, name, current_quantity, reorder_threshold) VALUES (?, ?, ?, ?, ?)").bind(&id).bind(&tenant_id).bind(name).bind(current_quantity).bind(reorder_threshold).execute(pool).await.map(|_| ()),
};
    match result {
    Ok(_) => (axum::http::StatusCode::OK, axum::Json(serde_json::json!({"id": id, "name": name, "current_quantity": current_quantity, "reorder_threshold": reorder_threshold}))).into_response(),
    Err(e) => {
        ::server_telemetry::record_error_signal("[bug] Failed to create UI raw material");
        tracing::error!("Failed to create UI raw material: {}", e);
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({"error": "database write failed"}))).into_response()
    }
}
}

pub(crate) async fn create_ui_bom_item_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::Json(payload): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let finished_good_id = payload
        .get("finished_good_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let raw_material_id = payload
        .get("raw_material_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let quantity_required = payload
        .get("quantity_required")
        .and_then(|v| v.as_i64())
        .unwrap_or(1) as i32;
    if finished_good_id.is_empty() || raw_material_id.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(
                serde_json::json!({"error": "finished_good_id and raw_material_id are required"}),
            ),
        )
            .into_response();
    }
    let id = uuid::Uuid::new_v4().to_string();
    let result = match &db.store {
    crate::db::DbStore::Postgres => sqlx::query("INSERT INTO bom_items (id, tenant_id, finished_good_id, raw_material_id, quantity_required) VALUES ($1, $2, $3, $4, $5)").bind(&id).bind(&tenant_id).bind(finished_good_id).bind(raw_material_id).bind(quantity_required).execute(&db.pool).await.map(|_| ()),
    crate::db::DbStore::Sqlite(pool) => sqlx::query("INSERT INTO bom_items (id, tenant_id, finished_good_id, raw_material_id, quantity_required) VALUES (?, ?, ?, ?, ?)").bind(&id).bind(&tenant_id).bind(finished_good_id).bind(raw_material_id).bind(quantity_required).execute(pool).await.map(|_| ()),
};
    match result {
    Ok(_) => (axum::http::StatusCode::OK, axum::Json(serde_json::json!({"id": id, "finished_good_id": finished_good_id, "raw_material_id": raw_material_id, "quantity_required": quantity_required}))).into_response(),
    Err(e) => {
        ::server_telemetry::record_error_signal("[bug] Failed to create UI BOM item");
        tracing::error!("Failed to create UI BOM item: {}", e);
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR, axum::Json(serde_json::json!({"error": "database write failed"}))).into_response()
    }
}
}
