use super::*;
use std::sync::Arc;
use crate::orchestration::queue::omnisolo_job_queue::OmniSoloJobQueue;
use serde_json::json;

#[tokio::test]
async fn test_malformed_payload_fails_job() {
    if std::env::var("OMNISOLO_DATABASE_URL").is_err() {
        return; // Skip if no real test DB is available
    }

    let database_url = std::env::var("OMNISOLO_DATABASE_URL").unwrap();

    let pool = crate::db::secure_pg_pool_options().connect(&database_url).await.unwrap();
    let queue = OmniSoloJobQueue::new(Arc::new(pool.clone()));

    let worker = AgentActionWorker::new(pool.clone(), String::new());

    let payload = json!({
        "action_id": "test_action_123",
        "is_incident": false
        // missing feature_type
    }).to_string();

    let job_id = queue.enqueue("system", "agent_feed_action", &serde_json::from_str(&payload).unwrap()).await.unwrap();

    let job = queue.dequeue(vec!["agent_feed_action"]).await.unwrap().unwrap();
    assert_eq!(job.id, job_id);

    worker.process_job(job, &queue).await;

    // After processing, the job should be marked as FAILED or PENDING with increased retry count, due to malformed payload.
    // For malformed payload, it calls queue.fail. The status in the db will be PENDING with retry_count > 0, or FAILED if max retries reached.
    let row = sqlx::query("SELECT status, retry_count FROM ohc_job_queue WHERE id = $1")
        .bind(&job_id)
        .fetch_one(&pool)
        .await
        .unwrap();

    use sqlx::Row;
    let retry_count: i32 = row.get("retry_count");
    assert!(retry_count > 0);
}
