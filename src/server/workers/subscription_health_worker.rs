use crate::db::DB;
use crate::orchestration::departments::orchestrator::DepartmentOrchestrator;
use crate::orchestration::departments::types::DepartmentEvent;
use crate::runtime_shutdown::CancellationToken;
use sqlx::Row;
use std::sync::Arc;
use uuid::Uuid;

pub struct SubscriptionHealthWorker {
    pub db: Arc<DB>,
    pub orchestrator: Option<Arc<DepartmentOrchestrator>>,
}

impl SubscriptionHealthWorker {
    pub fn new(db: Arc<DB>) -> Self {
        Self {
            db,
            orchestrator: None,
        }
    }

    pub fn with_orchestrator(mut self, orchestrator: Arc<DepartmentOrchestrator>) -> Self {
        self.orchestrator = Some(orchestrator);
        self
    }

    pub fn start(&self) {
        self.start_with_shutdown(CancellationToken::new());
    }

    pub fn start_with_shutdown(&self, shutdown: CancellationToken) {
        let db = self.db.clone();
        let orchestrator = self.orchestrator.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        tracing::info!("subscription health worker stopping on shutdown");
                        break;
                    }
                    _ = interval.tick() => {
                        let _ = process_next_job(&db, orchestrator.as_ref()).await;
                    }
                }
            }
        });
    }
}

/// Claim one `subscription_health` job in a short transaction, commit the claim
/// BEFORE evaluating/dispatching orchestration events, and mark completion in a
/// separate short transaction so DB connections are never held across multi-agent
/// orchestration.
pub(crate) async fn process_next_job(
    db: &Arc<DB>,
    orchestrator: Option<&Arc<DepartmentOrchestrator>>,
) -> bool {
    let job = match &db.store {
        crate::db::DbStore::Postgres => {
            let mut tx = match db.pool.begin().await {
                Ok(tx) => tx,
                Err(error) => {
                    tracing::warn!(
                        "subscription health worker failed to begin claim transaction: {}",
                        error
                    );
                    return false;
                }
            };
            if let Err(error) = crate::tenant_context()
                .bind_pg_system_transaction(&mut tx)
                .await
            {
                tracing::warn!(
                    "subscription health worker failed to bind system transaction: {}",
                    error
                );
                return false;
            }
            let claimed = sqlx::query(
                "UPDATE ohc_job_queue SET status = 'PROCESSING', updated_at = NOW() \
                 WHERE id = ( \
                     SELECT id FROM ohc_job_queue \
                     WHERE status = 'PENDING' \
                       AND (next_retry_at IS NULL OR next_retry_at <= NOW()) \
                       AND job_type = 'subscription_health' \
                     FOR UPDATE SKIP LOCKED LIMIT 1 \
                 ) RETURNING id, tenant_id, payload",
            )
            .fetch_optional(&mut *tx)
            .await
            .unwrap_or(None)
            .map(|r| {
                let job_id: String = r.get("id");
                let tenant_id: String = r.get("tenant_id");
                let payload: serde_json::Value = r.get("payload");
                (job_id, tenant_id, payload)
            });
            if let Err(error) = tx.commit().await {
                tracing::warn!(
                    "subscription health worker failed to commit claim transaction: {}",
                    error
                );
                return false;
            }
            claimed
        }
        crate::db::DbStore::Sqlite(sqlite_pool) => sqlx::query(
            "UPDATE ohc_job_queue SET status = 'PROCESSING', updated_at = CURRENT_TIMESTAMP \
             WHERE id = ( \
                 SELECT id FROM ohc_job_queue \
                 WHERE status = 'PENDING' \
                   AND (next_retry_at IS NULL OR next_retry_at <= CURRENT_TIMESTAMP) \
                   AND job_type = 'subscription_health' \
                 LIMIT 1 \
             ) RETURNING id, tenant_id, payload",
        )
        .fetch_optional(sqlite_pool)
        .await
        .unwrap_or(None)
        .map(|r| {
            let job_id: String = r.get("id");
            let tenant_id: String = r.get("tenant_id");
            let payload_str: String = r.get("payload");
            let payload = serde_json::from_str(&payload_str).unwrap_or_default();
            (job_id, tenant_id, payload)
        }),
    };

    let Some((job_id, tenant_id, payload)) = job else {
        return false;
    };

    let event_to_dispatch = if let (Some(subscriber_id), Some(customer_id)) = (
        payload.get("subscriber_id").and_then(|v| v.as_str()),
        payload.get("customer_id").and_then(|v| v.as_str()),
    ) {
        evaluate_subscriber_health(db, &tenant_id, subscriber_id, customer_id).await
    } else {
        None
    };

    // Dispatch outside any open DB transaction so pool connections stay free.
    if let (Some(event), Some(orch)) = (event_to_dispatch, orchestrator) {
        let _ = orch.dispatch_event(event).await;
    }

    match &db.store {
        crate::db::DbStore::Postgres => {
            if let Ok(mut tx) = db.pool.begin().await
                && ::server_common::auth_utils::set_org_context(&mut *tx, &tenant_id)
                    .await
                    .is_ok()
            {
                let _ = sqlx::query(
                    "UPDATE ohc_job_queue SET status = 'COMPLETED', updated_at = NOW() \
                     WHERE id = $1 AND tenant_id = $2",
                )
                .bind(&job_id)
                .bind(&tenant_id)
                .execute(&mut *tx)
                .await;
                if tx.commit().await.is_err() {
                    tracing::warn!(
                        "subscription health worker failed to commit completion transaction"
                    );
                }
            }
        }
        crate::db::DbStore::Sqlite(sqlite_pool) => {
            let _ = sqlx::query(
                "UPDATE ohc_job_queue SET status = 'COMPLETED', updated_at = CURRENT_TIMESTAMP \
                 WHERE id = ? AND tenant_id = ?",
            )
            .bind(&job_id)
            .bind(&tenant_id)
            .execute(sqlite_pool)
            .await;
        }
    }

    true
}

