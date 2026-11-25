// SPDX-License-Identifier: Apache-2.0
//! Unified feed aggregation and agent-feed UI endpoints.

pub(crate) async fn fetch_unified_feed_data(
    db: &std::sync::Arc<crate::db::DB>,
    tenant_id: &str,
    mobile_optimized: bool,
) -> serde_json::Value {
    let m_key = format!(
        "ui_dashboard_metrics:{}:mobile:{}",
        tenant_id, mobile_optimized
    );
    let o_key = format!("ui_orders:{}:mobile:{}", tenant_id, mobile_optimized);
    let i_key = format!("ui_inbox:{}:mobile:{}", tenant_id, mobile_optimized);
    let t_key = format!("ui_triage:{}:mobile:{}", tenant_id, mobile_optimized);
    let p_key = format!(
        "ui_priority_tasks:{}:mobile:{}",
        tenant_id, mobile_optimized
    );
    let a_key = format!("ui_approvals:{}:mobile:{}", tenant_id, mobile_optimized);
    let f_key = format!("ui_agent_feed:{}:mobile:{}", tenant_id, mobile_optimized);

    let (
        metrics_res,
        orders_res,
        inbox_res,
        triage_res,
        priority_tasks_res,
        approvals_res,
        agent_feed_res,
        invoices_res,
    ) = tokio::join!(
        tokio::spawn({
            let db_clone = db.clone();
            let t_clone = tenant_id.to_string();
            let m_key_clone = m_key.clone();
            async move {
                let cache = super::ui_caches::UI_DASHBOARD_METRICS_CACHE.get_or_init(|| {
                    ::server_utils::cache::HybridCache::new(crate::distributed_cache())
                });
                cache
                    .get_or_fetch_with_swr(
                        &m_key_clone,
                        std::time::Duration::from_secs(60),
                        move || async move {
                            super::ui_triage_handlers::load_ui_dashboard_metrics(
                                &db_clone,
                                &t_clone,
                                mobile_optimized,
                            )
                            .await
                            .map(|m| serde_json::to_value(m).unwrap_or_default())
                            .ok()
                        },
                    )
                    .await
                    .unwrap_or_default()
            }
        }),
        tokio::spawn({
            let db_clone = db.clone();
            let t_clone = tenant_id.to_string();
            let o_key_clone = o_key.clone();
            async move {
                let cache = super::ui_caches::UI_ORDERS_CACHE.get_or_init(|| {
                    ::server_utils::cache::HybridCache::new(crate::distributed_cache())
                });
                cache
                    .get_or_fetch_with_swr(
                        &o_key_clone,
                        std::time::Duration::from_secs(5),
                        move || async move {
                            crate::api::ui_orders::load(&db_clone, &t_clone, mobile_optimized, None)
                                .await
                                .ok()
                        },
                    )
                    .await
                    .unwrap_or_default()
            }
        }),
        tokio::spawn({
            let db_clone = db.clone();
            let t_clone = tenant_id.to_string();
            let i_key_clone = i_key.clone();
            async move {
                let cache = super::ui_caches::UI_INBOX_CACHE.get_or_init(|| {
                    ::server_utils::cache::HybridCache::new(crate::distributed_cache())
                });
                cache
                    .get_or_fetch_with_swr(
                        &i_key_clone,
                        std::time::Duration::from_secs(5),
                        move || async move {
                            super::ui_inbox_ledger_readers::load_ui_inbox_from_db(
                                &db_clone,
                                &t_clone,
                                mobile_optimized,
                            )
                            .await
                            .ok()
                        },
                    )
                    .await
                    .unwrap_or_default()
            }
        }),
        tokio::spawn({
            let db_clone = db.clone();
            let t_clone = tenant_id.to_string();
            let t_key_clone = t_key.clone();
            async move {
                let cache = super::ui_caches::UI_TRIAGE_CACHE.get_or_init(|| {
                    ::server_utils::cache::HybridCache::new(crate::distributed_cache())
                });
                cache
                    .get_or_fetch_with_swr(
                        &t_key_clone,
                        std::time::Duration::from_secs(10),
                        move || async move {
                            super::ui_triage_feed_readers::load_ui_triage_from_db(
                                &db_clone,
                                &t_clone,
                                mobile_optimized,
                            )
                            .await
                            .ok()
                        },
                    )
                    .await
                    .unwrap_or_default()
            }
        }),
        tokio::spawn({
            let db_clone = db.clone();
            let t_clone = tenant_id.to_string();
            let p_key_clone = p_key.clone();
            async move {
                let cache = super::ui_caches::UI_PRIORITY_TASKS_CACHE.get_or_init(|| {
                    ::server_utils::cache::HybridCache::new(crate::distributed_cache())
                });
                cache
                    .get_or_fetch_with_swr(
                        &p_key_clone,
                        std::time::Duration::from_secs(10),
                        move || async move {
                            super::ui_triage_feed_readers::load_ui_priority_tasks_from_db(
                                &db_clone,
                                &t_clone,
                                mobile_optimized,
                            )
                            .await
                            .ok()
                        },
                    )
                    .await
                    .unwrap_or_default()
            }
        }),
        tokio::spawn({
            let db_clone = db.clone();
            let t_clone = tenant_id.to_string();
            let a_key_clone = a_key.clone();
            async move {
                let cache = super::ui_caches::UI_AGENT_APPROVALS_CACHE.get_or_init(|| {
                    ::server_utils::cache::HybridCache::new(crate::distributed_cache())
                });
                cache
                    .get_or_fetch_with_swr(
                        &a_key_clone,
                        std::time::Duration::from_secs(10),
                        move || async move {
                            let mut res =
                                super::ui_inbox_ledger_readers::load_ui_agent_approvals_from_db(
                                    &db_clone,
                                    &t_clone,
                                    mobile_optimized,
                                )
                                .await
                                .ok()?;
                            for approval in &mut res {
                                if let Some(obj) = approval.as_object_mut()
                                    && !obj.contains_key("lifecycle_state")
                                {
                                    let status = obj
                                        .get("status")
                                        .and_then(|s| s.as_str())
                                        .unwrap_or("PENDING");
                                    let lifecycle_state =
                                        if status == "DRAFT" || status == "PENDING" {
                                            "PENDING_APPROVAL"
                                        } else {
                                            status
                                        };
                                    obj.insert(
                                        "lifecycle_state".to_string(),
                                        serde_json::json!(lifecycle_state),
                                    );
                                }
                            }
                            Some(res)
                        },
                    )
                    .await
                    .unwrap_or_default()
            }
        }),
        tokio::spawn({
            let db_clone = db.clone();
            let t_clone = tenant_id.to_string();
            let f_key_clone = f_key.clone();
            async move {
                let cache = super::ui_caches::UI_AGENT_FEED_CACHE.get_or_init(|| {
                    ::server_utils::cache::HybridCache::new(crate::distributed_cache())
                });
                cache
                    .get_or_fetch_with_tags_swr(
                        &f_key_clone,
                        vec![format!("ui_feed_tenant:{}", t_clone)],
                        std::time::Duration::from_secs(10),
                        move || async move {
                            super::ui_triage_feed_readers::load_ui_agent_feed_from_db(
                                &db_clone,
                                &t_clone,
                                mobile_optimized,
                            )
                            .await
                            .ok()
                        },
                    )
                    .await
                    .unwrap_or_default()
            }
        }),
        tokio::spawn({
            let db_clone = db.clone();
            let t_clone = tenant_id.to_string();
            async move {
                let cache = super::ui_caches::UI_INVOICES_CACHE.get_or_init(|| {
                    ::server_utils::cache::HybridCache::new(crate::distributed_cache())
                });
                let i_key = format!("ui_invoices:{}:mobile:{}", t_clone, mobile_optimized);
                cache
                    .get_or_fetch_with_swr(
                        &i_key,
                        std::time::Duration::from_secs(10),
                        move || async move {
                            super::ui_ops_handlers::load_ui_invoices_from_db(
                                &db_clone,
                                &t_clone,
                                mobile_optimized,
                            )
                            .await
                            .ok()
                        },
                    )
                    .await
                    .unwrap_or_default()
            }
        })
    );

    serde_json::json!({
        "metrics": metrics_res.unwrap_or_default(),
        "orders": orders_res.unwrap_or_default(),
        "inbox": inbox_res.unwrap_or_default(),
        "triage": triage_res.unwrap_or_default(),
        "pending_approvals": approvals_res.unwrap_or_default(),
        "agent_feed": agent_feed_res.unwrap_or_default(),
        "priority_tasks": priority_tasks_res.unwrap_or_default(),
        "invoices": invoices_res.unwrap_or_else(|_| vec![]),
    })
}

