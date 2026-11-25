// SPDX-License-Identifier: Apache-2.0
//! Real-stack SQLite and live PostgreSQL 15 regression tests covering the final
//! backend E2E root causes:
//! - First-party `agent_definitions` seeded idempotently on SQLite & PG15
//! - `ThrottlingManager::check_and_consume_budget` without Cloud `plan_tier`/`tenant_ai_budgets`
//! - `delivery_provider_bindings.last_event_at_ms IS NULL` for new shipping labels
//! - `pos_inventory` adjustments succeeding with `NULL` `pn_counter_p`/`pn_counter_n`
//! - `PATCH`/`GET /api/v1/assistant/settings` `application_settings` `(tenant_id, key)` upsert
//! - `WorkflowExecution::available_policy` with explicit tenant-bound `managed_api` funding
//! - `GET /api/v1/growth/birthday-club/embed` & `/embed/widget` strict CSP with zero inline `<script>`
//! - `POST /api/v1/billing/create-checkout-session` mounted in OSS alongside `usage_api`

use std::str::FromStr;
use std::sync::Arc;

use axum::{
    Extension, Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode, header},
};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

use crate::persistence::{AppDatabase, agent_definitions::DefinitionStore};
use crate::workflow_execution::{
    AdmittedAnalysis, AnalysisPolicy, InferenceResult, TextInference, WorkflowExecution,
    funding::{FundingContext, FundingPolicy},
};

struct UnavailableTextInference;

impl TextInference for UnavailableTextInference {
    fn infer<'a>(
        &'a self,
        _input: &'a AdmittedAnalysis,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<InferenceResult, ()>> + Send + 'a>>
    {
        Box::pin(async { Err(()) })
    }
}

