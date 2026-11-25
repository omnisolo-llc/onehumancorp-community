// SPDX-License-Identifier: Apache-2.0
//! Real-stack SQLite and live PostgreSQL 15 regression tests covering the final
//! backend E2E root causes:
//! - `POST /api/v1/billing/create-checkout-session` with isolated owner tenant
//!   different from static `SingleTenantContext` workspace ID: `200` on first
//!   single-unit stock lock + loopback Stripe session, `409` on exhausted stock,
//!   and `502` with clean inventory lock release on upstream Stripe failure
//! - `GET /api/v1/usage` mounted alongside `/api/v1/billing/*`: `200 OK` for
//!   both unconfigured new tenants (`limit_micros: null`) and configured tenants
//! - `Hub::check_health()` on OSS PG15 & SQLite without Cloud `agent_missions`
//!   sync columns (`sync_error` / `synced_to_cloud`): healthy with zero SQL errors

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode, header},
    routing::{get, post},
};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::sync::{Mutex, OnceCell};
use tower::ServiceExt;
use uuid::Uuid;

use crate::persistence::AppDatabase;

type RecordedStripeRequest = (Option<String>, BTreeMap<String, String>);

#[derive(Clone)]
struct LoopbackStripeState {
    expected_auth: String,
    fail_next: Arc<AtomicBool>,
    recorded: Arc<Mutex<Vec<RecordedStripeRequest>>>,
}

async fn loopback_stripe_sessions(
    State(state): State<LoopbackStripeState>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, Json<Value>) {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth != state.expected_auth {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        );
    }

    let idempotency = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let form: BTreeMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    state
        .recorded
        .lock()
        .await
        .push((idempotency, form.clone()));

    if state.fail_next.swap(false, Ordering::SeqCst) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "upstream unavailable" })),
        );
    }

    let amount_total = form
        .get("line_items[0][price_data][unit_amount]")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let session_id = format!("cs_test_{}", Uuid::new_v4().simple());
    (
        StatusCode::OK,
        Json(json!({
            "id": session_id,
            "url": format!("https://checkout.stripe.com/c/pay/{session_id}"),
            "payment_status": "unpaid",
            "amount_total": amount_total,
            "currency": "usd"
        })),
    )
}

async fn start_loopback_stripe(
    api_key: &str,
) -> (String, LoopbackStripeState, tokio::task::JoinHandle<()>) {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{api_key}:"));
    let state = LoopbackStripeState {
        expected_auth: format!("Basic {encoded}"),
        fail_next: Arc::new(AtomicBool::new(false)),
        recorded: Arc::new(Mutex::new(Vec::new())),
    };
    let app = Router::new()
        .route("/v1/checkout/sessions", post(loopback_stripe_sessions))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback stripe listener");
    let addr = listener.local_addr().expect("stripe listener local_addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), state, handle)
}

async fn stripe_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceCell<Mutex<()>> = OnceCell::const_new();
    LOCK.get_or_init(|| async { Mutex::new(()) }).await
}

async fn setup_sqlite() -> (tempfile::TempDir, Arc<crate::db::DB>, AppDatabase) {
    let dir = tempfile::tempdir().expect("create sqlite tempdir");
    let db_path = dir.path().join("last7.sqlite");
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
    let app_db = AppDatabase::from_connection(sea_orm::SqlxSqliteConnector::from_sqlx_sqlite_pool(
        sqlite_pool.clone(),
    ));
    crate::persistence::migration::migrate(&app_db)
        .await
        .expect("apply post-storage sqlite migrations");
    let pg_lazy = crate::db::create_dummy_pg_pool().await;
    let db = Arc::new(crate::db::DB {
        pool: pg_lazy,
        store: crate::db::DbStore::Sqlite(sqlite_pool),
    });
    (dir, db, app_db)
}

async fn setup_auth_users(tenant_id: &str) -> (Arc<::server_auth::Store>, String, String) {
    let store = Arc::new(::server_auth::Store::new());
    let owner = store
        .create_user(
            format!("owner-{tenant_id}"),
            format!("owner-{tenant_id}@example.test"),
            "StrongPassword123!".to_string(),
            vec!["ADMIN".to_string()],
            tenant_id.to_string(),
        )
        .await
        .expect("create owner user");
    let member = store
        .create_user(
            format!("member-{tenant_id}"),
            format!("member-{tenant_id}@example.test"),
            "StrongPassword123!".to_string(),
            vec!["MEMBER".to_string()],
            tenant_id.to_string(),
        )
        .await
        .expect("create member user");
    let owner_token = store.issue_token(&owner).expect("issue owner token");
    let member_token = store.issue_token(&member).expect("issue member token");
    (store, owner_token, member_token)
}