async fn evaluate_subscriber_health(
    db: &Arc<DB>,
    tenant_id: &str,
    subscriber_id: &str,
    customer_id: &str,
) -> Option<DepartmentEvent> {
    match &db.store {
        crate::db::DbStore::Postgres => {
            let mut tx = db.pool.begin().await.ok()?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id)
                .await
                .ok()?;

            let (status, last_engagement_at) = sqlx::query(
                "SELECT status, last_engagement_at FROM subscribers WHERE id = $1 AND tenant_id = $2",
            )
            .bind(subscriber_id)
            .bind(tenant_id)
            .fetch_optional(&mut *tx)
            .await
            .unwrap_or(None)
            .map(|r| {
                let s: String = r.get("status");
                let dt: Option<chrono::DateTime<chrono::Utc>> =
                    r.try_get("last_engagement_at").unwrap_or(None);
                (s, dt.map(|d| d.timestamp()))
            })
            .unwrap_or(("ACTIVE".to_string(), None));

            let health_score = compute_health_score(&status, last_engagement_at);
            let _ = sqlx::query(
                "UPDATE subscribers SET health_score = $1 WHERE id = $2 AND tenant_id = $3",
            )
            .bind(health_score)
            .bind(subscriber_id)
            .bind(tenant_id)
            .execute(&mut *tx)
            .await;

            let should_dispatch = if health_score < 50 {
                let already_drafted = sqlx::query(
                    "SELECT id FROM agent_action_requests \
                     WHERE tenant_id = $1 AND payload->>'subscriber_id' = $2 \
                       AND created_at > NOW() - INTERVAL '30 days'",
                )
                .bind(tenant_id)
                .bind(subscriber_id)
                .fetch_optional(&mut *tx)
                .await
                .unwrap_or(None)
                .is_some();
                !already_drafted
            } else {
                false
            };

            let _ = tx.commit().await;

            should_dispatch
                .then(|| build_at_risk_event(tenant_id, subscriber_id, customer_id, health_score))
        }
        crate::db::DbStore::Sqlite(sqlite_pool) => {
            let (status, last_engagement_at) = sqlx::query(
                "SELECT status, last_engagement_at FROM subscribers WHERE id = ? AND tenant_id = ?",
            )
            .bind(subscriber_id)
            .bind(tenant_id)
            .fetch_optional(sqlite_pool)
            .await
            .unwrap_or(None)
            .map(|r| {
                let s: String = r.get("status");
                let dt: Option<String> = r.try_get("last_engagement_at").unwrap_or(None);
                let ts = dt
                    .and_then(|d| chrono::DateTime::parse_from_rfc3339(d.as_str()).ok())
                    .map(|d| d.timestamp());
                (s, ts)
            })
            .unwrap_or(("ACTIVE".to_string(), None));

            let health_score = compute_health_score(&status, last_engagement_at);
            let _ = sqlx::query(
                "UPDATE subscribers SET health_score = ? WHERE id = ? AND tenant_id = ?",
            )
            .bind(health_score)
            .bind(subscriber_id)
            .bind(tenant_id)
            .execute(sqlite_pool)
            .await;

            let should_dispatch = if health_score < 50 {
                let already_drafted = sqlx::query(
                    "SELECT id FROM agent_action_requests \
                     WHERE tenant_id = ? AND json_extract(payload, '$.subscriber_id') = ? \
                       AND created_at > datetime('now', '-30 days')",
                )
                .bind(tenant_id)
                .bind(subscriber_id)
                .fetch_optional(sqlite_pool)
                .await
                .unwrap_or(None)
                .is_some();
                !already_drafted
            } else {
                false
            };

            should_dispatch
                .then(|| build_at_risk_event(tenant_id, subscriber_id, customer_id, health_score))
        }
    }
}

