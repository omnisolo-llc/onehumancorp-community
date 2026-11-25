// SPDX-License-Identifier: Apache-2.0
//! UI inbox, supply, agent-approvals, and ledger readers.

use sqlx::Row;

pub(crate) async fn load_ui_inbox_from_db(
    db: &crate::db::DB,
    tenant_id: &str,
    mobile_optimized: bool,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    match &db.store {
        crate::db::DbStore::Postgres => {
            let mut tx = db.pool.begin().await?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id).await?;
            let res = if mobile_optimized {
                sqlx::query("SELECT id, COALESCE(source, '') AS source, COALESCE(status, '') AS status, CAST(created_at AS text) AS created_at FROM inbox_messages WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT 50")
                .bind(tenant_id)
                .fetch_all(&mut *tx)
                .await.map(|rows| rows.into_iter().map(|row| {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "source": row.get::<String, _>("source"),
                        "status": row.get::<String, _>("status"),
                        "created_at": row.get::<String, _>("created_at")
                    })
                }).collect())
            } else {
                sqlx::query("SELECT id, COALESCE(source, '') AS source, COALESCE(content, '') AS content, COALESCE(original_content, content, '') AS original_content, COALESCE(translated_from_language, '') AS translated_from_language, COALESCE(draft_reply, '') AS draft_reply, COALESCE(status, '') AS status, COALESCE(sender_id, '') AS sender_id, COALESCE(customer_id, '') AS customer_id, CAST(created_at AS text) AS created_at FROM inbox_messages WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT 50")
                .bind(tenant_id)
                .fetch_all(&mut *tx)
                .await.map(|rows| rows.into_iter().map(|row| {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "source": row.get::<String, _>("source"),
                        "content": row.get::<String, _>("content"),
                        "original_message": row.get::<String, _>("original_content"),
                        "translated_from_language": row.get::<String, _>("translated_from_language"),
                        "generated_response": row.get::<String, _>("draft_reply"),
                        "status": row.get::<String, _>("status"),
                        "sender_id": row.get::<String, _>("sender_id"),
                        "customer_id": row.get::<String, _>("customer_id"),
                        "created_at": row.get::<String, _>("created_at")
                    })
                }).collect())
            };
            tx.commit().await?;
            res
        }
        crate::db::DbStore::Sqlite(pool) => {
            if mobile_optimized {
                sqlx::query("SELECT id, COALESCE(source, '') AS source, COALESCE(status, '') AS status, CAST(created_at AS TEXT) AS created_at FROM inbox_messages WHERE tenant_id = ? ORDER BY created_at DESC LIMIT 50")
                .bind(tenant_id)
                .fetch_all(pool)
                .await.map(|rows| rows.into_iter().map(|row| {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "source": row.get::<String, _>("source"),
                        "status": row.get::<String, _>("status"),
                        "created_at": row.get::<String, _>("created_at")
                    })
                }).collect())
            } else {
                sqlx::query("SELECT id, COALESCE(source, '') AS source, COALESCE(content, '') AS content, COALESCE(original_content, content, '') AS original_content, COALESCE(translated_from_language, '') AS translated_from_language, COALESCE(draft_reply, '') AS draft_reply, COALESCE(status, '') AS status, COALESCE(sender_id, '') AS sender_id, CAST(created_at AS TEXT) AS created_at FROM inbox_messages WHERE tenant_id = ? ORDER BY created_at DESC LIMIT 50")
                .bind(tenant_id)
                .fetch_all(pool)
                .await.map(|rows| rows.into_iter().map(|row| {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "source": row.get::<String, _>("source"),
                        "content": row.get::<String, _>("content"),
                        "original_message": row.get::<String, _>("original_content"),
                        "translated_from_language": row.get::<String, _>("translated_from_language"),
                        "generated_response": row.get::<String, _>("draft_reply"),
                        "status": row.get::<String, _>("status"),
                        "sender_id": row.get::<String, _>("sender_id"),
                        "customer_id": row.get::<String, _>("customer_id"),
                        "created_at": row.get::<String, _>("created_at")
                    })
                }).collect())
            }
        }
    }
}