async fn setup_sqlite() -> (tempfile::TempDir, Arc<crate::db::DB>, AppDatabase) {
    let dir = tempfile::tempdir().expect("create sqlite tempdir");
    let db_path = dir.path().join("final29.sqlite");
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

async fn setup_owner_auth(
    tenant_id: &str,
) -> (Arc<::server_auth::Store>, ::server_auth::User, String) {
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
    let token = store.issue_token(&owner).expect("issue owner token");
    (store, owner, token)
}

#[tokio::test]
async fn sqlite_final29_backend_regressions() {
    let tenant_id = "sqlite-final29-tenant";
    let (_dir, db, app_db) = setup_sqlite().await;
    let sqlite = match &db.store {
        crate::db::DbStore::Sqlite(pool) => pool.clone(),
        crate::db::DbStore::Postgres => unreachable!(),
    };
    let (auth_store, owner, owner_token) = setup_owner_auth(tenant_id).await;

    sqlx::query("INSERT INTO tenants (id, name) VALUES (?, ?)")
        .bind(tenant_id)
        .bind("Final29 SQLite Bakery")
        .execute(&sqlite)
        .await
        .expect("insert tenant");
    sqlx::query(
        "INSERT INTO users (id, username, email, password_hash, roles, active, tenant_id) \
         VALUES (?, ?, ?, 'hash', '[\"ADMIN\"]', 1, ?)",
    )
    .bind(&owner.id)
    .bind(&owner.username)
    .bind(&owner.email)
    .bind(tenant_id)
    .execute(&sqlite)
    .await
    .expect("insert user");

    // 1. First-party agent_definitions returned via GET /api/v1/agents/definitions.
    let defs_app = crate::api::agents::definitions::router::<()>(
        DefinitionStore::Database(app_db.clone()),
        auth_store.clone(),
    );
    let defs_resp = defs_app
        .oneshot(
            Request::builder()
                .uri("/api/v1/agents/definitions")
                .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                .header("x-ohc-expected-user", &owner.id)
                .header("x-ohc-expected-tenant", tenant_id)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(defs_resp.status(), StatusCode::OK);
    let defs_json: Value = serde_json::from_slice(
        &axum::body::to_bytes(defs_resp.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    let names: Vec<&str> = defs_json["definitions"]
        .as_array()
        .expect("definitions array")
        .iter()
        .filter_map(|d| d["name"].as_str())
        .collect();
    assert_eq!(names, vec!["Senior Rust Developer", "Technical Writer"]);

    // 2. Department throttling succeeds on OSS SQLite without plan_tier / tenant_ai_budgets.
    let throttler =
        crate::orchestration::departments::throttling::ThrottlingManager::new(db.clone());
    assert!(
        throttler
            .check_and_consume_budget(tenant_id, 1)
            .await
            .expect("OSS sqlite throttling must succeed"),
    );
    assert!(
        !throttler
            .check_and_consume_budget(tenant_id, 1001)
            .await
            .expect("OSS sqlite throttling ceiling check"),
    );

    // 3. Assistant settings PATCH upsert twice + GET readback on SQLite (tenant_id, key) PK.
    let claims = auth_store
        .validate_token(&owner_token)
        .await
        .expect("validate owner token");
    let assistant_app = crate::api::assistant::router::<()>(db.clone()).layer(Extension(claims));
    for name in ["Alpha Assistant", "Updated Assistant"] {
        let resp = assistant_app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/settings")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(json!({ "agentName": name }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let read_resp = assistant_app
        .oneshot(
            Request::builder()
                .uri("/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(read_resp.status(), StatusCode::OK);
    let read_json: Value = serde_json::from_slice(
        &axum::body::to_bytes(read_resp.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(read_json["settings"]["agentName"], "Updated Assistant");

    // 4. Growth birthday-club & embed/widget return strict script-src 'none' CSP with zero <script>.
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let hub = Arc::new(crate::hub::Hub::new(tx, db.pool.clone()));
    hub.set_db(db.clone());
    let growth_app = crate::api::growth::router::<()>(
        db.pool.clone(),
        hub.clone(),
        Arc::new(crate::services::growth::viral_loop::ViralLoopTracker::new()),
    );
    for uri in [
        "/birthday-club/embed?tenant=my-store&discount=15&hideBranding=false",
        "/embed/widget?tenant=my-store&type=booking&theme=light",
    ] {
        let resp = growth_app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "embed route {uri}");
        let csp = resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            csp.contains("script-src 'none'"),
            "embed route {uri} must send strict script-src 'none' CSP, got: {csp}"
        );
        let html = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(
            !html.to_ascii_lowercase().contains("<script"),
            "embed route {uri} must not contain inline <script> under script-src 'none'"
        );
    }

    // 5. OSS /api/v1/billing mounts merchant_checkout alongside usage_api without conflict.
    let billing_app = Router::new()
        .nest(
            "/api/v1/billing",
            crate::api::usage_api::router()
                .with_state(hub.clone())
                .merge(crate::api::merchant_checkout::router(
                    crate::api::merchant_checkout::MerchantCheckoutState {
                        db: db.clone(),
                        tenant_ctx: Arc::new(
                            ::ohc_core::defaults::SingleTenantContext::new(tenant_id)
                                .expect("single-tenant context"),
                        ),
                        billing_platform: Arc::new(::ohc_core::defaults::NoOpPlatformBilling),
                        sync_transport: Arc::new(
                            ::ohc_core::defaults::InProcessLocalTransport::default(),
                        ),
                    },
                )),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            auth_store.clone(),
            ::server_auth::strict_bearer_auth_middleware,
        ));
    let missing_prod = billing_app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/billing/create-checkout-session")
                .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({ "is_subscription": false, "product_id": "missing-prod", "quantity": 1 })
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        missing_prod.status(),
        StatusCode::NOT_FOUND,
        "merchant_checkout must be mounted at /api/v1/billing/create-checkout-session and query real SQLite products"
    );
}

#[tokio::test]
async fn live_postgres_final29_backend_regressions() {
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
            if std::env::var("OMNISOLO_REQUIRE_POSTGRES_TESTS")
                .ok()
                .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            {
                panic!(
                    "OMNISOLO_REQUIRE_POSTGRES_TESTS=1 requires a PostgreSQL URL in OHC_STORAGE_TEST_DATABASE_URL / OMNISOLO_DATABASE_URL / DATABASE_URL"
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
    let schema = format!("final29_reg_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&admin_pool)
        .await
        .expect("create isolated PG15 schema");

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
        .expect("connect isolated PG15 pool");

    let result: Result<(), String> = async {
        let app_db = AppDatabase::from_connection(
            sea_orm::SqlxPostgresConnector::from_sqlx_postgres_pool(pool.clone()),
        );
        crate::persistence::migration::migrate(&app_db)
            .await
            .map_err(|e| format!("AppDatabase::migrate failed: {e}"))?;

        let tenant_id = "pg-final29-tenant";
        let (auth_store, owner, owner_token) = setup_owner_auth(tenant_id).await;
        let claims = auth_store
            .validate_token(&owner_token)
            .await
            .map_err(|e| format!("validate owner token: {e}"))?;

        sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, $2)")
            .bind(tenant_id)
            .bind("PG15 Final29 Bakery")
            .execute(&pool)
            .await
            .map_err(|e| format!("insert tenant: {e}"))?;
        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, roles, active, tenant_id) \
             VALUES ($1, $2, $3, 'hash', ARRAY['ADMIN']::TEXT[], TRUE, $4)",
        )
        .bind(&owner.id)
        .bind(&owner.username)
        .bind(&owner.email)
        .bind(tenant_id)
        .execute(&pool)
        .await
        .map_err(|e| format!("insert pg user: {e}"))?;

        let db = Arc::new(crate::db::DB {
            pool: pool.clone(),
            store: crate::db::DbStore::Postgres,
        });
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let hub = Arc::new(crate::hub::Hub::new(tx, pool.clone()));
        hub.set_db(db.clone());
        let bearer_layer = axum::middleware::from_fn_with_state(
            auth_store.clone(),
            ::server_auth::strict_bearer_auth_middleware,
        );

        // 1. First-party Agent Marketplace catalog seeded on PG15.
        let defs_app = crate::api::agents::definitions::router::<()>(
            DefinitionStore::Database(app_db.clone()),
            auth_store.clone(),
        );
        let defs_resp = defs_app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/agents/definitions")
                    .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                    .header("x-ohc-expected-user", &owner.id)
                    .header("x-ohc-expected-tenant", tenant_id)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(defs_resp.status(), StatusCode::OK);
        let defs_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(defs_resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        let names: Vec<&str> = defs_json["definitions"]
            .as_array()
            .expect("pg definitions array")
            .iter()
            .filter_map(|d| d["name"].as_str())
            .collect();
        assert_eq!(names, vec!["Senior Rust Developer", "Technical Writer"]);

        // 2. Department throttling succeeds on OSS PG15 without plan_tier / tenant_ai_budgets.
        let throttler =
            crate::orchestration::departments::throttling::ThrottlingManager::new(db.clone());
        assert!(
            throttler
                .check_and_consume_budget(tenant_id, 1)
                .await
                .map_err(|e| format!("PG15 throttling failed: {e}"))?,
            "OSS PG15 throttling must allow bounded actions without plan_tier"
        );
        assert!(
            !throttler
                .check_and_consume_budget(tenant_id, 1001)
                .await
                .map_err(|e| format!("PG15 throttling ceiling check failed: {e}"))?,
            "OSS PG15 throttling must enforce ceiling"
        );

        // 3. delivery_provider_bindings.last_event_at_ms defaults to NULL on PG15.
        let ship_order_id = Uuid::new_v4().to_string();
        let delivery_task_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO orders (id, tenant_id, total_amount, currency, status) \
             VALUES ($1, $2, 19.99, 'USD', 'pending')",
        )
        .bind(&ship_order_id)
        .bind(tenant_id)
        .execute(&pool)
        .await
        .map_err(|e| format!("insert shipping order: {e}"))?;
        sqlx::query(
            "INSERT INTO delivery_tasks (id, tenant_id, organization_id, order_id, status, provider) \
             VALUES ($1, $2, $2, $3, 'LABEL_CREATED', 'shippo')",
        )
        .bind(delivery_task_id)
        .bind(tenant_id)
        .bind(&ship_order_id)
        .execute(&pool)
        .await
        .map_err(|e| format!("insert delivery_task: {e}"))?;
        sqlx::query(
            "INSERT INTO delivery_provider_bindings (delivery_task_id, tenant_id, organization_id, provider, account_namespace, is_test, provider_object_id) \
             VALUES ($1, $2, $2, 'shippo', 'acct-final29', TRUE, 'tx_final29_1')",
        )
        .bind(delivery_task_id)
        .bind(tenant_id)
        .execute(&pool)
        .await
        .map_err(|e| format!("insert delivery_provider_binding: {e}"))?;
        let last_event_at_ms: Option<i64> = sqlx::query_scalar(
            "SELECT last_event_at_ms FROM delivery_provider_bindings WHERE delivery_task_id = $1",
        )
        .bind(delivery_task_id)
        .fetch_one(&pool)
        .await
        .map_err(|e| format!("read last_event_at_ms: {e}"))?;
        assert_eq!(last_event_at_ms, None);

        // 4. Inventory adjustment on PG15 succeeds for plain-stock product with NULL pn_counters.
        let product_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO products (id, tenant_id, title, type, price, price_cents, inventory_count, available_quantity, locked_quantity) \
             VALUES ($1, $2, 'PG15 Plain Stock Cake', 'physical', 10.0, 1000, 12, 12, 0)",
        )
        .bind(&product_id)
        .bind(tenant_id)
        .execute(&pool)
        .await
        .map_err(|e| format!("insert pg product: {e}"))?;
        let items = crate::api::pos::read_inventory_postgres_for_test(&pool, tenant_id)
            .await
            .map_err(|e| format!("read_postgres inventory: {e:?}"))?;
        let initial_ver = items
            .iter()
            .find(|item| item["id"] == product_id)
            .and_then(|item| item["inventory_version"].as_str())
            .expect("initial inventory_version")
            .to_string();
        let dec_id = Uuid::new_v4().to_string();
        let dec_receipt = crate::api::pos::apply_inventory_postgres_for_test(
            &pool,
            tenant_id,
            &dec_id,
            &product_id,
            -1,
            &initial_ver,
        )
        .await
        .map_err(|e| format!("apply_postgres -1 failed: {e:?}"))?;
        assert_eq!(dec_receipt.status, "acknowledged");
        assert_eq!(dec_receipt.stock, 11);
        let inc_id = Uuid::new_v4().to_string();
        let inc_receipt = crate::api::pos::apply_inventory_postgres_for_test(
            &pool,
            tenant_id,
            &inc_id,
            &product_id,
            1,
            &dec_receipt.inventory_version,
        )
        .await
        .map_err(|e| format!("apply_postgres +1 failed: {e:?}"))?;
        assert_eq!(inc_receipt.status, "acknowledged");
        assert_eq!(inc_receipt.stock, 12);

        // 5. application_settings upsert on PG15 (PRIMARY KEY (tenant_id, key)).
        let assistant_app =
            crate::api::assistant::router::<()>(db.clone()).layer(Extension(claims.clone()));
        for name in ["PG Assistant v1", "PG Assistant v2"] {
            let resp = assistant_app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PATCH")
                        .uri("/settings")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(json!({ "agentName": name }).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
        let settings_resp = assistant_app
            .oneshot(
                Request::builder()
                    .uri("/settings")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(settings_resp.status(), StatusCode::OK);
        let settings_json: Value = serde_json::from_slice(
            &axum::body::to_bytes(settings_resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(settings_json["settings"]["agentName"], "PG Assistant v2");

        // 6. WorkflowExecution::available_policy with explicit managed_api funding on PG15.
        let policy = AnalysisPolicy::new(
            "openai-compatible".to_string(),
            "owned-browser-model".to_string(),
            128,
        )
        .map_err(|e| format!("AnalysisPolicy::new: {e:?}"))?;
        let rate_cards = json!({
            "openai-compatible/owned-browser-model": {
                "revision": "owned-browser-v1",
                "input_micros_per_million": 1_000_000,
                "output_micros_per_million": 2_000_000,
                "cached_input_micros_per_million": 1_000_000
            }
        })
        .to_string();
        let funding_policy = FundingPolicy::from_values_for_test(&policy, false, |key| match key {
            "OMNISOLO_LLM_TENANT_ID" => Some(tenant_id.to_string()),
            "OMNISOLO_USAGE_PAYER" => Some("managed_api".to_string()),
            "OMNISOLO_USAGE_MAX_REQUEST_MICROS" => Some("20000".to_string()),
            "OMNISOLO_USAGE_RATE_CARDS" => Some(rate_cards.clone()),
            _ => None,
        })
        .map_err(|e| format!("FundingPolicy managed_api failed: {e:?}"))?;
        let funding_ctx = FundingContext::new(funding_policy, &app_db)
            .map_err(|e| format!("FundingContext::new failed: {e:?}"))?;
        let execution = WorkflowExecution::configured_funded(
            auth_store.clone(),
            policy.clone(),
            Arc::new(UnavailableTextInference),
            funding_ctx,
        );
        let mut auth_headers = HeaderMap::new();
        auth_headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {owner_token}").parse().unwrap(),
        );
        // Without ohc_usage_accounts row -> available_policy returns Ok(None).
        assert_eq!(
            execution
                .available_policy(&claims, &auth_headers)
                .await
                .map_err(|e| format!("available_policy before account: {e:?}"))?,
            None
        );
        // With ohc_usage_accounts row -> available_policy returns Some(&policy).
        sqlx::query("INSERT INTO ohc_usage_accounts (tenant_id, limit_micros) VALUES ($1, 20000)")
            .bind(tenant_id)
            .execute(&pool)
            .await
            .map_err(|e| format!("insert ohc_usage_accounts: {e}"))?;
        assert_eq!(
            execution
                .available_policy(&claims, &auth_headers)
                .await
                .map_err(|e| format!("available_policy with account: {e:?}"))?,
            Some(&policy)
        );

        // 7. Growth birthday-club/embed & embed/widget on PG15 (including hideBranding=true).
        let growth_app = crate::api::growth::router::<()>(
            pool.clone(),
            hub.clone(),
            Arc::new(crate::services::growth::viral_loop::ViralLoopTracker::new()),
        );
        for uri in [
            "/birthday-club/embed?tenant=my-store&discount=15&hideBranding=false",
            &format!("/birthday-club/embed?tenant={tenant_id}&discount=15&hideBranding=true"),
            "/embed/widget?tenant=my-store&type=booking&theme=dark",
        ] {
            let resp = growth_app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "PG15 embed route {uri}");
            let csp = resp
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert!(csp.contains("script-src 'none'"));
            let html = String::from_utf8(
                axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            assert!(!html.to_ascii_lowercase().contains("<script"));
        }

        // 8. merchant_checkout mounted at /api/v1/billing/create-checkout-session on PG15.
        let billing_app = Router::new()
            .nest(
                "/api/v1/billing",
                crate::api::usage_api::router()
                    .with_state(hub.clone())
                    .merge(crate::api::merchant_checkout::router(
                        crate::api::merchant_checkout::MerchantCheckoutState {
                            db: db.clone(),
                            tenant_ctx: Arc::new(
                                ::ohc_core::defaults::SingleTenantContext::new(tenant_id)
                                    .expect("single-tenant context"),
                            ),
                            billing_platform: Arc::new(::ohc_core::defaults::NoOpPlatformBilling),
                            sync_transport: Arc::new(
                                ::ohc_core::defaults::InProcessLocalTransport::default(),
                            ),
                        },
                    )),
            )
            .route_layer(bearer_layer);
        let checkout_not_found = billing_app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/billing/create-checkout-session")
                    .header(header::AUTHORIZATION, format!("Bearer {owner_token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "is_subscription": false, "product_id": "missing-pg-prod", "quantity": 1 })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(checkout_not_found.status(), StatusCode::NOT_FOUND);

        Ok(())
    }
    .await;

    pool.close().await;
    let _ = sqlx::query(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
        .execute(&admin_pool)
        .await;
    admin_pool.close().await;

    if let Err(err) = result {
        panic!("live_postgres_final29_backend_regressions failed: {err}");
    }
}