fn compute_health_score(status: &str, last_engagement_at: Option<i64>) -> i32 {
    let mut health_score = 100;
    if status == "PAST_DUE" {
        health_score -= 50;
    }
    if let Some(ts) = last_engagement_at {
        let now = chrono::Utc::now().timestamp();
        if now - ts > 1814400 {
            health_score -= 20;
        }
    } else {
        health_score -= 10;
    }
    health_score
}

fn build_at_risk_event(
    tenant_id: &str,
    subscriber_id: &str,
    customer_id: &str,
    health_score: i32,
) -> DepartmentEvent {
    DepartmentEvent {
        id: Uuid::new_v4().to_string(),
        tenant_id: tenant_id.to_string(),
        event_type: "tenant.subscription.at_risk".to_string(),
        payload: serde_json::json!({
            "subscriber_id": subscriber_id,
            "customer_id": customer_id,
            "health_score": health_score
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn process_next_job_updates_health_score_and_completes_job() {
        let sqlite = crate::db::create_sqlite_pool_for_test().await;
        sqlx::query(
            "CREATE TABLE ohc_job_queue (
                id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                job_type TEXT NOT NULL,
                status TEXT NOT NULL,
                payload TEXT NOT NULL,
                next_retry_at TEXT,
                updated_at TEXT
            )",
        )
        .execute(&sqlite)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE subscribers (
                id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                status TEXT NOT NULL,
                last_engagement_at TEXT,
                health_score INTEGER DEFAULT 100
            )",
        )
        .execute(&sqlite)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE agent_action_requests (
                id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                payload TEXT NOT NULL,
                created_at TEXT DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(&sqlite)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO subscribers (id, tenant_id, status, last_engagement_at)
             VALUES ('sub-1', 'tenant-a', 'PAST_DUE', NULL)",
        )
        .execute(&sqlite)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ohc_job_queue (id, tenant_id, job_type, status, payload)
             VALUES ('job-1', 'tenant-a', 'subscription_health', 'PENDING', '{\"subscriber_id\":\"sub-1\",\"customer_id\":\"cust-1\"}')",
        )
        .execute(&sqlite)
        .await
        .unwrap();

        let db = Arc::new(DB {
            pool: crate::db::create_dummy_pg_pool().await,
            store: crate::db::DbStore::Sqlite(sqlite.clone()),
        });

        assert!(process_next_job(&db, None).await);
        assert!(!process_next_job(&db, None).await);

        let status: String =
            sqlx::query_scalar("SELECT status FROM ohc_job_queue WHERE id = 'job-1'")
                .fetch_one(&sqlite)
                .await
                .unwrap();
        assert_eq!(status, "COMPLETED");

        let score: i32 =
            sqlx::query_scalar("SELECT health_score FROM subscribers WHERE id = 'sub-1'")
                .fetch_one(&sqlite)
                .await
                .unwrap();
        assert_eq!(score, 40);
    }
}
