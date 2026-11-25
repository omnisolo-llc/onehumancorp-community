// SPDX-License-Identifier: Apache-2.0
//! UI triage, priority-tasks, and agent-feed readers.

use sqlx::Row;

pub(crate) async fn load_ui_triage_from_db(
    db: &crate::db::DB,
    tenant_id: &str,
    mobile_optimized: bool,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let mut results = Vec::new();

    let db1 = db.clone();
    let db2 = db.clone();
    let db3 = db.clone();
    let db4 = db.clone();
    let db5 = db.clone();
    let t_id1 = tenant_id.to_string();
    let t_id2 = tenant_id.to_string();
    let t_id3 = tenant_id.to_string();
    let t_id4 = tenant_id.to_string();
    let t_id5 = tenant_id.to_string();

    let (legacy_res, feed_res, approvals_res, daily_work_res, envelope_res) = tokio::join!(
        tokio::spawn(async move {
            let mut legacy_rows_json = Vec::new();
            match &db1.store {
                crate::db::DbStore::Postgres => {
                    let mut rows_opt = None;
                    if let Ok(mut tx) = db1.pool.begin().await {
                        let ctx_res =
                            ::server_common::auth_utils::set_org_context(&mut *tx, &t_id1).await;
                        if ctx_res.is_ok() {
                            if mobile_optimized {
                                let query_str = "SELECT id, status, CAST(created_at AS text) AS created_at, action_type, source, context FROM (SELECT t.id, t.tenant_id, t.status, t.created_at, a.action_type, t.source, t.context FROM triage_items t LEFT JOIN triage_proposed_actions a ON t.id = a.triage_item_id UNION ALL SELECT a.id, a.tenant_id, a.status, a.created_at, a.action_type, t.channel AS source, (SELECT content FROM unified_messages WHERE thread_id = t.id ORDER BY created_at DESC LIMIT 1) AS context FROM unified_triage_actions a JOIN unified_threads t ON a.thread_id = t.id) sub WHERE tenant_id = $1 AND status != 'resolved' AND status != 'dismissed' ORDER BY created_at DESC LIMIT 50";
                                if let Ok(rows) = sqlx::query(query_str)
                                    .bind(&t_id1)
                                    .fetch_all(&mut *tx)
                                    .await
                                {
                                    rows_opt = Some((rows, true));
                                }
                            } else {
                                let query_str = "SELECT id, tenant_id, customer_id, source, priority, context, status, CAST(created_at AS text) AS created_at, action_type, action_payload FROM (SELECT t.id, t.tenant_id, t.customer_id, t.source, t.priority, t.context, t.status, t.created_at, a.action_type, a.payload::text AS action_payload FROM triage_items t LEFT JOIN triage_proposed_actions a ON t.id = a.triage_item_id UNION ALL SELECT a.id, a.tenant_id, t.customer_id, t.channel AS source, 'normal' AS priority, (SELECT content FROM unified_messages WHERE thread_id = t.id ORDER BY created_at DESC LIMIT 1) AS context, a.status, a.created_at, a.action_type, a.action_payload FROM unified_triage_actions a JOIN unified_threads t ON a.thread_id = t.id) sub WHERE tenant_id = $1 AND status != 'resolved' AND status != 'dismissed' ORDER BY created_at DESC LIMIT 50";
                                if let Ok(rows) = sqlx::query(query_str)
                                    .bind(&t_id1)
                                    .fetch_all(&mut *tx)
                                    .await
                                {
                                    rows_opt = Some((rows, false));
                                }
                            }
                            let _ = tx.commit().await;
                        }
                    }
                    if let Some((rows, is_mobile)) = rows_opt {
                        for row in rows {
                            use sqlx::Row;
                            if is_mobile {
                                let item = serde_json::json!({
                                    "id": row.get::<String, _>("id"),
                                    "status": row.try_get::<String, _>("status").unwrap_or_default(),
                                    "created_at": row.try_get::<String, _>("created_at").unwrap_or_else(|_| row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at").map(|dt| dt.to_rfc3339()).unwrap_or_default()),
                                    "action_type": row.try_get::<String, _>("action_type").unwrap_or_default(),
                                    "source": row.try_get::<String, _>("source").unwrap_or_default(),
                                    "context": row.try_get::<String, _>("context").unwrap_or_default(),
                                });
                                legacy_rows_json.push(item);
                            } else {
                                let item = serde_json::json!({
                                    "id": row.get::<String, _>("id"),
                                    "tenant_id": row.get::<String, _>("tenant_id"),
                                    "customer_id": row.try_get::<String, _>("customer_id").unwrap_or_default(),
                                    "source": row.try_get::<String, _>("source").unwrap_or_default(),
                                    "priority": row.try_get::<String, _>("priority").unwrap_or_default(),
                                    "context": row.try_get::<String, _>("context").unwrap_or_default(),
                                    "status": row.try_get::<String, _>("status").unwrap_or_default(),
                                    "created_at": row.try_get::<String, _>("created_at").unwrap_or_else(|_| row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at").map(|dt| dt.to_rfc3339()).unwrap_or_default()),
                                    "action_type": row.try_get::<String, _>("action_type").unwrap_or_default(),
                                    "action_payload": row.try_get::<String, _>("action_payload").unwrap_or_default(),
                                });
                                legacy_rows_json.push(item);
                            }
                        }
                    }
                }
                crate::db::DbStore::Sqlite(pool) => {
                    if mobile_optimized {
                        let query_str = "SELECT id, status, CAST(created_at AS TEXT) AS created_at, action_type, source, context FROM (SELECT t.id, t.tenant_id, t.status, t.created_at, a.action_type, t.source, t.context FROM triage_items t LEFT JOIN triage_proposed_actions a ON t.id = a.triage_item_id UNION ALL SELECT a.id, a.tenant_id, a.status, a.created_at, a.action_type, t.channel AS source, (SELECT content FROM unified_messages WHERE thread_id = t.id ORDER BY created_at DESC LIMIT 1) AS context FROM unified_triage_actions a JOIN unified_threads t ON a.thread_id = t.id) sub WHERE tenant_id = ? AND status != 'resolved' AND status != 'dismissed' ORDER BY created_at DESC LIMIT 50";
                        if let Ok(rows) = sqlx::query(query_str).bind(&t_id1).fetch_all(pool).await
                        {
                            for row in rows {
                                use sqlx::Row;
                                let item = serde_json::json!({
                                    "id": row.get::<String, _>("id"),
                                    "status": row.try_get::<String, _>("status").unwrap_or_default(),
                                    "created_at": row.try_get::<String, _>("created_at").unwrap_or_else(|_| row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at").map(|dt| dt.to_rfc3339()).unwrap_or_default()),
                                    "action_type": row.try_get::<String, _>("action_type").unwrap_or_default(),
                                    "source": row.try_get::<String, _>("source").unwrap_or_default(),
                                    "context": row.try_get::<String, _>("context").unwrap_or_default(),
                                });
                                legacy_rows_json.push(item);
                            }
                        }
                    } else {
                        let query_str = "SELECT id, tenant_id, customer_id, source, priority, context, status, CAST(created_at AS TEXT) AS created_at, action_type, action_payload FROM (SELECT t.id, t.tenant_id, t.customer_id, t.source, t.priority, t.context, t.status, t.created_at, a.action_type, a.payload AS action_payload FROM triage_items t LEFT JOIN triage_proposed_actions a ON t.id = a.triage_item_id UNION ALL SELECT a.id, a.tenant_id, t.customer_id, t.channel AS source, 'normal' AS priority, (SELECT content FROM unified_messages WHERE thread_id = t.id ORDER BY created_at DESC LIMIT 1) AS context, a.status, a.created_at, a.action_type, a.action_payload FROM unified_triage_actions a JOIN unified_threads t ON a.thread_id = t.id) sub WHERE tenant_id = ? AND status != 'resolved' AND status != 'dismissed' ORDER BY created_at DESC LIMIT 50";
                        if let Ok(rows) = sqlx::query(query_str).bind(&t_id1).fetch_all(pool).await
                        {
                            for row in rows {
                                use sqlx::Row;
                                let item = serde_json::json!({
                                    "id": row.get::<String, _>("id"),
                                    "tenant_id": row.get::<String, _>("tenant_id"),
                                    "customer_id": row.try_get::<String, _>("customer_id").unwrap_or_default(),
                                    "source": row.try_get::<String, _>("source").unwrap_or_default(),
                                    "priority": row.try_get::<String, _>("priority").unwrap_or_default(),
                                    "context": row.try_get::<String, _>("context").unwrap_or_default(),
                                    "status": row.try_get::<String, _>("status").unwrap_or_default(),
                                    "created_at": row.try_get::<String, _>("created_at").unwrap_or_else(|_| row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at").map(|dt| dt.to_rfc3339()).unwrap_or_default()),
                                    "action_type": row.try_get::<String, _>("action_type").unwrap_or_default(),
                                    "action_payload": row.try_get::<String, _>("action_payload").unwrap_or_default(),
                                });
                                legacy_rows_json.push(item);
                            }
                        }
                    }
                }
            }
            legacy_rows_json
        }),
        tokio::spawn(async move {
            let mut feed_rows_json = Vec::new();
            match &db2.store {
                crate::db::DbStore::Postgres => {
                    let mut rows_opt = None;
                    if let Ok(mut tx) = db2.pool.begin().await {
                        let ctx_res =
                            ::server_common::auth_utils::set_org_context(&mut *tx, &t_id2).await;
                        if ctx_res.is_ok() {
                            let query_str = "SELECT id, tenant_id, event_source, context_payload, proposed_action, lifecycle_state, created_at, updated_at FROM agent_feed_items WHERE tenant_id = $1 AND lifecycle_state = 'PENDING_APPROVAL' ORDER BY created_at DESC LIMIT 50";
                            if let Ok(rows) = sqlx::query(query_str)
                                .bind(&t_id2)
                                .fetch_all(&mut *tx)
                                .await
                            {
                                rows_opt = Some(rows);
                            }
                            let _ = tx.commit().await;
                        }
                    }
                    if let Some(rows) = rows_opt {
                        for row in rows {
                            use sqlx::Row;
                            let context_payload: Option<serde_json::Value> =
                                match row.try_get::<sqlx::types::Json<serde_json::Value>, _>(
                                    "context_payload",
                                ) {
                                    Ok(j) => Some(j.0),
                                    Err(_) => match row.try_get::<String, _>("context_payload") {
                                        Ok(s) => serde_json::from_str(&s).ok(),
                                        Err(_) => None,
                                    },
                                };
                            let proposed_action: Option<serde_json::Value> =
                                match row.try_get::<sqlx::types::Json<serde_json::Value>, _>(
                                    "proposed_action",
                                ) {
                                    Ok(j) => Some(j.0),
                                    Err(_) => match row.try_get::<String, _>("proposed_action") {
                                        Ok(s) => serde_json::from_str(&s).ok(),
                                        Err(_) => None,
                                    },
                                };
                            let item = serde_json::json!({
                                "id": row.get::<String, _>("id"),
                                "tenant_id": row.get::<String, _>("tenant_id"),
                                "event_source": row.get::<String, _>("event_source"),
                                "source": row.get::<String, _>("event_source"),
                                "action_type": "approval",
                                "context_payload": context_payload,
                                "proposed_action": proposed_action.clone(),
                                "payload": proposed_action,
                                "lifecycle_state": row.get::<String, _>("lifecycle_state"),
                                "created_at": match row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at") { Ok(dt) => dt.to_rfc3339(), Err(_) => "".to_string() },
                                "updated_at": match row.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at") { Ok(dt) => dt.to_rfc3339(), Err(_) => "".to_string() },
                            });
                            feed_rows_json.push(item);
                        }
                    }
                }
                crate::db::DbStore::Sqlite(pool) => {
                    let query_str = "SELECT id, tenant_id, event_source, context_payload, proposed_action, lifecycle_state, created_at, updated_at FROM agent_feed_items WHERE tenant_id = ? AND lifecycle_state = 'PENDING_APPROVAL' ORDER BY created_at DESC LIMIT 50";
                    if let Ok(rows) = sqlx::query(query_str).bind(&t_id2).fetch_all(pool).await {
                        for row in rows {
                            use sqlx::Row;
                            let context_payload: Option<serde_json::Value> =
                                match row.try_get::<sqlx::types::Json<serde_json::Value>, _>(
                                    "context_payload",
                                ) {
                                    Ok(j) => Some(j.0),
                                    Err(_) => match row.try_get::<String, _>("context_payload") {
                                        Ok(s) => serde_json::from_str(&s).ok(),
                                        Err(_) => None,
                                    },
                                };
                            let proposed_action: Option<serde_json::Value> =
                                match row.try_get::<sqlx::types::Json<serde_json::Value>, _>(
                                    "proposed_action",
                                ) {
                                    Ok(j) => Some(j.0),
                                    Err(_) => match row.try_get::<String, _>("proposed_action") {
                                        Ok(s) => serde_json::from_str(&s).ok(),
                                        Err(_) => None,
                                    },
                                };
                            let item = serde_json::json!({
                                "id": row.get::<String, _>("id"),
                                "tenant_id": row.get::<String, _>("tenant_id"),
                                "event_source": row.get::<String, _>("event_source"),
                                "source": row.get::<String, _>("event_source"),
                                "action_type": "approval",
                                "context_payload": context_payload,
                                "proposed_action": proposed_action.clone(),
                                "payload": proposed_action,
                                "lifecycle_state": row.get::<String, _>("lifecycle_state"),
                                "created_at": match row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at") { Ok(dt) => dt.to_rfc3339(), Err(_) => "".to_string() },
                                "updated_at": match row.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at") { Ok(dt) => dt.to_rfc3339(), Err(_) => "".to_string() },
                            });
                            feed_rows_json.push(item);
                        }
                    }
                }
            }
            feed_rows_json
        }),
        tokio::spawn(async move {
            let mut approvals = super::ui_inbox_ledger_readers::load_ui_agent_approvals_from_db(
                &db3,
                &t_id3,
                mobile_optimized,
            )
            .await
            .unwrap_or_default();
            for approval in &mut approvals {
                if let Some(obj) = approval.as_object_mut() {
                    // Ensure approval items map correctly to triage UI
                    if !obj.contains_key("lifecycle_state") {
                        let status = obj
                            .get("status")
                            .and_then(|s| s.as_str())
                            .unwrap_or("PENDING");
                        let lifecycle_state = if status == "DRAFT" || status == "PENDING" {
                            "PENDING_APPROVAL"
                        } else {
                            status
                        };
                        obj.insert(
                            "lifecycle_state".to_string(),
                            serde_json::json!(lifecycle_state),
                        );
                    }
                    if !obj.contains_key("action_type") {
                        obj.insert("action_type".to_string(), serde_json::json!("approval"));
                    }
                    if !obj.contains_key("created_at") {
                        obj.insert("created_at".to_string(), serde_json::json!(""));
                    }
                }
            }
            approvals
        }),
        tokio::spawn(async move {
            let mut daily_work_rows_json = Vec::new();
            match &db4.store {
                crate::db::DbStore::Postgres => {
                    let mut rows_opt = None;
                    if let Ok(mut tx) = db4.pool.begin().await {
                        let ctx_res =
                            ::server_common::auth_utils::set_org_context(&mut *tx, &t_id4).await;
                        if ctx_res.is_ok() {
                            let query_str = if mobile_optimized {
                                "SELECT id, signal_id, intent, status, CAST(created_at AS text) AS created_at FROM daily_work_items WHERE tenant_id = $1 AND status = 'PENDING' ORDER BY created_at DESC LIMIT 50"
                            } else {
                                "SELECT id, signal_id, intent, customer_info, suggested_actions, status, CAST(created_at AS text) AS created_at FROM daily_work_items WHERE tenant_id = $1 AND status = 'PENDING' ORDER BY created_at DESC LIMIT 50"
                            };
                            if let Ok(rows) = sqlx::query(query_str)
                                .bind(&t_id4)
                                .fetch_all(&mut *tx)
                                .await
                            {
                                rows_opt = Some(rows);
                            }
                            let _ = tx.commit().await;
                        }
                    }
                    if let Some(rows) = rows_opt {
                        for row in rows {
                            use sqlx::Row;
                            if mobile_optimized {
                                daily_work_rows_json.push(serde_json::json!({
                                "id": row.get::<String, _>("id"),
                                "tenant_id": t_id4,
                                "action_type": "approval",
                                "signal_id": row.try_get::<String, _>("signal_id").unwrap_or_default(),
                                "intent": row.get::<String, _>("intent"),
                                "status": row.get::<String, _>("status"),
                                "created_at": row.try_get::<String, _>("created_at").unwrap_or_default(),
                            }));
                            } else {
                                let customer_info: Option<serde_json::Value> = row
                                    .try_get::<sqlx::types::Json<serde_json::Value>, _>(
                                        "customer_info",
                                    )
                                    .ok()
                                    .map(|j| j.0);
                                let suggested_actions: Option<serde_json::Value> = row
                                    .try_get::<sqlx::types::Json<serde_json::Value>, _>(
                                        "suggested_actions",
                                    )
                                    .ok()
                                    .map(|j| j.0);
                                daily_work_rows_json.push(serde_json::json!({
                                "id": row.get::<String, _>("id"),
                                "tenant_id": t_id4,
                                "signal_id": row.try_get::<String, _>("signal_id").unwrap_or_default(),
                                "intent": row.get::<String, _>("intent"),
                                "customer_info": customer_info,
                                "suggested_actions": suggested_actions,
                                "status": row.get::<String, _>("status"),
                                "created_at": row.try_get::<String, _>("created_at").unwrap_or_default(),
                            }));
                            }
                        }
                    }
                }
                crate::db::DbStore::Sqlite(pool) => {
                    let query_str = if mobile_optimized {
                        "SELECT id, signal_id, intent, status, CAST(created_at AS TEXT) AS created_at FROM daily_work_items WHERE tenant_id = ? AND status = 'PENDING' ORDER BY created_at DESC LIMIT 50"
                    } else {
                        "SELECT id, signal_id, intent, customer_info, suggested_actions, status, CAST(created_at AS TEXT) AS created_at FROM daily_work_items WHERE tenant_id = ? AND status = 'PENDING' ORDER BY created_at DESC LIMIT 50"
                    };
                    if let Ok(rows) = sqlx::query(query_str).bind(&t_id4).fetch_all(pool).await {
                        for row in rows {
                            use sqlx::Row;
                            if mobile_optimized {
                                daily_work_rows_json.push(serde_json::json!({
                                "id": row.get::<String, _>("id"),
                                "tenant_id": t_id4,
                                "action_type": "approval",
                                "signal_id": row.try_get::<String, _>("signal_id").unwrap_or_default(),
                                "intent": row.get::<String, _>("intent"),
                                "status": row.get::<String, _>("status"),
                                "created_at": row.try_get::<String, _>("created_at").unwrap_or_default(),
                            }));
                            } else {
                                let customer_info: Option<serde_json::Value> = row
                                    .try_get::<String, _>("customer_info")
                                    .ok()
                                    .and_then(|s| serde_json::from_str(&s).ok());
                                let suggested_actions: Option<serde_json::Value> = row
                                    .try_get::<String, _>("suggested_actions")
                                    .ok()
                                    .and_then(|s| serde_json::from_str(&s).ok());
                                daily_work_rows_json.push(serde_json::json!({
                                "id": row.get::<String, _>("id"),
                                "tenant_id": t_id4,
                                "signal_id": row.try_get::<String, _>("signal_id").unwrap_or_default(),
                                "intent": row.get::<String, _>("intent"),
                                "customer_info": customer_info,
                                "suggested_actions": suggested_actions,
                                "status": row.get::<String, _>("status"),
                                "created_at": row.try_get::<String, _>("created_at").unwrap_or_default(),
                            }));
                            }
                        }
                    }
                }
            }
            daily_work_rows_json
        }),
        tokio::spawn(async move {
            let mut envelope_rows_json = Vec::new();
            match &db5.store {
                crate::db::DbStore::Postgres => {
                    let mut rows_opt = None;
                    if let Ok(mut tx) = db5.pool.begin().await {
                        let ctx_res =
                            ::server_common::auth_utils::set_org_context(&mut *tx, &t_id5).await;
                        if ctx_res.is_ok() {
                            let query_str = if mobile_optimized {
                                "SELECT id, status, CAST(created_at AS text) AS created_at FROM task_envelopes WHERE tenant_id = $1 AND status NOT IN ('COMPLETED', 'APPROVED', 'DISMISSED') ORDER BY created_at DESC LIMIT 50"
                            } else {
                                "SELECT id, current_department, status, payload::text AS payload, routing_history::text AS routing_history, CAST(created_at AS text) AS created_at FROM task_envelopes WHERE tenant_id = $1 AND status NOT IN ('COMPLETED', 'APPROVED', 'DISMISSED') ORDER BY created_at DESC LIMIT 50"
                            };
                            if let Ok(rows) = sqlx::query(query_str)
                                .bind(&t_id5)
                                .fetch_all(&mut *tx)
                                .await
                            {
                                rows_opt = Some(rows);
                            }
                            let _ = tx.commit().await;
                        }
                    }
                    if let Some(rows) = rows_opt {
                        for row in rows {
                            use sqlx::Row;
                            let mut map = serde_json::Map::new();
                            map.insert(
                                "id".to_string(),
                                serde_json::json!(
                                    row.try_get::<String, _>("id").unwrap_or_default()
                                ),
                            );
                            map.insert("tenant_id".to_string(), serde_json::json!(t_id5));
                            map.insert("intent".to_string(), serde_json::json!("task_envelope"));
                            map.insert(
                                "status".to_string(),
                                serde_json::json!(
                                    row.try_get::<String, _>("status").unwrap_or_default()
                                ),
                            );
                            map.insert(
                                "created_at".to_string(),
                                serde_json::json!(
                                    row.try_get::<String, _>("created_at").unwrap_or_default()
                                ),
                            );

                            if !mobile_optimized {
                                let dept = row
                                    .try_get::<String, _>("current_department")
                                    .unwrap_or_default();
                                map.insert(
                                    "customer_info".to_string(),
                                    serde_json::json!({ "department": dept }),
                                );
                                let payload_str: String =
                                    row.try_get("payload").unwrap_or_else(|_| "{}".to_string());
                                let payload_val: serde_json::Value =
                                    serde_json::from_str(&payload_str)
                                        .unwrap_or_else(|_| serde_json::json!({}));
                                map.insert("suggested_actions".to_string(), payload_val);
                            }

                            envelope_rows_json.push(serde_json::Value::Object(map));
                        }
                    }
                }
                crate::db::DbStore::Sqlite(pool) => {
                    let query_str = if mobile_optimized {
                        "SELECT id, status, CAST(created_at AS TEXT) AS created_at FROM task_envelopes WHERE tenant_id = ? AND status NOT IN ('COMPLETED', 'APPROVED', 'DISMISSED') ORDER BY created_at DESC LIMIT 50"
                    } else {
                        "SELECT id, current_department, status, payload, routing_history, CAST(created_at AS TEXT) AS created_at FROM task_envelopes WHERE tenant_id = ? AND status NOT IN ('COMPLETED', 'APPROVED', 'DISMISSED') ORDER BY created_at DESC LIMIT 50"
                    };
                    if let Ok(rows) = sqlx::query(query_str).bind(&t_id5).fetch_all(pool).await {
                        for row in rows {
                            use sqlx::Row;
                            let mut map = serde_json::Map::new();
                            map.insert(
                                "id".to_string(),
                                serde_json::json!(
                                    row.try_get::<String, _>("id").unwrap_or_default()
                                ),
                            );
                            map.insert("tenant_id".to_string(), serde_json::json!(t_id5));
                            map.insert("intent".to_string(), serde_json::json!("task_envelope"));
                            map.insert(
                                "status".to_string(),
                                serde_json::json!(
                                    row.try_get::<String, _>("status").unwrap_or_default()
                                ),
                            );
                            map.insert(
                                "created_at".to_string(),
                                serde_json::json!(
                                    row.try_get::<String, _>("created_at").unwrap_or_default()
                                ),
                            );

                            if !mobile_optimized {
                                let dept = row
                                    .try_get::<String, _>("current_department")
                                    .unwrap_or_default();
                                map.insert(
                                    "customer_info".to_string(),
                                    serde_json::json!({ "department": dept }),
                                );
                                let payload_str: String =
                                    row.try_get("payload").unwrap_or_else(|_| "{}".to_string());
                                let payload_val: serde_json::Value =
                                    serde_json::from_str(&payload_str)
                                        .unwrap_or_else(|_| serde_json::json!({}));
                                map.insert("suggested_actions".to_string(), payload_val);
                            }

                            envelope_rows_json.push(serde_json::Value::Object(map));
                        }
                    }
                }
            }
            envelope_rows_json
        })
    );

    if let Ok(legacy_rows) = legacy_res {
        results.extend(legacy_rows);
    }
    if let Ok(feed_rows) = feed_res {
        results.extend(feed_rows);
    }
    if let Ok(approvals_rows) = approvals_res {
        results.extend(approvals_rows);
    }
    if let Ok(daily_work_rows) = daily_work_res {
        results.extend(daily_work_rows);
    }
    if let Ok(envelope_rows) = envelope_res {
        results.extend(envelope_rows);
    }

    // Sort combined results by created_at DESC
    results.sort_by(|a, b| {
        let a_time = a.get("created_at").and_then(|t| t.as_str()).unwrap_or("");
        let b_time = b.get("created_at").and_then(|t| t.as_str()).unwrap_or("");
        b_time.cmp(a_time)
    });

    Ok(results)
}

pub(crate) async fn load_ui_priority_tasks_from_db(
    db: &crate::db::DB,
    tenant_id: &str,
    mobile_optimized: bool,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let limit = 20i64;
    match &db.store {
        crate::db::DbStore::Postgres => {
            let mut tx = db.pool.begin().await?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id).await?;
            let res = if mobile_optimized {
                sqlx::query(
                "SELECT id, title, status, created_at, updated_at FROM shared_tasks WHERE (organization_id = $1 OR tenant_id = $1) AND status IN ('PENDING', 'IN_PROGRESS') ORDER BY created_at DESC LIMIT $2"
            )
            .bind(tenant_id)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            .map(|rows| rows.into_iter().map(|row| {
                serde_json::json!({
                    "id": row.get::<String, _>("id"),
                    "title": row.try_get::<String, _>("title").unwrap_or_default(),
                    "status": row.try_get::<String, _>("status").unwrap_or_default(),
                    "created_at": row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at").map(|dt| dt.to_rfc3339()).unwrap_or_default(),
                    "updated_at": row.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at").map(|dt| dt.to_rfc3339()).unwrap_or_default(),
                })
            }).collect::<Vec<_>>())
            } else {
                sqlx::query(
                "SELECT id, title, description, status, created_at, updated_at FROM shared_tasks WHERE (organization_id = $1 OR tenant_id = $1) AND status IN ('PENDING', 'IN_PROGRESS') ORDER BY created_at DESC LIMIT $2"
            )
            .bind(tenant_id)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            .map(|rows| rows.into_iter().map(|row| {
                serde_json::json!({
                    "id": row.get::<String, _>("id"),
                    "title": row.try_get::<String, _>("title").unwrap_or_default(),
                    "description": row.try_get::<String, _>("description").unwrap_or_default(),
                    "status": row.try_get::<String, _>("status").unwrap_or_default(),
                    "created_at": row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at").map(|dt| dt.to_rfc3339()).unwrap_or_default(),
                    "updated_at": row.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at").map(|dt| dt.to_rfc3339()).unwrap_or_default(),
                })
            }).collect::<Vec<_>>())
            };
            tx.commit().await?;
            res
        }
        crate::db::DbStore::Sqlite(pool) => {
            if mobile_optimized {
                sqlx::query("SELECT id, title, status, CAST(created_at AS TEXT) AS created_at, CAST(updated_at AS TEXT) AS updated_at FROM shared_tasks WHERE (organization_id = ? OR tenant_id = ?) AND status IN ('PENDING', 'IN_PROGRESS') ORDER BY created_at DESC LIMIT ?")
                .bind(tenant_id)
                .bind(tenant_id)
                .bind(limit)
                .fetch_all(pool)
                .await
                .map(|rows| rows.into_iter().map(|row| {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "title": row.try_get::<String, _>("title").unwrap_or_default(),
                        "status": row.try_get::<String, _>("status").unwrap_or_default(),
                        "created_at": row.try_get::<String, _>("created_at").unwrap_or_default(),
                        "updated_at": row.try_get::<String, _>("updated_at").unwrap_or_default(),
                    })
                }).collect::<Vec<_>>())
            } else {
                sqlx::query("SELECT id, title, description, status, CAST(created_at AS TEXT) AS created_at, CAST(updated_at AS TEXT) AS updated_at FROM shared_tasks WHERE (organization_id = ? OR tenant_id = ?) AND status IN ('PENDING', 'IN_PROGRESS') ORDER BY created_at DESC LIMIT ?")
                .bind(tenant_id)
                .bind(tenant_id)
                .bind(limit)
                .fetch_all(pool)
                .await
                .map(|rows| rows.into_iter().map(|row| {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "title": row.try_get::<String, _>("title").unwrap_or_default(),
                        "description": row.try_get::<String, _>("description").unwrap_or_default(),
                        "status": row.try_get::<String, _>("status").unwrap_or_default(),
                        "created_at": row.try_get::<String, _>("created_at").unwrap_or_default(),
                        "updated_at": row.try_get::<String, _>("updated_at").unwrap_or_default(),
                    })
                }).collect::<Vec<_>>())
            }
        }
    }
}