fn build_last7_router(
    db: Arc<crate::db::DB>,
    hub: Arc<crate::hub::Hub>,
    auth_store: Arc<::server_auth::Store>,
) -> Router {
    let bearer_layer = axum::middleware::from_fn_with_state(
        auth_store,
        ::server_auth::strict_bearer_auth_middleware,
    );
    Router::new()
        .route(
            "/api/v1/usage",
            get(crate::api::usage_api::overview_summary).with_state(hub.clone()),
        )
        .nest(
            "/api/v1/billing",
            crate::api::usage_api::router().with_state(hub).merge(
                crate::api::merchant_checkout::router(
                    crate::api::merchant_checkout::MerchantCheckoutState {
                        db,
                        // Deliberately use a different static single-tenant workspace ID
                        // to mirror `ohc-standalone` (`default_tenant`) serving an isolated
                        // owner tenant (`e2e-last7-*`).
                        tenant_ctx: Arc::new(
                            ::ohc_core::defaults::SingleTenantContext::new("default_tenant")
                                .expect("single-tenant context"),
                        ),
                        billing_platform: Arc::new(::ohc_core::defaults::NoOpPlatformBilling),
                        sync_transport: Arc::new(
                            ::ohc_core::defaults::InProcessLocalTransport::default(),
                        ),
                    },
                ),
            ),
        )
        .route_layer(bearer_layer)
}