pub(crate) async fn load_ui_supply_from_db(
    db: &crate::db::DB,
    tenant_id: &str,
    mobile_optimized: bool,
) -> Result<serde_json::Value, sqlx::Error> {
    // Query and decode errors remain unavailable data, never empty inventory.
    // The same transaction installs PostgreSQL RLS context and owns all reads.
    match &db.store {
        crate::db::DbStore::Postgres => {
            let mut tx = db.pool.begin().await?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id).await?;
            let vendors = sqlx::query("SELECT id, name, COALESCE(contact_info, '') AS contact_info FROM vendors WHERE tenant_id = $1 ORDER BY name")
                .bind(tenant_id).fetch_all(&mut *tx).await?
                .into_iter().map(|row| {
                    let mut value = serde_json::json!({"id": row.try_get::<String, _>("id")?, "name": row.try_get::<String, _>("name")?});
                    if !mobile_optimized { value["contact_info"] = serde_json::json!(row.try_get::<String, _>("contact_info")?); }
                    Ok(value)
                }).collect::<Result<Vec<_>, sqlx::Error>>()?;
            let raw_materials = sqlx::query("SELECT id, name, current_quantity, reorder_threshold FROM raw_materials WHERE tenant_id = $1 ORDER BY name")
                .bind(tenant_id).fetch_all(&mut *tx).await?
                .into_iter().map(|row| {
                    Ok(serde_json::json!({"id": row.try_get::<String, _>("id")?, "name": row.try_get::<String, _>("name")?, "current_quantity": row.try_get::<i32, _>("current_quantity")?, "reorder_threshold": row.try_get::<i32, _>("reorder_threshold")?}))
                }).collect::<Result<Vec<_>, sqlx::Error>>()?;
            let bom_items = sqlx::query("SELECT id, finished_good_id, raw_material_id, quantity_required FROM bom_items WHERE tenant_id = $1 ORDER BY id")
                .bind(tenant_id).fetch_all(&mut *tx).await?
                .into_iter().map(|row| {
                    Ok(serde_json::json!({"id": row.try_get::<String, _>("id")?, "finished_good_id": row.try_get::<String, _>("finished_good_id")?, "raw_material_id": row.try_get::<String, _>("raw_material_id")?, "quantity_required": row.try_get::<i32, _>("quantity_required")?}))
                }).collect::<Result<Vec<_>, sqlx::Error>>()?;
            tx.commit().await?;
            Ok(
                serde_json::json!({ "vendors": vendors, "raw_materials": raw_materials, "bom_items": bom_items }),
            )
        }
        crate::db::DbStore::Sqlite(pool) => {
            let mut tx = pool.begin().await?;
            let vendors = sqlx::query("SELECT id, name, COALESCE(contact_info, '') AS contact_info FROM vendors WHERE tenant_id = ? ORDER BY name")
                .bind(tenant_id).fetch_all(&mut *tx).await?
                .into_iter().map(|row| {
                    let mut value = serde_json::json!({"id": row.try_get::<String, _>("id")?, "name": row.try_get::<String, _>("name")?});
                    if !mobile_optimized { value["contact_info"] = serde_json::json!(row.try_get::<String, _>("contact_info")?); }
                    Ok(value)
                }).collect::<Result<Vec<_>, sqlx::Error>>()?;
            let raw_materials = sqlx::query("SELECT id, name, current_quantity, reorder_threshold FROM raw_materials WHERE tenant_id = ? ORDER BY name")
                .bind(tenant_id).fetch_all(&mut *tx).await?
                .into_iter().map(|row| {
                    Ok(serde_json::json!({"id": row.try_get::<String, _>("id")?, "name": row.try_get::<String, _>("name")?, "current_quantity": row.try_get::<i32, _>("current_quantity")?, "reorder_threshold": row.try_get::<i32, _>("reorder_threshold")?}))
                }).collect::<Result<Vec<_>, sqlx::Error>>()?;
            let bom_items = sqlx::query("SELECT id, finished_good_id, raw_material_id, quantity_required FROM bom_items WHERE tenant_id = ? ORDER BY id")
                .bind(tenant_id).fetch_all(&mut *tx).await?
                .into_iter().map(|row| {
                    Ok(serde_json::json!({"id": row.try_get::<String, _>("id")?, "finished_good_id": row.try_get::<String, _>("finished_good_id")?, "raw_material_id": row.try_get::<String, _>("raw_material_id")?, "quantity_required": row.try_get::<i32, _>("quantity_required")?}))
                }).collect::<Result<Vec<_>, sqlx::Error>>()?;
            tx.commit().await?;
            Ok(
                serde_json::json!({ "vendors": vendors, "raw_materials": raw_materials, "bom_items": bom_items }),
            )
        }
    }
}

