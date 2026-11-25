//! Real-flow integration tests for Omni Assistant & OHC Task Center: real Axum router,
//! real SQLite migrations, real UsageLedger micro-USD enforcement, and real
//! local TCP/HTTP loopback provider endpoints. Zero mocks, fakes, or stubs.
use super::executor::{
    RedactedSecret, SseEventKind, TaskCenterEventStream, TaskCenterExecutor, TaskCenterLlmConfig,
    format_sse_frame,
};
use super::store::{
    PayerMode, ProvenanceState, SpaceRecord, SpaceSourceHealth, TaskArtifactRecord,
    TaskCenterSseEvent, TaskCenterTask, TaskCenterTaskStatus,
};
use super::{TaskCenterState, router_with_state};
use crate::db::{DB, DbStore};
use crate::persistence::AppDatabase;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::{Json, Router, routing::post};
use chrono::Utc;
use futures_util::StreamExt;
use sea_orm::SqlxSqliteConnector;
use serde_json::{Value, json};
use server_common::Claims;
use server_harness::middleware::usage_ledger::RateCard;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{Notify, broadcast};
use tower::ServiceExt;
use uuid::Uuid;

struct Client<'a>(&'a TaskCenterState, Claims);

impl<'a> Client<'a> {
    fn new(
        st: &'a TaskCenterState,
        tenant: &str,
        user: &str,
        roles: &[&str],
        exp_off: i64,
    ) -> Self {
        let now = Utc::now().timestamp();
        let claims = Claims {
            sub: user.into(),
            exp: now + exp_off,
            iat: now - 5,
            organization_id: Some(tenant.into()),
            username: user.into(),
            email: format!("{user}@{tenant}.test"),
            roles: roles.iter().map(|r| (*r).into()).collect(),
            session_id: Some(format!("sess-{user}")),
            jti: Uuid::new_v4().to_string(),
        };
        Self(st, claims)
    }

    async fn send(
        &self,
        m: Method,
        uri: &str,
        idem: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value, HeaderMap) {
        let app: Router = router_with_state(self.0.clone()).layer(axum::Extension(self.1.clone()));
        let mut b = Request::builder().method(m).uri(uri);
        if let Some(k) = idem {
            b = b.header("idempotency-key", k);
        }
        let req = match body {
            Some(j) => b
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&j).unwrap()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let resp = app.oneshot(req).await.unwrap();
        let (code, hdrs) = (resp.status(), resp.headers().clone());
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        (
            code,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            hdrs,
        )
    }

    async fn get(&self, uri: &str) -> (StatusCode, Value, HeaderMap) {
        self.send(Method::GET, uri, None, None).await
    }
    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        let (c, v, _) = self.send(Method::POST, uri, None, Some(body)).await;
        (c, v)
    }
    async fn post_empty(&self, uri: &str) -> (StatusCode, Value) {
        let (c, v, _) = self.send(Method::POST, uri, None, None).await;
        (c, v)
    }
    async fn post_idem(&self, uri: &str, key: &str, body: Value) -> (StatusCode, Value) {
        let (c, v, _) = self.send(Method::POST, uri, Some(key), Some(body)).await;
        (c, v)
    }
    async fn patch(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        let (c, v, _) = self.send(Method::PATCH, uri, None, Some(body)).await;
        (c, v)
    }
    async fn put(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        let (c, v, _) = self.send(Method::PUT, uri, None, Some(body)).await;
        (c, v)
    }
}

async fn open_sqlite_db(sqlite: sqlx::SqlitePool) -> Arc<DB> {
    let app_db =
        AppDatabase::from_connection(SqlxSqliteConnector::from_sqlx_sqlite_pool(sqlite.clone()));
    crate::persistence::migration::migrate(&app_db)
        .await
        .expect("sqlite migration must succeed");
    Arc::new(DB {
        pool: crate::db::create_dummy_pg_pool().await,
        store: DbStore::Sqlite(sqlite),
    })
}

async fn new_memory_db() -> Arc<DB> {
    open_sqlite_db(crate::db::create_sqlite_pool_for_test().await).await
}