#[tokio::test]
async fn sqlite_last7_backend_regressions() {
    let _env_guard = stripe_env_lock().await.lock().await;
    let tenant_id = format!("e2e-last7-sqlite-{}", Uuid::new_v4().simple());
    let (_dir, db, _app_db) = setup_sqlite().await;
    let sqlite = match &db.store {
        crate::db::DbStore::Sqlite(pool) => pool.clone(),
        crate::db::DbStore::Postgres => unreachable!(),
    };
    let (auth_store, owner_token, member_token) = setup_auth_users(&tenant_id).await;

    sqlx::query("INSERT INTO tenants (id, name) VALUES (?, ?)")
        .bind(&tenant_id)
        .bind("Last7 SQLite Shop")
        .execute(&sqlite)
        .await
        .expect("insert tenant");

    let product_id = Uuid::new_v4().to_string();
    let sold_out_product_id = Uuid::new_v4().to_string();
    let release_product_id = Uuid::new_v4().to_string();
    for (id, title, available, inv_count) in [
        (&product_id, "Last7 Single Unit", 1_i32, 1_i32),
        (&sold_out_product_id, "Cash Already Sold", 0_i32, 0_i32),
        (
            &release_product_id,
            "Release On Upstream Error",
            1_i32,
            1_i32,
        ),
    ] {
        sqlx::query(
            "INSERT INTO products (id, tenant_id, title, type, price, price_cents, inventory_count, available_quantity, locked_quantity) \
             VALUES (?, ?, ?, 'physical', 19.99, 1999, ?, ?, 0)",
        )
        .bind(id)
        .bind(&tenant_id)
        .bind(title)
        .bind(inv_count)
        .bind(available)
        .execute(&sqlite)
        .await
        .expect("insert product");
    }

    sqlx::query(
        "INSERT INTO agent_missions (id, tenant_id, status, payload) VALUES (?, ?, 'STUCK', '{}')",
    )
    .bind(format!("stuck-{}", Uuid::new_v4().simple()))
    .bind(&tenant_id)
    .execute(&sqlite)
    .await
    .expect("insert stuck mission");

    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let hub = Arc::new(crate::hub::Hub::new(tx, db.pool.clone()));
    hub.set_db(db.clone());
    let app = build_last7_router(db.clone(), hub.clone(), auth_store);

    // 1. GET /api/v1/usage succeeds with 200 OK for unconfigured new tenant.
    let unauth_usage = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/usage")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth_usage.status(), StatusCode::UNAUTHORIZED);

    let member_usage = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/usage")
                .header(header::AUTHORIZATION, format!("Bearer {member_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(member_usage.status(), StatusCode::FORBIDDEN);

    let initial_usage = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/usage")
                .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(initial_usage.status(), StatusCode::OK);
    let initial_usage_json: Value = serde_json::from_slice(
        &axum::body::to_bytes(initial_usage.into_body(), 1024 * 64)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(initial_usage_json["total_micros"], 0);
    assert!(initial_usage_json["limit_micros"].is_null());
    assert_eq!(initial_usage_json["total_requests"], 0);
    assert!(initial_usage_json["account"].is_null());

    // Configure limit via /api/v1/billing/usage/spending-limit and verify GET /api/v1/usage reflects it.
    let set_limit = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v1/billing/usage/spending-limit")
                .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "limit_micros": 100_000_000_i64 }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(set_limit.status(), StatusCode::OK);

    let configured_usage = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/usage")
                .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(configured_usage.status(), StatusCode::OK);
    let configured_json: Value = serde_json::from_slice(
        &axum::body::to_bytes(configured_usage.into_body(), 1024 * 64)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(configured_json["total_micros"], 0);
    assert_eq!(configured_json["limit_micros"], 100_000_000);
    assert_eq!(configured_json["account"]["limit_micros"], 100_000_000);

    // 2. Hub::check_health() on SQLite without Cloud agent_missions sync columns.
    let health = hub.check_health().await.expect("sqlite check_health");
    assert_eq!(health["status"], "healthy");
    assert_eq!(health["local_to_cloud_sync_queue"], 0);
    assert_eq!(health["sync_error_count"], 0);
    assert_eq!(health["stuck_missions"], 1);

    // 3. Merchant checkout: 200 on first valid reservation, 409 on exhausted stock, 502 + lock release on Stripe failure.
    let stripe_key = format!("sk_test_local_{}", Uuid::new_v4().simple());
    let (stripe_base, stripe_state, stripe_handle) = start_loopback_stripe(&stripe_key).await;

    temp_env::async_with_vars(
        [
            ("STRIPE_API_BASE", Some(stripe_base.as_str())),
            ("STRIPE_API_KEY", Some(stripe_key.as_str())),
            ("PUBLIC_APP_URL", Some("http://127.0.0.1:3000")),
        ],
        async {
            let first = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/billing/create-checkout-session")
                        .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            json!({ "is_subscription": false, "product_id": product_id, "quantity": 1 })
                                .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(first.status(), StatusCode::OK);
            let first_json: Value = serde_json::from_slice(
                &axum::body::to_bytes(first.into_body(), 1024 * 64)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert!(
                first_json["checkout_url"]
                    .as_str()
                    .is_some_and(|u| u.starts_with("https://checkout.stripe.com/c/pay/cs_test_"))
            );

            let stock_after_first: (i32, i32, i32) = sqlx::query_as(
                "SELECT inventory_count, available_quantity, locked_quantity FROM products WHERE id = ? AND tenant_id = ?",
            )
            .bind(&product_id)
            .bind(&tenant_id)
            .fetch_one(&sqlite)
            .await
            .unwrap();
            assert_eq!(stock_after_first, (1, 0, 1));

            // Second checkout for the same product must fail with 409 Conflict and zero new Stripe calls.
            let second = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/billing/create-checkout-session")
                        .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            json!({ "is_subscription": false, "product_id": product_id, "quantity": 1 })
                                .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(second.status(), StatusCode::CONFLICT);

            // Product already exhausted by cash terminal must return 409 Conflict with zero Stripe calls.
            let sold_out = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/billing/create-checkout-session")
                        .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            json!({ "is_subscription": false, "product_id": sold_out_product_id, "quantity": 1 })
                                .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(sold_out.status(), StatusCode::CONFLICT);
            assert_eq!(stripe_state.recorded.lock().await.len(), 1);

            // Upstream Stripe failure must return 502 and release the reserved stock back to available.
            stripe_state.fail_next.store(true, Ordering::SeqCst);
            let failed_upstream = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/billing/create-checkout-session")
                        .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            json!({ "is_subscription": false, "product_id": release_product_id, "quantity": 1 })
                                .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(failed_upstream.status(), StatusCode::BAD_GATEWAY);
            let restored_stock: (i32, i32, i32) = sqlx::query_as(
                "SELECT inventory_count, available_quantity, locked_quantity FROM products WHERE id = ? AND tenant_id = ?",
            )
            .bind(&release_product_id)
            .bind(&tenant_id)
            .fetch_one(&sqlite)
            .await
            .unwrap();
            assert_eq!(restored_stock, (1, 1, 0));
        },
    )
    .await;

    stripe_handle.abort();
}

#[tokio::test]
async fn live_postgres_last7_backend_regressions() {
    let _env_guard = stripe_env_lock().await.lock().await;
    let database_url = match std::env::var("OHC_STORAGE_TEST_DATABASE_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("OMNISOLO_DATABASE_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .or_else(|| {
            std::env::var("DATABASE_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
        }) {
        Some(url) if url.starts_with("postgres://") || url.starts_with("postgresql://") => url,
        _ => {
            if std::env::var("OMNISOLO_REQUIRE_POSTGRES_TESTS").as_deref() == Ok("1") {
                panic!(
                    "OMNISOLO_REQUIRE_POSTGRES_TESTS=1 but no PostgreSQL URL was set for live_postgres_last7_backend_regressions"
                );
            }
            return;
        }
    };

    let schema = format!("last7_reg_{}", Uuid::new_v4().simple());
    let admin_pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("connect PG admin pool");
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin_pool)
        .await
        .expect("create isolated PG schema");

    let search_path_sql = format!("SET search_path TO {schema}, public");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .after_connect(move |conn, _meta| {
            let sql = search_path_sql.clone();
            Box::pin(async move {
                sqlx::query(&sql).execute(conn).await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .expect("connect schema-scoped PG pool");

    let stripe_key = format!("sk_test_local_{}", Uuid::new_v4().simple());
    let (stripe_base, stripe_state, stripe_handle) = start_loopback_stripe(&stripe_key).await;

    let result: Result<(), String> = async {
        ::ohc_storage::migrate_postgres(&pool)
            .await
            .map_err(|e| format!("migrate_postgres: {e}"))?;
        let app_db = AppDatabase::from_connection(
            sea_orm::SqlxPostgresConnector::from_sqlx_postgres_pool(pool.clone()),
        );
        crate::persistence::migration::migrate(&app_db)
            .await
            .map_err(|e| format!("post-storage migrate: {e}"))?;

        let db = Arc::new(crate::db::DB {
            pool: pool.clone(),
            store: crate::db::DbStore::Postgres,
        });
        let tenant_id = format!("e2e-last7-pg-{}", Uuid::new_v4().simple());
        let (auth_store, owner_token, member_token) = setup_auth_users(&tenant_id).await;

        sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, $2)")
            .bind(&tenant_id)
            .bind("Last7 PG15 Shop")
            .execute(&pool)
            .await
            .map_err(|e| format!("insert tenant: {e}"))?;

        let product_id = Uuid::new_v4().to_string();
        let cash_exhausted_id = Uuid::new_v4().to_string();
        let release_product_id = Uuid::new_v4().to_string();
        for (id, title, available, inv_count) in [
            (&product_id, "PG15 Owned Last Unit", 1_i32, 1_i32),
            (&cash_exhausted_id, "PG15 Cash Exhausted", 0_i32, 0_i32),
            (&release_product_id, "PG15 Release On Error", 1_i32, 1_i32),
        ] {
            sqlx::query(
                "INSERT INTO products (id, tenant_id, title, type, price, price_cents, inventory_count, available_quantity, locked_quantity) \
                 VALUES ($1, $2, $3, 'physical', 19.99, 1999, $4, $5, 0)",
            )
            .bind(id)
            .bind(&tenant_id)
            .bind(title)
            .bind(inv_count)
            .bind(available)
            .execute(&pool)
            .await
            .map_err(|e| format!("insert product {id}: {e}"))?;
        }

        sqlx::query(
            "INSERT INTO agent_missions (id, tenant_id, status, payload) VALUES ($1, $2, 'STUCK', '{}')",
        )
        .bind(format!("pg-stuck-{}", Uuid::new_v4().simple()))
        .bind(&tenant_id)
        .execute(&pool)
        .await
        .map_err(|e| format!("insert stuck mission: {e}"))?;

        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let hub = Arc::new(crate::hub::Hub::new(tx, pool.clone()));
        hub.set_db(db.clone());
        let app = build_last7_router(db.clone(), hub.clone(), auth_store);

        // 1. GET /api/v1/usage on live PG15: 200 OK before & after spending limit configuration.
        let member_usage = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/usage")
                    .header(header::AUTHORIZATION, format!("Bearer {member_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(member_usage.status(), StatusCode::FORBIDDEN);

        let initial_usage = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/usage")
                    .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(initial_usage.status(), StatusCode::OK);
        let initial_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(initial_usage.into_body(), 1024 * 64)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(initial_json["total_micros"], 0);
        assert!(initial_json["limit_micros"].is_null());
        assert_eq!(initial_json["total_requests"], 0);

        let set_limit = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/billing/usage/spending-limit")
                    .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(json!({ "limit_micros": 50_000_000_i64 }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(set_limit.status(), StatusCode::OK);

        let after_limit = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/usage")
                    .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(after_limit.status(), StatusCode::OK);
        let after_limit_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(after_limit.into_body(), 1024 * 64)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(after_limit_json["limit_micros"], 50_000_000);
        assert_eq!(after_limit_json["account"]["limit_micros"], 50_000_000);

        // 2. Hub::check_health() on OSS PG15 with zero sync_error / synced_to_cloud SQL errors.
        let health = hub
            .check_health()
            .await
            .map_err(|e| format!("pg check_health: {e}"))?;
        assert_eq!(health["status"], "healthy");
        assert_eq!(health["local_to_cloud_sync_queue"], 0);
        assert_eq!(health["sync_error_count"], 0);
        assert_eq!(health["stuck_missions"], 1);

        // 3. POST /api/v1/billing/create-checkout-session on live PG15: 200 on first valid lock,
        //    409 on exhausted stock, 502 with lock release on upstream failure.
        temp_env::async_with_vars(
            [
                ("STRIPE_API_BASE", Some(stripe_base.as_str())),
                ("STRIPE_API_KEY", Some(stripe_key.as_str())),
                ("PUBLIC_APP_URL", Some("http://127.0.0.1:3000")),
            ],
            async {
                let first = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/api/v1/billing/create-checkout-session")
                            .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                json!({ "is_subscription": false, "product_id": product_id, "quantity": 1 })
                                    .to_string(),
                            ))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(first.status(), StatusCode::OK);
                let first_json: Value = serde_json::from_slice(
                    &axum::body::to_bytes(first.into_body(), 1024 * 64)
                        .await
                        .unwrap(),
                )
                .unwrap();
                assert!(
                    first_json["checkout_url"]
                        .as_str()
                        .is_some_and(|u| u.starts_with("https://checkout.stripe.com/c/pay/cs_test_"))
                );

                // Verify exact Stripe provider wire contract & persisted inventory hold.
                let recorded = stripe_state.recorded.lock().await;
                assert_eq!(recorded.len(), 1);
                let (idem_key, form) = &recorded[0];
                assert!(
                    idem_key
                        .as_deref()
                        .is_some_and(|k| k.starts_with("billing_checkout:"))
                );
                assert_eq!(form.get("client_reference_id").map(String::as_str), Some(tenant_id.as_str()));
                assert_eq!(form.get("metadata[checkout_kind]").map(String::as_str), Some("merchant_order"));
                assert_eq!(form.get("metadata[product_id]").map(String::as_str), Some(product_id.as_str()));
                assert_eq!(form.get("line_items[0][price_data][unit_amount]").map(String::as_str), Some("1999"));
                assert!(form.get("metadata[inventory_lock_id]").is_some_and(|v| !v.is_empty()));
                drop(recorded);

                let stock_after_first: (i32, i32, i32) = sqlx::query_as(
                    "SELECT inventory_count, available_quantity, locked_quantity FROM products WHERE id = $1 AND tenant_id = $2",
                )
                .bind(&product_id)
                .bind(&tenant_id)
                .fetch_one(&pool)
                .await
                .unwrap();
                assert_eq!(stock_after_first, (1, 0, 1));

                // Second checkout against the now-locked product must return 409 Conflict.
                let second = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/api/v1/billing/create-checkout-session")
                            .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                json!({ "is_subscription": false, "product_id": product_id, "quantity": 1 })
                                    .to_string(),
                            ))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(second.status(), StatusCode::CONFLICT);

                // Cash-exhausted product must return 409 Conflict without contacting Stripe.
                let exhausted = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/api/v1/billing/create-checkout-session")
                            .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                json!({ "is_subscription": false, "product_id": cash_exhausted_id, "quantity": 1 })
                                    .to_string(),
                            ))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(exhausted.status(), StatusCode::CONFLICT);
                assert_eq!(stripe_state.recorded.lock().await.len(), 1);

                // Upstream Stripe failure must return 502 and release reserved inventory.
                stripe_state.fail_next.store(true, Ordering::SeqCst);
                let upstream_err = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/api/v1/billing/create-checkout-session")
                            .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(
                                json!({ "is_subscription": false, "product_id": release_product_id, "quantity": 1 })
                                    .to_string(),
                            ))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(upstream_err.status(), StatusCode::BAD_GATEWAY);
                let restored: (i32, i32, i32) = sqlx::query_as(
                    "SELECT inventory_count, available_quantity, locked_quantity FROM products WHERE id = $1 AND tenant_id = $2",
                )
                .bind(&release_product_id)
                .bind(&tenant_id)
                .fetch_one(&pool)
                .await
                .unwrap();
                assert_eq!(restored, (1, 1, 0));
            },
        )
        .await;

        Ok(())
    }
    .await;

    stripe_handle.abort();
    pool.close().await;
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&admin_pool)
        .await;
    admin_pool.close().await;

    if let Err(err) = result {
        panic!("live_postgres_last7_backend_regressions failed: {err}");
    }
}
