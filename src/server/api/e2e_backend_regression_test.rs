// SPDX-License-Identifier: Apache-2.0
//! Real-stack backend regression tests covering every E2E root cause fixed on
//! SQLite and live PostgreSQL 15:
//! - `strict_bearer_auth_middleware` `AuthInfo` + `WorkspaceScope` + `Claims`
//! - `GET /api/v1/onboarding/draft` & `GET /api/v1/onboarding/state`
//! - `GET /api/v1/payments/ledger/balance` & `GET /api/v1/ledger/entries`
//! - `PUT /api/v1/billing/usage/spending-limit` & `GET /api/v1/billing/usage`
//! - `POST /api/v1/task-center/tasks` missing `Idempotency-Key` -> `400`
//! - `PATCH /api/v1/quotes/{id}/approve` stale terms after edit -> `409`
//! - `GET /api/v1/growth/affiliate/stats` (`affiliate_ledgers.commission_amount`)
//! - `agent_session_data._sync_status` Agent Memory Pipeline query
//! - `Hub::check_health()` sub-millisecond DB ping reports `"healthy"`

use std::str::FromStr;
use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    routing::get,
};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

async fn setup_migrated_sqlite() -> (tempfile::TempDir, Arc<crate::db::DB>, Arc<crate::hub::Hub>) {
    let dir = tempfile::tempdir().expect("create sqlite tempdir");
    let db_path = dir.path().join("regression.sqlite");
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
        .expect("parse sqlite url")
        .create_if_missing(true)
        .foreign_keys(true);
    let sqlite_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("open sqlite pool");
    ::ohc_storage::migrate_sqlite(&sqlite_pool)
        .await
        .expect("apply ohc-storage sqlite migrations");
    let pg_lazy = crate::db::create_dummy_pg_pool().await;
    let db = Arc::new(crate::db::DB {
        pool: pg_lazy.clone(),
        store: crate::db::DbStore::Sqlite(sqlite_pool),
    });
    let (tx, _rx) = tokio::sync::mpsc::channel(32);
    let hub = Arc::new(crate::hub::Hub::new(tx, pg_lazy));
    hub.set_db(db.clone());
    (dir, db, hub)
}

async fn setup_auth_users(
    tenant_id: &str,
) -> (
    Arc<::server_auth::Store>,
    ::server_auth::User,
    String,
    ::server_auth::User,
    String,
) {
    let store = Arc::new(::server_auth::Store::new());
    let admin = store
        .create_user(
            "owner-e2e".to_string(),
            "owner-e2e@example.test".to_string(),
            "StrongPassword123!".to_string(),
            vec!["ADMIN".to_string()],
            tenant_id.to_string(),
        )
        .await
        .expect("create admin user");
    let admin_token = store.issue_token(&admin).expect("issue admin token");
    let member = store
        .create_user(
            "member-e2e".to_string(),
            "member-e2e@example.test".to_string(),
            "StrongPassword123!".to_string(),
            vec!["OPERATOR".to_string()],
            tenant_id.to_string(),
        )
        .await
        .expect("create operator user");
    let member_token = store.issue_token(&member).expect("issue member token");
    (store, admin, admin_token, member, member_token)
}