struct LoopbackProvider {
    endpoint: String,
    calls: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release: Arc<Notify>,
    handle: tokio::task::JoinHandle<()>,
}
impl Drop for LoopbackProvider {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn spawn_loopback_provider(fail_http: bool, hold: bool) -> LoopbackProvider {
    let (calls, started, release) = (
        Arc::new(AtomicUsize::new(0)),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
    );
    let (c, s, r) = (calls.clone(), started.clone(), release.clone());
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(req): Json<Value>| {
            let (calls, started, release) = (c.clone(), s.clone(), r.clone());
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                started.notify_waiters();
                if hold {
                    release.notified().await;
                }
                if fail_http {
                    return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "upstream error" })));
                }
                let prompt = req["messages"].as_array().and_then(|a| a.last()).and_then(|m| m["content"].as_str()).unwrap_or("");
                (
                    StatusCode::OK,
                    Json(json!({
                        "id": format!("chatcmpl-loopback-{n}-{}", Uuid::new_v4()),
                        "choices": [{ "index": 0, "finish_reason": "stop", "message": { "role": "assistant", "content": format!("Verified reply #{n}: {prompt}") } }],
                        "usage": { "prompt_tokens": 120, "completion_tokens": 80, "prompt_tokens_details": { "cached_tokens": 20 } }
                    })),
                )
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    LoopbackProvider {
        endpoint: format!("http://{addr}/v1/chat/completions"),
        calls,
        started,
        release,
        handle,
    }
}

fn managed_config(endpoint: String) -> TaskCenterLlmConfig {
    TaskCenterLlmConfig {
        provider: "openai-compatible".into(),
        model: "gpt-loopback-1".into(),
        endpoint,
        api_key: Some(RedactedSecret::new("sk-loopback-test-key-0001".into())),
        payer_mode: PayerMode::ManagedApi,
        rate_card: Some(RateCard {
            revision: "loopback-rate-v1".into(),
            input_micros_per_million: 1_000_000,
            output_micros_per_million: 2_000_000,
            cached_input_micros_per_million: 500_000,
        }),
        max_output_tokens: 256,
        request_ceiling_micros: 100_000,
        timeout_secs: 15,
    }
}

