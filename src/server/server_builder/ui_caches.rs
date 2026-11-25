// SPDX-License-Identifier: Apache-2.0
//! Shared UI handler caches, tenant claim validation, and legacy triage invalidation.

use sqlx::Row;
use std::sync::OnceLock;

pub(crate) static ORG_CACHE_ADVISORY: OnceLock<
    ::server_utils::cache::HybridCache<Option<(String, String)>>,
> = OnceLock::new();
pub(crate) static ACTIVE_ORDERS_CACHE: OnceLock<::server_utils::cache::HybridCache<i64>> =
    OnceLock::new();
pub(crate) static ADVISORY_INSIGHT_CACHE: OnceLock<::server_utils::cache::HybridCache<String>> =
    OnceLock::new();
pub static AI_CACHE: OnceLock<::server_utils::cache::HybridCache<String>> = OnceLock::new();

pub(crate) static UI_ORDERS_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<Vec<serde_json::Value>>,
> = OnceLock::new();
pub(crate) static UI_BOOKINGS_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<Vec<serde_json::Value>>,
> = OnceLock::new();
pub(crate) static UI_INBOX_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<Vec<serde_json::Value>>,
> = OnceLock::new();
pub(crate) static UI_OMNI_INBOX_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<Vec<serde_json::Value>>,
> = OnceLock::new();
pub(crate) static UI_DASHBOARD_METRICS_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<serde_json::Value>,
> = OnceLock::new();
pub(crate) static UI_AGENT_FEED_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<Vec<serde_json::Value>>,
> = OnceLock::new();
pub(crate) static UI_UNIFIED_FEED_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<serde_json::Value>,
> = OnceLock::new();
pub(crate) static UI_UNIFIED_AGENT_FEED_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<serde_json::Value>,
> = OnceLock::new();
pub(crate) static UI_LEDGER_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<Vec<serde_json::Value>>,
> = OnceLock::new();
pub(crate) static UI_TRIAGE_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<Vec<serde_json::Value>>,
> = OnceLock::new();
pub(crate) static UI_ANALYTICS_BRIEFING_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<serde_json::Value>,
> = OnceLock::new();
pub(crate) static UI_ANALYTICS_CHAT_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<serde_json::Value>,
> = OnceLock::new();
pub(crate) static METRICS_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<super::metrics_handlers::HttpMetricsResponse>,
> = OnceLock::new();
pub(crate) static UI_PRIORITY_TASKS_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<Vec<serde_json::Value>>,
> = OnceLock::new();
pub(crate) static UI_AGENT_APPROVALS_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<Vec<serde_json::Value>>,
> = OnceLock::new();
pub(crate) static UI_INVOICES_CACHE: OnceLock<
    ::server_utils::cache::HybridCache<Vec<serde_json::Value>>,
> = OnceLock::new();

#[derive(serde::Deserialize)]
pub struct CreateTriageItemPayload {
    pub source: Option<String>,
    pub priority: Option<String>,
    pub context: Option<String>,
    pub customer_id: Option<String>,
    pub action_type: Option<String>,
    pub action_payload: Option<String>,
}

pub use crate::orchestration::departments::message_delivery::manual_inbox::ManualAction as OmniInboxActionPayload;

pub(crate) fn strict_ui_claim_tenant(claims: &::server_common::Claims) -> Option<String> {
    claims
        .organization_id
        .as_deref()
        .map(str::trim)
        .filter(|tenant_id| !tenant_id.is_empty() && !tenant_id.eq_ignore_ascii_case("system"))
        .map(str::to_string)
}

/// Invalidate every cached representation after a durable agent-feed mutation.
pub async fn invalidate_agent_feed_caches(tenant_id: &str) {
    crate::api::agent_feed::get_agent_feed_cache()
        .invalidate_by_tag(&format!("agent_feed_tenant:{tenant_id}"))
        .await;
    let tag = format!("ui_feed_tenant:{tenant_id}");
    if let Some(cache) = UI_AGENT_FEED_CACHE.get() {
        cache.invalidate_by_tag(&tag).await;
    }
    if let Some(cache) = UI_UNIFIED_FEED_CACHE.get() {
        cache.invalidate_by_tag(&tag).await;
    }
    if let Some(cache) = UI_UNIFIED_AGENT_FEED_CACHE.get() {
        cache.invalidate_by_tag(&tag).await;
    }
}