pub(crate) async fn ui_dashboard_unified_feed_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::extract::Query(query): axum::extract::Query<crate::common::auth_utils::UiTenantQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let mobile_optimized = query.mobile_optimized.unwrap_or(false);
    let fields = query.fields.as_deref();

    let cache_key = format!(
        "ui_dashboard_unified:{}:mobile:{}:fields:{}",
        tenant_id,
        mobile_optimized,
        fields.unwrap_or("")
    );
    let cache = super::ui_caches::UI_UNIFIED_FEED_CACHE
        .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));

    let supply_future = tokio::spawn({
        let db = db.clone();
        let t = tenant_id.clone();
        async move {
            super::ui_inbox_ledger_readers::load_ui_supply_from_db(&db, &t, mobile_optimized).await
        }
    });

    let mut final_result = cache
        .get_or_fetch_with_tags_swr(
            &cache_key,
            vec![format!("ui_feed_tenant:{}", tenant_id)],
            std::time::Duration::from_secs(10),
            {
                let db_bg = db.clone();
                let t_bg = tenant_id.clone();
                move || async move {
                    Some(fetch_unified_feed_data(&db_bg, &t_bg, mobile_optimized).await)
                }
            },
        )
        .await
        .unwrap_or_else(|| serde_json::json!({}));

    // Supply should not be cached because it changes continuously (inventory counts),
    // so we fetch supply and merge it on cache hit or miss.
    let supply_val = match supply_future.await {
        Ok(Ok(supply)) => supply,
        _ => serde_json::json!({"error": "supply_unavailable", "success": false}),
    };
    if let Some(obj) = final_result.as_object_mut() {
        obj.insert("supply".to_string(), supply_val.clone());
    }

    let shaped = ::server_utils::payload_shaper::shape_payload(final_result, fields);
    (axum::http::StatusCode::OK, axum::Json(shaped)).into_response()
}