async fn wait_status(
    st: &TaskCenterState,
    tenant: &str,
    tid: &str,
    want: TaskCenterTaskStatus,
) -> TaskCenterTask {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(m) = st.store.get_task_meta(tenant, tid).await.unwrap()
            && m.task.status == want
        {
            return m.task;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timeout {tid} -> {want:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn funded_state(
    tenant: &str,
    limit: i64,
    endpoint: String,
) -> (TaskCenterState, Arc<TaskCenterExecutor>) {
    let db = new_memory_db().await;
    let exec =
        Arc::new(TaskCenterExecutor::with_config(db.clone(), managed_config(endpoint)).unwrap());
    exec.usage_ledger().set_limit(tenant, limit).await.unwrap();
    (TaskCenterState::new(db, exec.clone()), exec)
}

#[tokio::test]
async fn unconfigured_runtime_fails_closed_with_zero_fake_completions() {
    let db = new_memory_db().await;
    let st = TaskCenterState::new(db.clone(), Arc::new(TaskCenterExecutor::unconfigured(db)));
    let owner = Client::new(&st, "tenant-unconf", "owner-1", &["owner"], 3600);

    let (code, summary, hdrs) = owner.get("/summary").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(hdrs["cache-control"], "private, no-store");
    assert_eq!(summary["runtime"]["llm_configured"], false);
    assert_eq!(summary["runtime"]["payer_mode"], "unconfigured");
    assert!(summary["runtime"]["reason"].as_str().is_some());
    assert_eq!(summary["budget"]["configured"], false);
    assert_eq!(
        summary["budget"]["customer_direct_inference_is_not_rebilled"],
        true
    );

    let (code, body) = owner
        .post_idem(
            "/tasks",
            &Uuid::new_v4().to_string(),
            json!({ "prompt": "Report" }),
        )
        .await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        (body["provenance"].as_str(), body["payer_mode"].as_str()),
        (Some("unconfigured"), Some("unconfigured"))
    );
    assert!(
        owner.get("/tasks").await.1["tasks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn space_crud_manifest_validation_and_cross_tenant_isolation() {
    let db = new_memory_db().await;
    let st = TaskCenterState::new(db.clone(), Arc::new(TaskCenterExecutor::unconfigured(db)));
    let a = Client::new(&st, "tenant-a", "owner-a", &["owner"], 3600);
    let b = Client::new(&st, "tenant-b", "owner-b", &["owner"], 3600);

    assert_eq!(
        a.post(
            "/spaces",
            json!({ "name": "Bad", "reads": [], "shows": ["x"], "cannot": ["y"] })
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        a.post("/spaces", json!({ "name": "<script>x</script>" }))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );

    let (code, created) = a.post("/spaces", json!({
        "name": "Cash Recovery HQ", "description": "Track overdue invoices",
        "kind": "cash_recovery", "prompt_template": "Review overdue accounts", "department": "finance",
        "reads": ["Usage ledger", "Invoices"], "shows": ["Recovery queue"],
        "cannot": ["Issue refunds without approval", "Send email without approval"]
    })).await;
    assert_eq!(code, StatusCode::CREATED);
    let sp: SpaceRecord = serde_json::from_value(created).unwrap();
    assert!(sp.id.starts_with("spc-") && sp.source_health == SpaceSourceHealth::Unconfigured);

    let sp_uri = format!("/spaces/{}", sp.id);
    let (code, ren) = a
        .patch(
            &sp_uri,
            json!({ "action": "rename", "name": "Collections Desk" }),
        )
        .await;
    assert_eq!(
        (code, ren["name"].as_str()),
        (StatusCode::OK, Some("Collections Desk"))
    );
    let (code, arch) = a.patch(&sp_uri, json!({ "action": "archive" })).await;
    assert_eq!(
        (code, arch["archived"].as_bool()),
        (StatusCode::OK, Some(true))
    );

    assert!(
        b.get("/spaces").await.1["spaces"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        b.patch(&sp_uri, json!({ "action": "rename", "name": "Hijack" }))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn standing_authority_approval_cas_sha256_idempotency_and_usage_ledger_flow() {
    let prov = spawn_loopback_provider(false, false).await;
    let (st, exec) = funded_state("tenant-alpha", 50_000, prov.endpoint.clone()).await;
    let owner = Client::new(&st, "tenant-alpha", "owner-1", &["owner"], 3600);
    let staff = Client::new(&st, "tenant-alpha", "staff-1", &["staff"], 3600);
    let other = Client::new(&st, "tenant-beta", "owner-b", &["owner"], 3600);

    assert_eq!(
        owner.post("/tasks", json!({ "prompt": "No key" })).await.0,
        StatusCode::BAD_REQUEST
    );

    let key1 = Uuid::new_v4().to_string();
    let req1 = json!({ "prompt": "Prepare Q3 margin summary", "department": "operations", "estimated_cost_micros": 5_000 });
    let (code, tj) = owner.post_idem("/tasks", &key1, req1.clone()).await;
    assert_eq!(code, StatusCode::CREATED);
    let task: TaskCenterTask = serde_json::from_value(tj).unwrap();
    assert_eq!(
        (task.status, task.provenance),
        (
            TaskCenterTaskStatus::AwaitingApproval,
            ProvenanceState::AwaitingApproval
        )
    );
    let appr = task.pending_approval.clone().expect("pending approval");
    assert_eq!(prov.calls.load(Ordering::SeqCst), 0);

    let (code, replay) = owner.post_idem("/tasks", &key1, req1).await;
    assert_eq!(
        (code, replay["id"].as_str()),
        (StatusCode::OK, Some(task.id.as_str()))
    );
    assert_eq!(owner.post_idem("/tasks", &key1, json!({ "prompt": "Mutated", "department": "operations", "estimated_cost_micros": 5_000 })).await.0, StatusCode::CONFLICT);

    let appr_uri = format!("/tasks/{}/approve", task.id);
    let ok_body = json!({ "approved": true, "payload_sha256": appr.payload_sha256 });
    assert_eq!(
        staff.post(&appr_uri, ok_body.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        staff
            .put(
                "/policies/operations",
                json!({ "auto_approve_max_micros": 25_000 })
            )
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        owner
            .post(
                &appr_uri,
                json!({ "approved": true, "payload_sha256": "0".repeat(64) })
            )
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        other.get(&format!("/tasks/{}", task.id)).await.0,
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        owner.post(&appr_uri, ok_body.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(owner.post(&appr_uri, ok_body).await.0, StatusCode::CONFLICT);

    let done = wait_status(
        &st,
        "tenant-alpha",
        &task.id,
        TaskCenterTaskStatus::Completed,
    )
    .await;
    assert_eq!(
        (
            done.provenance,
            done.actual_cost_micros,
            done.artifacts.len()
        ),
        (ProvenanceState::Verified, Some(270), 1)
    );
    assert!(
        done.output_summary
            .as_deref()
            .unwrap()
            .contains("Verified reply #1")
    );
    let led = exec.usage_ledger().summary("tenant-alpha").await.unwrap();
    assert_eq!((led.reserved_micros, led.spent_micros), (0, 270));

    assert_eq!(owner.put("/policies/operations", json!({ "auto_approve_max_micros": 20_000, "allow_external_send": false, "allow_financial_commit": false })).await.0, StatusCode::OK);
    let (code, auto_j) = owner.post_idem("/tasks", &Uuid::new_v4().to_string(), json!({ "prompt": "Daily metrics", "department": "operations", "estimated_cost_micros": 3_000 })).await;
    assert_eq!(code, StatusCode::ACCEPTED);
    assert_eq!(
        wait_status(
            &st,
            "tenant-alpha",
            auto_j["id"].as_str().unwrap(),
            TaskCenterTaskStatus::Completed
        )
        .await
        .provenance,
        ProvenanceState::Verified
    );

    let (code, email_j) = owner.post_idem("/tasks", &Uuid::new_v4().to_string(), json!({ "prompt": "Email Acme", "department": "operations", "requested_action": "send_email", "estimated_cost_micros": 2_000 })).await;
    assert_eq!(
        (code, email_j["status"].as_str()),
        (StatusCode::CREATED, Some("awaiting_approval"))
    );

    let (_, sp_j) = owner
        .post(
            "/spaces",
            json!({ "name": "Ops Board", "kind": "operations" }),
        )
        .await;
    let sp_id = sp_j["id"].as_str().unwrap().to_string();
    let art_id = done.artifacts[0].id.clone();
    let (code, pin_j) = owner
        .post(
            &format!("/tasks/{}/artifacts/{}/pin", task.id, art_id),
            json!({ "space_id": sp_id, "pinned": true }),
        )
        .await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        serde_json::from_value::<TaskArtifactRecord>(pin_j)
            .unwrap()
            .pinned
    );
    assert!(
        st.store
            .get_space("tenant-alpha", &sp_id)
            .await
            .unwrap()
            .unwrap()
            .pinned_artifact_ids
            .contains(&art_id)
    );

    let sha = email_j["pending_approval"]["payload_sha256"]
        .as_str()
        .unwrap();
    let (code, rej) = owner
        .post(
            &format!("/tasks/{}/approve", email_j["id"].as_str().unwrap()),
            json!({ "approved": false, "payload_sha256": sha, "reason": "Manual" }),
        )
        .await;
    assert_eq!(
        (code, rej["status"].as_str()),
        (StatusCode::OK, Some("cancelled"))
    );
    assert_eq!(
        exec.usage_ledger()
            .summary("tenant-alpha")
            .await
            .unwrap()
            .reserved_micros,
        0
    );
}

#[tokio::test]
async fn budget_exhaustion_returns_409_with_zero_provider_calls() {
    let prov = spawn_loopback_provider(false, false).await;
    let (st, exec) = funded_state("tenant-tight", 1_000, prov.endpoint.clone()).await;
    let owner = Client::new(&st, "tenant-tight", "owner-1", &["owner"], 3600);

    let (code, body) = owner
        .post_idem(
            "/tasks",
            &Uuid::new_v4().to_string(),
            json!({ "prompt": "Expensive", "estimated_cost_micros": 5_000 }),
        )
        .await;
    assert_eq!(
        (code, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("budget_exhausted"))
    );
    assert_eq!(prov.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        exec.usage_ledger()
            .summary("tenant-tight")
            .await
            .unwrap()
            .reserved_micros,
        0
    );
}

#[tokio::test]
async fn in_flight_cancellation_and_provider_error_preserve_outcome_unknown_without_retry() {
    let held = spawn_loopback_provider(false, true).await;
    let (st, exec) = funded_state("tenant-cancel", 50_000, held.endpoint.clone()).await;
    let owner = Client::new(&st, "tenant-cancel", "owner-1", &["owner"], 3600);
    owner
        .put(
            "/policies/operations",
            json!({ "auto_approve_max_micros": 25_000 }),
        )
        .await;

    let (code, c) = owner
        .post_idem(
            "/tasks",
            &Uuid::new_v4().to_string(),
            json!({ "prompt": "Long", "estimated_cost_micros": 4_000 }),
        )
        .await;
    assert_eq!(code, StatusCode::ACCEPTED);
    let tid = c["id"].as_str().unwrap().to_string();
    tokio::time::timeout(Duration::from_secs(3), held.started.notified())
        .await
        .expect("provider hit");

    let (code, can) = owner.post_empty(&format!("/tasks/{tid}/cancel")).await;
    assert_eq!(
        (code, can["status"].as_str(), can["provenance"].as_str()),
        (
            StatusCode::OK,
            Some("outcome_unknown"),
            Some("outcome_unknown")
        )
    );
    held.release.notify_waiters();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(held.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        exec.usage_ledger()
            .summary("tenant-cancel")
            .await
            .unwrap()
            .reserved_micros,
        4_000
    );

    let (code, body) = owner
        .post(
            &format!("/tasks/{tid}/messages"),
            json!({ "prompt": "Retry" }),
        )
        .await;
    assert_eq!(
        (code, body["code"].as_str()),
        (StatusCode::CONFLICT, Some("outcome_unknown_no_retry"))
    );
    assert_eq!(held.calls.load(Ordering::SeqCst), 1);

    let fail = spawn_loopback_provider(true, false).await;
    let (st2, _) = funded_state("tenant-err", 50_000, fail.endpoint.clone()).await;
    let owner2 = Client::new(&st2, "tenant-err", "owner-2", &["owner"], 3600);
    owner2
        .put(
            "/policies/operations",
            json!({ "auto_approve_max_micros": 25_000 }),
        )
        .await;
    let (_, c2) = owner2
        .post_idem(
            "/tasks",
            &Uuid::new_v4().to_string(),
            json!({ "prompt": "Fail", "estimated_cost_micros": 3_000 }),
        )
        .await;
    assert_eq!(
        wait_status(
            &st2,
            "tenant-err",
            c2["id"].as_str().unwrap(),
            TaskCenterTaskStatus::OutcomeUnknown
        )
        .await
        .provenance,
        ProvenanceState::OutcomeUnknown
    );
    assert_eq!(fail.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn persistence_survives_sqlite_connection_restart() {
    let path = std::env::temp_dir().join(format!("ohc-task-center-{}.sqlite", Uuid::new_v4()));
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let pool1 = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(opts.clone())
        .await
        .unwrap();
    let db1 = open_sqlite_db(pool1.clone()).await;
    let prov = spawn_loopback_provider(false, false).await;
    let exec1 = Arc::new(
        TaskCenterExecutor::with_config(db1.clone(), managed_config(prov.endpoint.clone()))
            .unwrap(),
    );
    exec1
        .usage_ledger()
        .set_limit("tenant-r", 50_000)
        .await
        .unwrap();
    let st1 = TaskCenterState::new(db1, exec1);
    let owner1 = Client::new(&st1, "tenant-r", "owner-r", &["owner"], 3600);

    let (_, sp) = owner1
        .post(
            "/spaces",
            json!({ "name": "Durable Space", "kind": "research" }),
        )
        .await;
    owner1
        .put(
            "/policies/operations",
            json!({ "auto_approve_max_micros": 25_000 }),
        )
        .await;
    let (_, tk) = owner1
        .post_idem(
            "/tasks",
            &Uuid::new_v4().to_string(),
            json!({ "prompt": "Persist", "space_id": sp["id"], "estimated_cost_micros": 3_000 }),
        )
        .await;
    wait_status(
        &st1,
        "tenant-r",
        tk["id"].as_str().unwrap(),
        TaskCenterTaskStatus::Completed,
    )
    .await;
    pool1.close().await;

    let pool2 = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(opts)
        .await
        .unwrap();
    let db2 = open_sqlite_db(pool2.clone()).await;
    let st2 = TaskCenterState::new(db2.clone(), Arc::new(TaskCenterExecutor::unconfigured(db2)));
    let owner2 = Client::new(&st2, "tenant-r", "owner-r", &["owner"], 3600);
    let (code, sum, _) = owner2.get("/summary").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        (
            sum["spaces"][0]["name"].as_str(),
            sum["tasks"][0]["status"].as_str(),
            sum["budget"]["spent_micros"].as_i64()
        ),
        (Some("Durable Space"), Some("completed"), Some(270))
    );
    pool2.close().await;
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn sse_stream_enforces_tenant_isolation_jwt_expiry_lag_resync_and_frame_safety() {
    let db = new_memory_db().await;
    let st = TaskCenterState::new(db.clone(), Arc::new(TaskCenterExecutor::unconfigured(db)));

    let forged = TaskCenterSseEvent {
        event_id: "evt-1".into(),
        kind: "task_updated\ndata: injected\n\nevent: evil".into(),
        tenant_id: "tenant-a".into(),
        occurred_at_unix: 100,
        payload: json!({ "ok": true }),
    };
    let frame = format_sse_frame(&forged.kind, &forged);
    assert!(frame.starts_with("event: task_updated\ndata: ") && !frame.contains("\nevent: evil"));

    let expired = Client::new(&st, "tenant-a", "owner-a", &["owner"], -60);
    assert_eq!(expired.get("/stream").await.0, StatusCode::UNAUTHORIZED);

    let mut sa = TaskCenterEventStream::new(st.events.subscribe("tenant-a"), "tenant-a".into());
    let mut sb = TaskCenterEventStream::new(st.events.subscribe("tenant-b"), "tenant-b".into());
    assert!(sa.next().await.unwrap().unwrap().contains("ping"));
    assert!(sb.next().await.unwrap().unwrap().contains("ping"));

    st.events.publish(
        "tenant-a",
        SseEventKind::SpaceUpdated,
        json!({ "id": "spc-wake" }),
    );
    let ev = tokio::time::timeout(Duration::from_millis(200), sa.next())
        .await
        .expect("wake")
        .unwrap()
        .unwrap();
    assert!(ev.starts_with("event: space_updated\n") && ev.contains("spc-wake"));
    assert!(
        tokio::time::timeout(Duration::from_millis(60), sb.next())
            .await
            .is_err()
    );

    let (tx, rx) = broadcast::channel::<TaskCenterSseEvent>(1);
    let mut lagged = TaskCenterEventStream::new(rx, "tenant-lag".into());
    for i in 0..5 {
        let _ = tx.send(TaskCenterSseEvent {
            event_id: format!("evt-{i}"),
            kind: "task_updated".into(),
            tenant_id: "tenant-lag".into(),
            occurred_at_unix: 100 + i,
            payload: json!({ "seq": i }),
        });
    }
    let frame = lagged.next().await.unwrap().unwrap();
    assert!(frame.starts_with("event: resync_required\n") && frame.contains("\"missed\""));
}