pub(crate) async fn load_ui_agent_feed_from_db(
    db: &crate::db::DB,
    tenant_id: &str,
    _mobile_optimized: bool,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let limit = 20i64;
    let query_pg = r#"
        SELECT id, tenant_id, event_source, context_payload, proposed_action, lifecycle_state, created_at, updated_at FROM agent_feed_items WHERE tenant_id = $1
        UNION ALL
        SELECT id, tenant_id, department as event_source, jsonb_build_object('description', description) as context_payload, payload as proposed_action, CASE WHEN status = 'DRAFT' THEN 'PENDING_APPROVAL' WHEN status = 'REJECTED' THEN 'DISMISSED' ELSE status END as lifecycle_state, created_at, updated_at FROM agent_approvals WHERE tenant_id = $1 AND status IN ('DRAFT', 'PAUSED', 'APPROVED', 'REJECTED', 'DISMISSED') AND NOT EXISTS (SELECT 1 FROM agent_feed_items canonical WHERE canonical.tenant_id = agent_approvals.tenant_id AND canonical.id = agent_approvals.id)
        UNION ALL
        SELECT id, tenant_id, COALESCE(agent_type, department_type, 'operations') as event_source, jsonb_build_object('description', COALESCE(description, 'Action Request: ' || action_type)) as context_payload, payload as proposed_action, CASE WHEN UPPER(status) IN ('PENDING', 'DRAFT') THEN 'PENDING_APPROVAL' WHEN UPPER(status) IN ('REJECTED', 'DISMISSED') THEN 'DISMISSED' WHEN UPPER(status) = 'APPROVED' THEN 'APPROVED' WHEN UPPER(status) = 'PAUSED' THEN 'PAUSED' ELSE UPPER(status) END as lifecycle_state, created_at, updated_at FROM agent_action_requests WHERE tenant_id = $1 AND UPPER(status) IN ('PENDING', 'APPROVED', 'REJECTED', 'DRAFT', 'PAUSED') AND NOT EXISTS (SELECT 1 FROM agent_feed_items canonical WHERE canonical.tenant_id = agent_action_requests.tenant_id AND canonical.id = agent_action_requests.id)
        UNION ALL
        SELECT id, tenant_id, COALESCE(source, 'omni_inbox') as event_source, jsonb_build_object('customer_message', COALESCE(original_content, ''), 'feature_type', CASE WHEN source = 'Instagram DM' THEN 'instagram_dm' ELSE 'omni_inbox' END) as context_payload, jsonb_build_object('draft_reply', COALESCE(draft_reply, ''), 'action_type', 'Draft Reply', 'feature_type', CASE WHEN source = 'Instagram DM' THEN 'instagram_dm' ELSE 'omni_inbox' END) as proposed_action, 'PENDING_APPROVAL' as lifecycle_state, created_at, updated_at FROM omni_inbox_messages WHERE tenant_id = $1 AND status NOT IN ('resolved', 'dismissed', 'sent', 'processed') AND NOT EXISTS (SELECT 1 FROM agent_feed_items canonical WHERE canonical.tenant_id = omni_inbox_messages.tenant_id AND canonical.id = omni_inbox_messages.id)
        UNION ALL
        SELECT id, tenant_id, 'orders' as event_source, jsonb_build_object('description', 'Pending Order') as context_payload, jsonb_build_object('message', 'Process Order') as proposed_action, 'PENDING_APPROVAL' as lifecycle_state, created_at, updated_at FROM orders WHERE tenant_id = $1 AND status = 'pending' AND NOT EXISTS (SELECT 1 FROM agent_feed_items canonical WHERE canonical.tenant_id = orders.tenant_id AND canonical.id = orders.id)
        UNION ALL
        SELECT id, tenant_id, 'invoices' as event_source, jsonb_build_object('description', 'Action Required: Overdue Invoice') as context_payload, jsonb_build_object('message', 'Send Reminder') as proposed_action, 'PENDING_APPROVAL' as lifecycle_state, created_at, updated_at FROM invoices WHERE tenant_id = $1 AND status IN ('draft', 'overdue') AND NOT EXISTS (SELECT 1 FROM agent_feed_items canonical WHERE canonical.tenant_id = invoices.tenant_id AND canonical.id = invoices.id)
        ORDER BY created_at DESC LIMIT $2
    "#;
    let query_sqlite = r#"
        SELECT id, tenant_id, event_source, context_payload, proposed_action, lifecycle_state, created_at, updated_at FROM agent_feed_items WHERE tenant_id = ?
        UNION ALL
        SELECT id, tenant_id, department as event_source, json_object('description', description) as context_payload, payload as proposed_action, CASE WHEN status = 'DRAFT' THEN 'PENDING_APPROVAL' WHEN status = 'REJECTED' THEN 'DISMISSED' ELSE status END as lifecycle_state, created_at, updated_at FROM agent_approvals WHERE tenant_id = ? AND status IN ('DRAFT', 'PAUSED', 'APPROVED', 'REJECTED', 'DISMISSED') AND NOT EXISTS (SELECT 1 FROM agent_feed_items canonical WHERE canonical.tenant_id = agent_approvals.tenant_id AND canonical.id = agent_approvals.id)
        UNION ALL
        SELECT id, tenant_id, COALESCE(agent_type, department_type, 'operations') as event_source, json_object('description', COALESCE(description, 'Action Request: ' || action_type)) as context_payload, payload as proposed_action, CASE WHEN UPPER(status) IN ('PENDING', 'DRAFT') THEN 'PENDING_APPROVAL' WHEN UPPER(status) IN ('REJECTED', 'DISMISSED') THEN 'DISMISSED' WHEN UPPER(status) = 'APPROVED' THEN 'APPROVED' WHEN UPPER(status) = 'PAUSED' THEN 'PAUSED' ELSE UPPER(status) END as lifecycle_state, created_at, updated_at FROM agent_action_requests WHERE tenant_id = ? AND UPPER(status) IN ('PENDING', 'APPROVED', 'REJECTED', 'DRAFT', 'PAUSED') AND NOT EXISTS (SELECT 1 FROM agent_feed_items canonical WHERE canonical.tenant_id = agent_action_requests.tenant_id AND canonical.id = agent_action_requests.id)
        UNION ALL
        SELECT id, tenant_id, COALESCE(source, 'omni_inbox') as event_source, json_object('customer_message', COALESCE(original_content, ''), 'feature_type', CASE WHEN source = 'Instagram DM' THEN 'instagram_dm' ELSE 'omni_inbox' END) as context_payload, json_object('draft_reply', COALESCE(draft_reply, ''), 'action_type', 'Draft Reply', 'feature_type', CASE WHEN source = 'Instagram DM' THEN 'instagram_dm' ELSE 'omni_inbox' END) as proposed_action, 'PENDING_APPROVAL' as lifecycle_state, created_at, updated_at FROM omni_inbox_messages WHERE tenant_id = ? AND status NOT IN ('resolved', 'dismissed', 'sent', 'processed') AND NOT EXISTS (SELECT 1 FROM agent_feed_items canonical WHERE canonical.tenant_id = omni_inbox_messages.tenant_id AND canonical.id = omni_inbox_messages.id)
        UNION ALL
        SELECT id, tenant_id, 'orders' as event_source, json_object('description', 'Pending Order') as context_payload, json_object('message', 'Process Order') as proposed_action, 'PENDING_APPROVAL' as lifecycle_state, created_at, updated_at FROM orders WHERE tenant_id = ? AND status = 'pending' AND NOT EXISTS (SELECT 1 FROM agent_feed_items canonical WHERE canonical.tenant_id = orders.tenant_id AND canonical.id = orders.id)
        UNION ALL
        SELECT id, tenant_id, 'invoices' as event_source, json_object('description', 'Action Required: Overdue Invoice') as context_payload, json_object('message', 'Send Reminder') as proposed_action, 'PENDING_APPROVAL' as lifecycle_state, created_at, updated_at FROM invoices WHERE tenant_id = ? AND status IN ('draft', 'overdue') AND NOT EXISTS (SELECT 1 FROM agent_feed_items canonical WHERE canonical.tenant_id = invoices.tenant_id AND canonical.id = invoices.id)
        ORDER BY created_at DESC LIMIT ?
    "#;
    match &db.store {
        crate::db::DbStore::Postgres => {
            let mut tx = db.pool.begin().await?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id).await?;
            let rows =
                sqlx::query_as::<_, crate::domain::repository::agent_feed_repo::AgentFeedItem>(
                    query_pg,
                )
                .bind(tenant_id)
                .bind(limit)
                .fetch_all(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok(rows
                .into_iter()
                .map(|item| {
                    let prop = item.proposed_action.map(|j| j.0);
                    let ctx = item.context_payload.map(|j| j.0);
                    serde_json::json!({
                        "id": item.id,
                        "tenant_id": item.tenant_id,
                        "event_source": item.event_source.clone(),
                        "source": item.event_source,
                        "context_payload": ctx,
                        "proposed_action": prop.clone(),
                        "payload": prop,
                        "lifecycle_state": item.lifecycle_state,
                        "created_at": item.created_at.map(|dt| dt.to_rfc3339()).unwrap_or_default(),
                        "updated_at": item.updated_at.map(|dt| dt.to_rfc3339()).unwrap_or_default(),
                    })
                })
                .collect())
        }
        crate::db::DbStore::Sqlite(pool) => {
            let rows =
                sqlx::query_as::<_, crate::domain::repository::agent_feed_repo::AgentFeedItem>(
                    query_sqlite,
                )
                .bind(tenant_id)
                .bind(tenant_id)
                .bind(tenant_id)
                .bind(tenant_id)
                .bind(tenant_id)
                .bind(tenant_id)
                .bind(limit)
                .fetch_all(pool)
                .await?;
            Ok(rows
                .into_iter()
                .map(|item| {
                    let prop = item.proposed_action.map(|j| j.0);
                    let ctx = item.context_payload.map(|j| j.0);
                    serde_json::json!({
                        "id": item.id,
                        "tenant_id": item.tenant_id,
                        "event_source": item.event_source.clone(),
                        "source": item.event_source,
                        "context_payload": ctx,
                        "proposed_action": prop.clone(),
                        "payload": prop,
                        "lifecycle_state": item.lifecycle_state,
                        "created_at": item.created_at.map(|dt| dt.to_rfc3339()).unwrap_or_default(),
                        "updated_at": item.updated_at.map(|dt| dt.to_rfc3339()).unwrap_or_default(),
                    })
                })
                .collect())
        }
    }
}