pub(crate) async fn fetch_unified_agent_feed_data(
    db: &std::sync::Arc<crate::db::DB>,
    tenant_id: &str,
    mobile_optimized: bool,
    fields: Option<&str>,
) -> serde_json::Value {
    let a_key = format!("ui_approvals:{}:mobile:{}", tenant_id, mobile_optimized);
    let f_key = format!("ui_agent_feed:{}:mobile:{}", tenant_id, mobile_optimized);

    let needs_pending_approvals = fields.is_none_or(|f| f.contains("pending_approvals"));
    let needs_ledger = fields.is_none_or(|f| f.contains("ledger") || f.contains("entries"));
    let needs_agent_feed = fields.is_none_or(|f| f.contains("agent_feed"));

    let (approvals_res, ledger_res, agent_feed_res) = tokio::join!(
        tokio::spawn({
            let db_clone = db.clone();
            let t_clone = tenant_id.to_string();
            let a_key_clone = a_key.clone();
            async move {
                if !needs_pending_approvals {
                    return serde_json::Value::Array(vec![]);
                }
                let cache = super::ui_caches::UI_AGENT_APPROVALS_CACHE.get_or_init(|| {
                    ::server_utils::cache::HybridCache::new(crate::distributed_cache())
                });
                let res = cache
                    .get_or_fetch_with_swr(
                        &a_key_clone,
                        std::time::Duration::from_secs(10),
                        move || async move {
                            let mut res =
                                super::ui_inbox_ledger_readers::load_ui_agent_approvals_from_db(
                                    &db_clone,
                                    &t_clone,
                                    mobile_optimized,
                                )
                                .await
                                .ok()?;
                            for approval in &mut res {
                                if let Some(obj) = approval.as_object_mut()
                                    && !obj.contains_key("lifecycle_state")
                                {
                                    let status = obj
                                        .get("status")
                                        .and_then(|s| s.as_str())
                                        .unwrap_or("PENDING");
                                    let lifecycle_state =
                                        if status == "DRAFT" || status == "PENDING" {
                                            "PENDING_APPROVAL"
                                        } else {
                                            status
                                        };
                                    obj.insert(
                                        "lifecycle_state".to_string(),
                                        serde_json::json!(lifecycle_state),
                                    );
                                }
                            }
                            Some(res)
                        },
                    )
                    .await
                    .unwrap_or_default();
                serde_json::Value::Array(res)
            }
        }),
        tokio::spawn({
            let db_clone = db.clone();
            let t_clone = tenant_id.to_string();
            let l_key = format!("ui_ledger:{}:mobile:{}", tenant_id, mobile_optimized);
            async move {
                if !needs_ledger {
                    return serde_json::Value::Array(vec![]);
                }
                let cache = super::ui_caches::UI_LEDGER_CACHE.get_or_init(|| {
                    ::server_utils::cache::HybridCache::new(crate::distributed_cache())
                });
                let res = cache
                    .get_or_fetch_with_swr(
                        &l_key,
                        std::time::Duration::from_secs(10),
                        move || async move {
                            super::ui_inbox_ledger_readers::load_ui_ledger_from_db(
                                &db_clone,
                                &t_clone,
                                mobile_optimized,
                            )
                            .await
                            .ok()
                        },
                    )
                    .await
                    .unwrap_or_default();
                serde_json::Value::Array(res)
            }
        }),
        tokio::spawn({
            let db_clone = db.clone();
            let t_clone = tenant_id.to_string();
            let f_key_clone = f_key.clone();
            async move {
                if !needs_agent_feed {
                    return serde_json::Value::Array(vec![]);
                }
                let cache = super::ui_caches::UI_AGENT_FEED_CACHE.get_or_init(|| {
                    ::server_utils::cache::HybridCache::new(crate::distributed_cache())
                });
                let res = cache
                    .get_or_fetch_with_tags_swr(
                        &f_key_clone,
                        vec![format!("ui_feed_tenant:{}", t_clone)],
                        std::time::Duration::from_secs(10),
                        move || async move {
                            super::ui_triage_feed_readers::load_ui_agent_feed_from_db(
                                &db_clone,
                                &t_clone,
                                mobile_optimized,
                            )
                            .await
                            .ok()
                        },
                    )
                    .await
                    .unwrap_or_default();
                serde_json::Value::Array(res)
            }
        })
    );

    let pending_approvals = approvals_res.unwrap_or_default();
    let entries = ledger_res.unwrap_or_default();
    let agent_feed = agent_feed_res.unwrap_or_default();

    serde_json::json!({
        "pending_approvals": pending_approvals,
        "entries": entries,
        "agent_feed": agent_feed
    })
}