pub async fn invalidate_legacy_triage_caches(tenant_id: &str) {
    crate::api::work_triage::invalidate_daily_work_cache(tenant_id).await;
    if let Some(cache) = UI_TRIAGE_CACHE.get() {
        for mobile in [false, true] {
            cache
                .invalidate(&format!("ui_triage:{tenant_id}:mobile:{mobile}"))
                .await;
        }
    }
    invalidate_agent_feed_caches(tenant_id).await;
}

pub(crate) async fn invalidate_ui_omni_inbox_cache(tenant_id: &str) {
    let cache = UI_OMNI_INBOX_CACHE
        .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));
    for mobile_optimized in [false, true] {
        cache
            .invalidate(&format!(
                "ui_omni_inbox:{}:mobile:{}",
                tenant_id, mobile_optimized
            ))
            .await;
    }
}

pub(crate) async fn load_ui_omni_inbox_from_db(
    db: &crate::db::DB,
    tenant_id: &str,
    mobile_optimized: bool,
) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    match &db.store {
        crate::db::DbStore::Postgres => {
            let mut tx = db.pool.begin().await?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id).await?;
            let items = if mobile_optimized {
                sqlx::query("SELECT id, COALESCE(source, '') AS source, COALESCE(status, '') AS status, COALESCE(sender_id, '') AS sender_id, COALESCE(customer_id, '') AS customer_id, CAST(created_at AS text) AS created_at FROM omni_inbox_messages WHERE tenant_id = $1 AND status != 'resolved' ORDER BY created_at DESC LIMIT 50")
                    .bind(tenant_id)
                    .fetch_all(&mut *tx)
                    .await?.into_iter().map(|row| {
                        serde_json::json!({
                            "id": row.get::<String, _>("id"),
                            "source": row.get::<String, _>("source"),
                            "status": row.get::<String, _>("status"),
                            "sender_id": row.get::<String, _>("sender_id"),
                            "customer_id": row.get::<String, _>("customer_id"),
                            "created_at": row.get::<String, _>("created_at")
                        })
                    }).collect()
            } else {
                sqlx::query("SELECT id, COALESCE(source, '') AS source, COALESCE(original_content, '') AS original_content, COALESCE(draft_reply, '') AS draft_reply, COALESCE(status, '') AS status, COALESCE(sender_id, '') AS sender_id, COALESCE(customer_id, '') AS customer_id, CAST(created_at AS text) AS created_at FROM omni_inbox_messages WHERE tenant_id = $1 AND status != 'resolved' ORDER BY created_at DESC LIMIT 50")
                    .bind(tenant_id)
                    .fetch_all(&mut *tx)
                    .await?.into_iter().map(|row| {
                        let original_content = row.get::<String, _>("original_content");
                        serde_json::json!({
                            "id": row.get::<String, _>("id"),
                            "source": row.get::<String, _>("source"),
                            "content": original_content,
                            "original_content": original_content,
                            "draft_reply": row.get::<String, _>("draft_reply"),
                            "status": row.get::<String, _>("status"),
                            "sender_id": row.get::<String, _>("sender_id"),
                            "customer_id": row.get::<String, _>("customer_id"),
                            "created_at": row.get::<String, _>("created_at")
                        })
                    }).collect()
            };
            tx.commit().await?;
            Ok(items)
        }
        crate::db::DbStore::Sqlite(pool) => {
            if mobile_optimized {
                sqlx::query("SELECT id, COALESCE(source, '') AS source, COALESCE(status, '') AS status, COALESCE(sender_id, '') AS sender_id, COALESCE(customer_id, '') AS customer_id, CAST(created_at AS TEXT) AS created_at FROM omni_inbox_messages WHERE tenant_id = ? AND status != 'resolved' ORDER BY created_at DESC LIMIT 50")
                    .bind(tenant_id)
                    .fetch_all(pool)
                    .await.map(|rows| rows.into_iter().map(|row| {
                        serde_json::json!({
                            "id": row.get::<String, _>("id"),
                            "source": row.get::<String, _>("source"),
                            "status": row.get::<String, _>("status"),
                            "sender_id": row.get::<String, _>("sender_id"),
                            "customer_id": row.get::<String, _>("customer_id"),
                            "created_at": row.get::<String, _>("created_at")
                        })
                    }).collect())
            } else {
                sqlx::query("SELECT id, COALESCE(source, '') AS source, COALESCE(original_content, '') AS original_content, COALESCE(draft_reply, '') AS draft_reply, COALESCE(status, '') AS status, COALESCE(sender_id, '') AS sender_id, COALESCE(customer_id, '') AS customer_id, CAST(created_at AS TEXT) AS created_at FROM omni_inbox_messages WHERE tenant_id = ? AND status != 'resolved' ORDER BY created_at DESC LIMIT 50")
                    .bind(tenant_id)
                    .fetch_all(pool)
                    .await.map(|rows| rows.into_iter().map(|row| {
                        let original_content = row.get::<String, _>("original_content");
                        serde_json::json!({
                            "id": row.get::<String, _>("id"),
                            "source": row.get::<String, _>("source"),
                            "content": original_content,
                            "original_content": original_content,
                            "draft_reply": row.get::<String, _>("draft_reply"),
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

#[cfg(test)]
pub(crate) async fn apply_daily_work_triage_action(
    db: &crate::db::DB,
    tenant_id: &str,
    item_id: &str,
    approved: bool,
    edited_payload: Option<&str>,
) -> Result<bool, sqlx::Error> {
    use crate::api::legacy_triage::edit_daily_work_actions;
    let status = if approved { "APPROVED" } else { "DISMISSED" };
    let rows_affected = match &db.store {
        crate::db::DbStore::Postgres => {
            let mut tx = db.pool.begin().await?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id).await?;
            let Some(actions) = sqlx::query_scalar::<_, Option<sqlx::types::Json<serde_json::Value>>>(
                "SELECT suggested_actions FROM daily_work_items WHERE id = $1 AND tenant_id = $2 FOR UPDATE",
            )
            .bind(item_id)
            .bind(tenant_id)
            .fetch_optional(&mut *tx)
            .await? else {
                return Ok(false);
            };
            let actions = edit_daily_work_actions(actions.map(|value| value.0), edited_payload)
                .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
            let result = sqlx::query(
                "UPDATE daily_work_items SET status = $1, suggested_actions = $2, updated_at = NOW() WHERE id = $3 AND tenant_id = $4",
            )
            .bind(status)
            .bind(actions.map(sqlx::types::Json))
            .bind(item_id)
            .bind(tenant_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            result.rows_affected()
        }
        crate::db::DbStore::Sqlite(pool) => {
            let mut tx = pool.begin().await?;
            let Some(actions) = sqlx::query_scalar::<_, Option<String>>(
                "SELECT suggested_actions FROM daily_work_items WHERE id = ? AND tenant_id = ?",
            )
            .bind(item_id)
            .bind(tenant_id)
            .fetch_optional(&mut *tx)
            .await?
            else {
                return Ok(false);
            };
            let actions = actions
                .map(|value| serde_json::from_str(&value))
                .transpose()
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
            let actions = edit_daily_work_actions(actions, edited_payload)
                .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
            let result = sqlx::query(
                "UPDATE daily_work_items SET status = ?, suggested_actions = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ? AND tenant_id = ?",
            )
            .bind(status)
            .bind(actions.map(|value| value.to_string()))
            .bind(item_id)
            .bind(tenant_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            result.rows_affected()
        }
    };
    if rows_affected > 0 {
        crate::api::work_triage::invalidate_daily_work_cache(tenant_id).await;
        if let Some(cache) = UI_TRIAGE_CACHE.get() {
            for mobile in [false, true] {
                cache
                    .invalidate(&format!("ui_triage:{tenant_id}:mobile:{mobile}"))
                    .await;
            }
        }
        invalidate_agent_feed_caches(tenant_id).await;
    }
    Ok(rows_affected > 0)
}

#[cfg(test)]
mod feed_backend_regression_tests {
    use super::*;
    use crate::api::legacy_triage::edit_daily_work_actions;

    #[tokio::test]
    async fn reservation_feed_invalidation_evicts_all_tenant_variants_only() {
        let tenant = format!("reservation-cache-{}", uuid::Uuid::new_v4());
        let other = format!("reservation-cache-other-{}", uuid::Uuid::new_v4());
        let ttl = std::time::Duration::from_secs(60);
        let feed =
            UI_AGENT_FEED_CACHE.get_or_init(|| ::server_utils::cache::HybridCache::new(None));
        let unified =
            UI_UNIFIED_FEED_CACHE.get_or_init(|| ::server_utils::cache::HybridCache::new(None));
        let agents = UI_UNIFIED_AGENT_FEED_CACHE
            .get_or_init(|| ::server_utils::cache::HybridCache::new(None));
        for owner in [&tenant, &other] {
            let tag = format!("ui_feed_tenant:{owner}");
            for mobile in [false, true] {
                let key = format!("ui_agent_feed:{owner}:mobile:{mobile}");
                feed.get_or_fetch_with_tags_swr(&key, vec![tag.clone()], ttl, || async {
                    Some(vec![serde_json::json!({"id": "prewarmed"})])
                })
                .await
                .unwrap();
                for fields in ["", "agent_feed", "agent_feed.id,pending_approvals"] {
                    for (cache, prefix) in [
                        (unified, "ui_dashboard_unified"),
                        (agents, "ui_unified_agent_feed"),
                    ] {
                        let key = format!("{prefix}:{owner}:mobile:{mobile}:fields:{fields}");
                        cache
                            .get_or_fetch_with_tags_swr(&key, vec![tag.clone()], ttl, || async {
                                Some(serde_json::json!({"agent_feed": [{"id": "prewarmed"}]}))
                            })
                            .await
                            .unwrap();
                    }
                }
            }
        }
        invalidate_agent_feed_caches(&tenant).await;
        for owner in [&tenant, &other] {
            let remains = owner == &other;
            for mobile in [false, true] {
                assert_eq!(
                    feed.get(&format!("ui_agent_feed:{owner}:mobile:{mobile}"))
                        .await
                        .is_some(),
                    remains
                );
                for fields in ["", "agent_feed", "agent_feed.id,pending_approvals"] {
                    for (cache, prefix) in [
                        (unified, "ui_dashboard_unified"),
                        (agents, "ui_unified_agent_feed"),
                    ] {
                        assert_eq!(
                            cache
                                .get(&format!("{prefix}:{owner}:mobile:{mobile}:fields:{fields}"))
                                .await
                                .is_some(),
                            remains
                        );
                    }
                }
            }
        }
        invalidate_agent_feed_caches(&other).await;
    }

    async fn daily_work_test_db() -> crate::db::DB {
        let sqlite = crate::db::create_sqlite_pool_for_test().await;
        sqlx::query("CREATE TABLE daily_work_items (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, suggested_actions TEXT, status TEXT NOT NULL CHECK (status IN ('PENDING', 'APPROVED', 'DISMISSED')), updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP)")
            .execute(&sqlite).await.unwrap();
        for (id, tenant) in [("work-a", "tenant-a"), ("work-b", "tenant-b")] {
            sqlx::query("INSERT INTO daily_work_items (id, tenant_id, suggested_actions, status) VALUES (?, ?, ?, 'PENDING')")
                .bind(id).bind(tenant)
                .bind(r#"{"draft_reply":"original draft","message_id":"message-1"}"#)
                .execute(&sqlite).await.unwrap();
        }
        crate::db::DB {
            pool: crate::db::create_dummy_pg_pool().await,
            store: crate::db::DbStore::Sqlite(sqlite),
        }
    }

    async fn saved_daily_work(db: &crate::db::DB, id: &str) -> (String, serde_json::Value) {
        let crate::db::DbStore::Sqlite(pool) = &db.store else {
            panic!("sqlite fixture required")
        };
        let (status, actions): (String, String) =
            sqlx::query_as("SELECT status, suggested_actions FROM daily_work_items WHERE id = ?")
                .bind(id)
                .fetch_one(pool)
                .await
                .unwrap();
        (status, serde_json::from_str(&actions).unwrap())
    }

    #[tokio::test]
    async fn daily_work_triage_approval_persists_edited_draft_without_claiming_delivery() {
        let db = daily_work_test_db().await;
        assert!(
            apply_daily_work_triage_action(
                &db,
                "tenant-a",
                "work-a",
                true,
                Some("Edited \"reply\"\nReady for review")
            )
            .await
            .unwrap()
        );
        let (status, actions) = saved_daily_work(&db, "work-a").await;
        assert_eq!(status, "APPROVED");
        assert_eq!(actions["draft_reply"], "Edited \"reply\"\nReady for review");
        assert_eq!(actions["message_id"], "message-1");
        assert_eq!(saved_daily_work(&db, "work-b").await.0, "PENDING");
    }

    #[tokio::test]
    async fn daily_work_triage_dismissal_preserves_unedited_draft() {
        let db = daily_work_test_db().await;
        assert!(
            apply_daily_work_triage_action(&db, "tenant-a", "work-a", false, None)
                .await
                .unwrap()
        );
        let (status, actions) = saved_daily_work(&db, "work-a").await;
        assert_eq!(status, "DISMISSED");
        assert_eq!(
            actions,
            serde_json::json!({"draft_reply": "original draft", "message_id": "message-1"})
        );
    }

    #[tokio::test]
    async fn daily_work_triage_cannot_update_another_tenant() {
        let db = daily_work_test_db().await;
        assert!(
            !apply_daily_work_triage_action(&db, "tenant-a", "work-b", true, Some("tampered"))
                .await
                .unwrap()
        );
        assert!(
            !apply_daily_work_triage_action(&db, "tenant-a", "missing", true, None)
                .await
                .unwrap()
        );
        assert_eq!(
            saved_daily_work(&db, "work-b").await,
            (
                "PENDING".to_string(),
                serde_json::json!({"draft_reply": "original draft", "message_id": "message-1"})
            )
        );
    }

    #[tokio::test]
    async fn daily_work_triage_edit_preserves_array_actions_and_metadata() {
        let db = daily_work_test_db().await;
        let crate::db::DbStore::Sqlite(pool) = &db.store else {
            panic!("sqlite fixture required")
        };
        let original = serde_json::json!([
            {"action_type": "Draft Reply", "message": "original", "metadata": {"channel": "email"}},
            {"action_type": "Other", "message": "leave unchanged"}
        ]);
        sqlx::query("UPDATE daily_work_items SET suggested_actions = ? WHERE id = 'work-a'")
            .bind(original.to_string())
            .execute(pool)
            .await
            .unwrap();
        assert!(
            apply_daily_work_triage_action(&db, "tenant-a", "work-a", true, Some("edited reply"))
                .await
                .unwrap()
        );
        assert_eq!(
            saved_daily_work(&db, "work-a").await,
            (
                "APPROVED".to_string(),
                serde_json::json!([
                    {"action_type": "Draft Reply", "message": "edited reply", "metadata": {"channel": "email"}},
                    {"action_type": "Other", "message": "leave unchanged"}
                ])
            )
        );
    }

    #[tokio::test]
    async fn daily_work_triage_edit_initializes_sql_and_json_null_drafts() {
        let db = daily_work_test_db().await;
        let crate::db::DbStore::Sqlite(pool) = &db.store else {
            panic!("sqlite fixture required")
        };
        for original in [None, Some("null")] {
            sqlx::query("UPDATE daily_work_items SET suggested_actions = ?, status = 'PENDING' WHERE id = 'work-a'")
                .bind(original).execute(pool).await.unwrap();
            assert!(
                apply_daily_work_triage_action(&db, "tenant-a", "work-a", true, Some("new reply"))
                    .await
                    .unwrap()
            );
            assert_eq!(
                saved_daily_work(&db, "work-a").await,
                (
                    "APPROVED".to_string(),
                    serde_json::json!({"draft_reply": "new reply"})
                )
            );
        }
    }

    #[tokio::test]
    async fn daily_work_triage_rejects_edits_without_a_supported_action_shape() {
        let db = daily_work_test_db().await;
        let crate::db::DbStore::Sqlite(pool) = &db.store else {
            panic!("sqlite fixture required")
        };
        for original in [
            serde_json::json!([]),
            serde_json::json!([false]),
            serde_json::json!(123),
        ] {
            sqlx::query("UPDATE daily_work_items SET suggested_actions = ? WHERE id = 'work-a'")
                .bind(original.to_string())
                .execute(pool)
                .await
                .unwrap();
            assert!(
                apply_daily_work_triage_action(
                    &db,
                    "tenant-a",
                    "work-a",
                    true,
                    Some("unsaved reply")
                )
                .await
                .is_err()
            );
            assert_eq!(
                saved_daily_work(&db, "work-a").await,
                ("PENDING".to_string(), original)
            );
        }
    }

    #[test]
    fn daily_work_draft_edit_preserves_an_array_draft_reply_field() {
        let original = serde_json::json!([{"draft_reply": "original", "message": "original", "action_type": "Draft Reply"}]);
        assert_eq!(
            edit_daily_work_actions(Some(original), Some("edited")).unwrap(),
            Some(serde_json::json!([
                {"draft_reply": "edited", "message": "edited", "action_type": "Draft Reply"}
            ]))
        );
    }

    #[tokio::test]
    async fn daily_work_triage_database_failure_does_not_report_success() {
        let db = daily_work_test_db().await;
        let crate::db::DbStore::Sqlite(pool) = &db.store else {
            panic!("sqlite fixture required")
        };
        sqlx::query("CREATE TRIGGER reject_decision BEFORE UPDATE ON daily_work_items BEGIN SELECT RAISE(ABORT, 'decision storage unavailable'); END")
            .execute(pool).await.unwrap();
        assert!(
            apply_daily_work_triage_action(&db, "tenant-a", "work-a", true, Some("unsaved"))
                .await
                .is_err()
        );
        assert_eq!(saved_daily_work(&db, "work-a").await.0, "PENDING");
    }
}