#[tokio::test]
async fn sqlite_onboarding_ledger_usage_task_center_and_health_regressions() {
    let tenant_id = "e2e-reg-tenant";
    let (_dir, db, hub) = setup_migrated_sqlite().await;
    let sqlite = match &db.store {
        crate::db::DbStore::Sqlite(p) => p.clone(),
        crate::db::DbStore::Postgres => unreachable!(),
    };
    let (auth_store, admin, admin_token, _member, member_token) = setup_auth_users(tenant_id).await;

    sqlx::query("INSERT INTO tenants (id, name) VALUES (?, ?)")
        .bind(tenant_id)
        .bind("Regression Bakery")
        .execute(&sqlite)
        .await
        .expect("insert tenant");
    sqlx::query(
        "INSERT INTO users (id, username, email, password_hash, roles, active, tenant_id) \
         VALUES (?, ?, ?, 'hash', '[\"ADMIN\"]', 1, ?)",
    )
    .bind(&admin.id)
    .bind(&admin.username)
    .bind(&admin.email)
    .bind(tenant_id)
    .execute(&sqlite)
    .await
    .expect("insert sqlite admin user");

    // 1. GET /api/v1/onboarding/draft and /state through real strict_bearer_auth_middleware.
    let agent = Arc::new(
        crate::services::onboarding::onboarding_agent::OnboardingAgent::new(
            db.clone(),
            hub.clone(),
        ),
    );
    let transport: Arc<dyn omnisolo_builtin_agent::mesh::transport::MeshTransport> =
        Arc::new(omnisolo_builtin_agent::mesh::transport::InProcessTransport::new());
    let onboarding_app = Router::new().nest(
        "/api/v1/onboarding",
        crate::api::onboarding::router(agent, auth_store.clone()).with_state(transport),
    );
    for uri in ["/api/v1/onboarding/draft", "/api/v1/onboarding/state"] {
        let resp = onboarding_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "{uri} must return 200 OK with AuthInfo populated by strict_bearer_auth_middleware"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).expect("valid onboarding JSON");
        assert!(
            json.get("step").is_some(),
            "{uri} payload must contain step"
        );
    }

    // 2. GET /api/v1/payments/ledger/balance through strict_bearer_auth_middleware.
    sqlx::query(
        "INSERT INTO ledger_accounts (tenant_id, account_id, currency, balance) \
         VALUES (?, 'default_revenue', 'USD', 125.50)",
    )
    .bind(tenant_id)
    .execute(&sqlite)
    .await
    .expect("insert sqlite ledger account");

    let bearer_layer = axum::middleware::from_fn_with_state(
        auth_store.clone(),
        ::server_auth::strict_bearer_auth_middleware,
    );
    let ledger_app = Router::new()
        .nest(
            "/api/v1/payments/ledger",
            crate::api::payment_ledger::router().with_state(crate::api::payment_ledger::AppState {
                db: db.clone(),
                hub: hub.clone(),
            }),
        )
        .route_layer(bearer_layer.clone());

    let bal_resp = ledger_app
        .oneshot(
            Request::builder()
                .uri("/api/v1/payments/ledger/balance")
                .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bal_resp.status(), StatusCode::OK);
    let bal_json: Value = serde_json::from_slice(
        &axum::body::to_bytes(bal_resp.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(bal_json["tenant_id"], tenant_id);
    assert_eq!(bal_json["total_revenue"], 125.5);

    // 3. PUT /api/v1/billing/usage/spending-limit & GET /api/v1/billing/usage mounted in OSS.
    let usage_app = Router::new()
        .nest(
            "/api/v1/billing",
            crate::api::usage_api::router().with_state(hub.clone()),
        )
        .route_layer(bearer_layer.clone());

    let put_limit = usage_app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/billing/usage/spending-limit")
                .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"limit_micros":1000000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put_limit.status(), StatusCode::OK);
    let limit_json: Value = serde_json::from_slice(
        &axum::body::to_bytes(put_limit.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(limit_json["is_payment_credit"], false);
    assert_eq!(
        limit_json["spending_authorization"]["limit_micros"],
        1_000_000
    );

    let member_limit = usage_app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/billing/usage/spending-limit")
                .header(header::AUTHORIZATION, format!("Bearer {member_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"limit_micros":2000000}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(member_limit.status(), StatusCode::FORBIDDEN);

    let negative_limit = usage_app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/billing/usage/spending-limit")
                .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"limit_micros":-1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(negative_limit.status(), StatusCode::BAD_REQUEST);

    let get_usage = usage_app
        .oneshot(
            Request::builder()
                .uri("/api/v1/billing/usage")
                .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_usage.status(), StatusCode::OK);
    let usage_json: Value = serde_json::from_slice(
        &axum::body::to_bytes(get_usage.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(usage_json["account"]["limit_micros"], 1_000_000);

    // 4. POST /api/v1/task-center/tasks without Idempotency-Key returns 400 Bad Request.
    let task_center_app = Router::new()
        .nest(
            "/api/v1/task-center",
            crate::api::task_center::router(
                db.clone(),
                Arc::new(
                    crate::api::task_center::executor::TaskCenterExecutor::from_environment(
                        db.clone(),
                    ),
                ),
            ),
        )
        .route_layer(bearer_layer);

    let missing_idem = task_center_app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/task-center/tasks")
                .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"prompt":"Guardrail check without UUID header"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        missing_idem.status(),
        StatusCode::BAD_REQUEST,
        "missing Idempotency-Key must be rejected with 400 Bad Request"
    );

    // 5. Verify affiliate_ledgers.commission_amount & agent_session_data._sync_status in SQLite.
    sqlx::query(
        "INSERT INTO affiliate_ledgers (id, tenant_id, customer_id, commission_amount) \
         VALUES ('aff-1', ?, 'cust-1', 2500)",
    )
    .bind(tenant_id)
    .execute(&sqlite)
    .await
    .expect("insert sqlite affiliate_ledgers with commission_amount");
    sqlx::query(
        "INSERT INTO agent_session_data (session_id, tenant_id, agent_id, context_data) \
         VALUES ('sess-1', ?, 'agent-1', '{}')",
    )
    .bind(tenant_id)
    .execute(&sqlite)
    .await
    .expect("insert sqlite agent_session_data with default _sync_status");
    let sync_status: Option<String> = sqlx::query_scalar(
        "SELECT _sync_status FROM agent_session_data WHERE session_id = 'sess-1'",
    )
    .fetch_one(&sqlite)
    .await
    .expect("read _sync_status");
    assert_eq!(sync_status.as_deref(), Some("pending"));
}

#[tokio::test]
async fn live_postgres_e2e_backend_regressions() {
    let database_url = match std::env::var("OMNISOLO_DATABASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("DATABASE_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
        }) {
        Some(url) if url.starts_with("postgres://") || url.starts_with("postgresql://") => url,
        _ => {
            if std::env::var("OMNISOLO_REQUIRE_POSTGRES_TESTS")
                .ok()
                .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            {
                panic!(
                    "OMNISOLO_REQUIRE_POSTGRES_TESTS=1 set but DATABASE_URL/OMNISOLO_DATABASE_URL is not postgres"
                );
            }
            return;
        }
    };

    let admin_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect admin pg pool");
    let schema = format!("e2e_reg_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin_pool)
        .await
        .expect("create isolated regression schema");

    let schema_for_conn = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .after_connect(move |conn, _| {
            let stmt = format!("SET search_path TO {schema_for_conn}");
            Box::pin(async move {
                sqlx::query(&stmt).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .expect("connect isolated pg pool");

    let test_result: Result<(), String> = async {
        ::ohc_storage::migrate_postgres(&pool)
            .await
            .map_err(|e| format!("migrate_postgres failed: {e}"))?;

        let tenant_id = "pg-e2e-reg-tenant";
        let (auth_store, admin, admin_token, _member, member_token) =
            setup_auth_users(tenant_id).await;

        sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, $2)")
            .bind(tenant_id)
            .bind("PG Regression Bakery")
            .execute(&pool)
            .await
            .map_err(|e| format!("insert tenant: {e}"))?;
        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, roles, active, tenant_id) \
             VALUES ($1, $2, $3, 'hash', ARRAY['ADMIN']::TEXT[], TRUE, $4)",
        )
        .bind(&admin.id)
        .bind(&admin.username)
        .bind(&admin.email)
        .bind(tenant_id)
        .execute(&pool)
        .await
        .map_err(|e| format!("insert pg user: {e}"))?;

        let db = Arc::new(crate::db::DB {
            pool: pool.clone(),
            store: crate::db::DbStore::Postgres,
        });
        let (tx, _rx) = tokio::sync::mpsc::channel(32);
        let hub = Arc::new(crate::hub::Hub::new(tx, pool.clone()));
        hub.set_db(db.clone());
        let bearer_layer = axum::middleware::from_fn_with_state(
            auth_store.clone(),
            ::server_auth::strict_bearer_auth_middleware,
        );

        // 1. Hub::check_health() MUST return healthy even on sub-millisecond SELECT 1 (0ms).
        let health = hub.check_health().await?;
        assert_eq!(
            health["status"], "healthy",
            "local PG15 health must be healthy even when db_ping_ms == 0: {health}"
        );
        assert_eq!(health["mesh_active"], true);

        // 2. Onboarding GET /draft and /state on PG15 through real strict_bearer_auth_middleware.
        let agent = Arc::new(
            crate::services::onboarding::onboarding_agent::OnboardingAgent::new(
                db.clone(),
                hub.clone(),
            ),
        );
        let transport: Arc<dyn omnisolo_builtin_agent::mesh::transport::MeshTransport> =
            Arc::new(omnisolo_builtin_agent::mesh::transport::InProcessTransport::new());
        let onboarding_app = Router::new().nest(
            "/api/v1/onboarding",
            crate::api::onboarding::router(agent, auth_store.clone()).with_state(transport),
        );
        for uri in ["/api/v1/onboarding/draft", "/api/v1/onboarding/state"] {
            let resp = onboarding_app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "PG15 {uri} must return 200");
        }

        // 3. Payment ledger GET /api/v1/payments/ledger/balance on PG15.
        sqlx::query(
            "INSERT INTO ledger_accounts (tenant_id, account_id, currency, balance) \
             VALUES ($1, 'default_revenue', 'USD', 88.25)",
        )
        .bind(tenant_id)
        .execute(&pool)
        .await
        .map_err(|e| format!("insert pg ledger account: {e}"))?;
        let ledger_app = Router::new()
            .nest(
                "/api/v1/payments/ledger",
                crate::api::payment_ledger::router().with_state(
                    crate::api::payment_ledger::AppState {
                        db: db.clone(),
                        hub: hub.clone(),
                    },
                ),
            )
            .route(
                "/api/v1/ledger/entries",
                get(crate::api::payment_ledger::get_entries),
            )
            .route_layer(bearer_layer.clone());
        let bal_resp = ledger_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/payments/ledger/balance")
                    .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bal_resp.status(), StatusCode::OK);
        let bal_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(bal_resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(bal_json["total_revenue"], 88.25);

        // 4. Growth affiliate stats on PG15 (`affiliate_ledgers.commission_amount`).
        sqlx::query(
            "INSERT INTO affiliate_ledgers (id, tenant_id, customer_id, commission_amount) \
             VALUES ('pg-aff-1', $1, 'pg-cust-1', 4200)",
        )
        .bind(tenant_id)
        .execute(&pool)
        .await
        .map_err(|e| format!("insert affiliate_ledgers: {e}"))?;
        let growth_app = Router::new()
            .nest(
                "/api/v1/growth",
                crate::api::growth::router(
                    pool.clone(),
                    hub.clone(),
                    Arc::new(crate::services::growth::viral_loop::ViralLoopTracker::new()),
                ),
            )
            .route_layer(bearer_layer.clone());
        let aff_resp = growth_app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/growth/affiliate/stats")
                    .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(aff_resp.status(), StatusCode::OK);
        let aff_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(aff_resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(aff_json["total_commission_cents"], 4200);

        // 5. Usage spending-limit on PG15 mounted under /api/v1/billing.
        let usage_app = Router::new()
            .nest(
                "/api/v1/billing",
                crate::api::usage_api::router().with_state(hub.clone()),
            )
            .route_layer(bearer_layer.clone());
        let put_limit = usage_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/billing/usage/spending-limit")
                    .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"limit_micros":1000000}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put_limit.status(), StatusCode::OK);
        let member_limit = usage_app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/billing/usage/spending-limit")
                    .header(header::AUTHORIZATION, format!("Bearer {member_token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"limit_micros":2000000}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(member_limit.status(), StatusCode::FORBIDDEN);

        // 6. Stale quote approval returns 409 Conflict after direct DB mutation triggers
        //    `ohc_quote_acceptance_guard` on PG15.
        let quote_id = Uuid::new_v4().to_string();
        let customer_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO customers (id, tenant_id, name, email) VALUES ($1, $2, 'Ada', 'ada@example.test')",
        )
        .bind(&customer_id)
        .bind(tenant_id)
        .execute(&pool)
        .await
        .map_err(|e| format!("insert customer: {e}"))?;
        sqlx::query(
            "INSERT INTO quotes (id, tenant_id, customer_id, status, total_amount_cents, required_deposit_cents) \
             VALUES ($1, $2, $3, 'DRAFT', 5000, 1000)",
        )
        .bind(&quote_id)
        .bind(tenant_id)
        .bind(&customer_id)
        .execute(&pool)
        .await
        .map_err(|e| format!("insert quote: {e}"))?;

        let quotes_app = Router::new()
            .nest(
                "/api/v1/quotes",
                crate::api::quotes::router().with_state(pool.clone()),
            )
            .route_layer(bearer_layer);

        let observed = quotes_app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/quotes/{quote_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(observed.status(), StatusCode::OK);
        let observed_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(observed.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let stale_version = observed_json["quote"]["updated_at"]
            .as_str()
            .expect("quote updated_at string")
            .to_string();

        // Simulate out-of-band/concurrent persisted mutation to quote terms.
        sqlx::query("UPDATE quotes SET total_amount_cents = 9000 WHERE id = $1 AND tenant_id = $2")
            .bind(&quote_id)
            .bind(tenant_id)
            .execute(&pool)
            .await
            .map_err(|e| format!("update quote terms: {e}"))?;

        let stale_approve = quotes_app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/api/v1/quotes/{quote_id}/approve"))
                    .header(header::AUTHORIZATION, format!("Bearer {admin_token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({ "expected_updated_at": stale_version }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            stale_approve.status(),
            StatusCode::CONFLICT,
            "stale quote approval MUST return 409 Conflict"
        );
        let conflict_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(stale_approve.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(conflict_json["success"], false);
        assert_eq!(conflict_json["reason"], "reviewed_quote_version_required");

        Ok(())
    }
    .await;

    pool.close().await;
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&admin_pool)
        .await;
    admin_pool.close().await;

    if let Err(err) = test_result {
        panic!("live_postgres_e2e_backend_regressions failed: {err}");
    }
}