pub(crate) async fn ui_dashboard_unified_agent_feed_handler(
    axum::extract::State(db): axum::extract::State<std::sync::Arc<crate::db::DB>>,
    axum::extract::Extension(claims): axum::extract::Extension<::server_common::Claims>,
    axum::extract::Query(query): axum::extract::Query<crate::common::auth_utils::UiTenantQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(tenant_id) = super::ui_caches::strict_ui_claim_tenant(&claims) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let mobile_optimized = query.mobile_optimized.unwrap_or(false);
    let fields = query.fields.as_deref();

    let cache_key = format!(
        "ui_unified_agent_feed:{}:mobile:{}:fields:{}",
        tenant_id,
        mobile_optimized,
        fields.unwrap_or("")
    );
    let cache = super::ui_caches::UI_UNIFIED_AGENT_FEED_CACHE
        .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));

    let fields_str = fields.map(|s| s.to_string());
    let items_opt = cache
        .get_or_fetch_with_tags_swr(
            &cache_key,
            vec![format!("ui_feed_tenant:{}", tenant_id)],
            std::time::Duration::from_secs(10),
            {
                let db = db.clone();
                let t = tenant_id.clone();
                let f_bg = fields_str.clone();
                move || async move {
                    Some(
                        fetch_unified_agent_feed_data(&db, &t, mobile_optimized, f_bg.as_deref())
                            .await,
                    )
                }
            },
        )
        .await;

    let mut result = items_opt.unwrap_or_else(|| serde_json::json!({}));
    result = ::server_utils::payload_shaper::shape_payload(result, fields);
    (axum::http::StatusCode::OK, axum::Json(result)).into_response()
}