pub(crate) async fn load_ui_agent_approvals_from_db(
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
                sqlx::query("SELECT id, department, description, status, action_risk FROM agent_approvals WHERE tenant_id = $1 AND status IN ('DRAFT', 'PAUSED') ORDER BY id ASC LIMIT $2")
                .bind(tenant_id)
                .bind(limit)
                .fetch_all(&mut *tx)
                .await.map(|rows| rows.into_iter().map(|row| {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "department": row.get::<String, _>("department"),
                        "description": row.get::<String, _>("description"),
                        "status": row.get::<String, _>("status"),
                        "action_risk": row.get::<String, _>("action_risk")
                    })
                }).collect())
            } else {
                sqlx::query("SELECT id, tenant_id, department, description, status, action_risk, payload FROM agent_approvals WHERE tenant_id = $1 AND status IN ('DRAFT', 'PAUSED') ORDER BY id ASC LIMIT $2")
                .bind(tenant_id)
                .bind(limit)
                .fetch_all(&mut *tx)
                .await.map(|rows| rows.into_iter().map(|row| {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "tenant_id": row.get::<String, _>("tenant_id"),
                        "department": row.get::<String, _>("department"),
                        "description": row.get::<String, _>("description"),
                        "status": row.get::<String, _>("status"),
                        "action_risk": row.get::<String, _>("action_risk"),
                        "payload": row.try_get::<Option<sqlx::types::Json<serde_json::Value>>, _>("payload")
                            .ok()
                            .flatten()
                            .map(|j| j.0)
                            .or_else(|| {
                                row.try_get::<Option<String>, _>("payload")
                                    .ok()
                                    .flatten()
                                    .and_then(|s| serde_json::from_str(&s).ok())
                            })
                    })
                }).collect())
            };
            tx.commit().await?;
            res
        }
        crate::db::DbStore::Sqlite(pool) => {
            if mobile_optimized {
                sqlx::query("SELECT id, department, description, status, action_risk FROM agent_approvals WHERE tenant_id = ? AND status IN ('DRAFT', 'PAUSED') ORDER BY id ASC LIMIT ?")
                .bind(tenant_id)
                .bind(limit)
                .fetch_all(pool)
                .await.map(|rows| rows.into_iter().map(|row| {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "department": row.get::<String, _>("department"),
                        "description": row.get::<String, _>("description"),
                        "status": row.get::<String, _>("status"),
                        "action_risk": row.get::<String, _>("action_risk")
                    })
                }).collect())
            } else {
                sqlx::query("SELECT id, tenant_id, department, description, status, action_risk, payload FROM agent_approvals WHERE tenant_id = ? AND status IN ('DRAFT', 'PAUSED') ORDER BY id ASC LIMIT ?")
                .bind(tenant_id)
                .bind(limit)
                .fetch_all(pool)
                .await.map(|rows| rows.into_iter().map(|row| {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "tenant_id": row.get::<String, _>("tenant_id"),
                        "department": row.get::<String, _>("department"),
                        "description": row.get::<String, _>("description"),
                        "status": row.get::<String, _>("status"),
                        "action_risk": row.get::<String, _>("action_risk"),
                        "payload": row.get::<Option<String>, _>("payload").and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    })
                }).collect())
            }
        }
    }
}

pub(crate) async fn load_ui_ledger_from_db(
    db: &crate::db::DB,
    tenant_id: &str,
    mobile_optimized: bool,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let limit_ledger = 50i64;
    match &db.store {
    crate::db::DbStore::Postgres => {
        let mut tx = db.pool.begin().await?;
        ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id).await?;
        let res = if mobile_optimized { sqlx::query("SELECT id, event_type, department, created_at FROM ohc_universal_ledger WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2") } else { sqlx::query("SELECT id, tenant_id, event_type, department, payload, created_at FROM ohc_universal_ledger WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2") }
            .bind(tenant_id)
            .bind(limit_ledger)
            .fetch_all(&mut *tx)
            .await.map(|rows| rows.into_iter().map(|row| {
                if mobile_optimized {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "event_type": row.get::<String, _>("event_type"),
                        "department": row.get::<String, _>("department"),
                        "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339()
                    })
                } else {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "tenant_id": row.get::<String, _>("tenant_id"),
                        "event_type": row.get::<String, _>("event_type"),
                        "department": row.get::<String, _>("department"),
                        "payload": row.get::<serde_json::Value, _>("payload"),
                        "created_at": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339()
                    })
                }
            }).collect());
        tx.commit().await?;
        res
    },
    crate::db::DbStore::Sqlite(pool) => {
        if mobile_optimized { sqlx::query("SELECT id, event_type, department, created_at FROM ohc_universal_ledger WHERE tenant_id = ? ORDER BY created_at DESC LIMIT ?") } else { sqlx::query("SELECT id, tenant_id, event_type, department, payload, created_at FROM ohc_universal_ledger WHERE tenant_id = ? ORDER BY created_at DESC LIMIT ?") }
            .bind(tenant_id)
            .bind(limit_ledger)
            .fetch_all(pool)
            .await.map(|rows| rows.into_iter().map(|row| {
                if mobile_optimized {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "event_type": row.get::<String, _>("event_type"),
                        "department": row.get::<String, _>("department"),
                        "created_at": row.get::<String, _>("created_at")
                    })
                } else {
                    serde_json::json!({
                        "id": row.get::<String, _>("id"),
                        "tenant_id": row.get::<String, _>("tenant_id"),
                        "event_type": row.get::<String, _>("event_type"),
                        "department": row.get::<String, _>("department"),
                        "payload": serde_json::from_str::<serde_json::Value>(&row.get::<String, _>("payload")).unwrap_or_else(|_| serde_json::json!({})),
                        "created_at": row.get::<String, _>("created_at")
                    })
                }
            }).collect())
    }
}
}
