// SPDX-License-Identifier: Apache-2.0
//! OneHumanCorp open-source single-tenant core server crate (`server_lib`).

pub mod agents;
pub mod analytics;
pub mod api;
pub mod autodream;
pub mod autodream_pipeline;
pub mod benchmarks;
pub mod builder;
pub mod cart_recovery;
pub mod domain;
pub mod hub;
pub mod integrations;
pub mod interop;
pub mod local_usage;
pub mod minimax;
pub mod msgbus;
pub mod orchestration;
pub mod persistence;
#[cfg(test)]
mod persistence_commands_test;
pub mod pipeline;
pub mod queue;
pub mod rag_sync;
pub mod rpc_error;
pub mod runtime_shutdown;
pub mod scheduler;
pub mod server_builder;
pub mod settings;
pub mod sip;
pub mod storage;
pub mod tasks;
pub mod tools;
pub mod ultraplan;
pub mod voice;
pub mod workers;
pub mod workflow_execution;

pub mod db;
pub use ::server_auth as auth;
pub use ::server_common as common;
pub use ::server_config as config;
pub use ::server_harness as harness;
pub use ::server_omnisolo as ohc;
pub use ::server_telemetry as telemetry;
pub use ::server_utils as utils;

#[path = "api/setup.rs"]
mod setup;

pub use server_builder::http_gateway_helpers::WorkflowRecord;
pub use server_builder::ui_caches::{
    AI_CACHE, CreateTriageItemPayload, OmniInboxActionPayload, invalidate_agent_feed_caches,
    invalidate_legacy_triage_caches,
};
pub use server_builder::{
    OhcServerBuilder, ServerContext, auth_policy, billing_platform, distributed_cache,
    install_runtime_extensions, sync_transport, tenant_context,
};

pub mod proto {
    pub mod interop {
        pub use ::server_omnisolo::interop::*;
    }
    pub mod mcp_proxy {
        pub use ::server_omnisolo::mcp_proxy::*;
    }
    pub mod orchestration {
        pub use ::server_omnisolo::orchestration::*;
    }
    pub mod agent {
        pub use ::server_omnisolo::agent::*;
        pub mod service {
            pub use ::server_omnisolo::agent::service::*;
        }
    }
    pub mod organization {
        pub use ::server_omnisolo::organization::*;
    }
    pub mod common {
        pub use ::server_omnisolo::common::*;
    }
    pub mod inventory {
        pub use ::server_omnisolo::inventory::*;
    }
    pub mod app {
        pub use ::server_omnisolo::app::*;
    }
}

pub mod services {
    pub mod agent;
    pub mod agent_feed;
    pub mod autodream;
    pub mod b2b;
    pub mod booking;
    #[cfg(test)]
    pub mod campaign;
    pub mod chat;
    pub mod customer_memory_graph;
    pub mod dashboard;
    pub mod growth;
    pub mod intake;
    pub mod integration;
    pub mod inventory;
    pub mod inventory_sync;
    pub mod mcp;
    pub mod onboarding;
    pub mod ops;
    pub mod org;
    pub mod pos;
    pub mod scheduler;
    pub mod subscription;
    pub mod sync;
    pub mod wizard;
}

use std::sync::{Arc, OnceLock};
use tokio::sync::mpsc;

static TELEMETRY_CHAN: OnceLock<mpsc::Sender<Box<dyn FnOnce() + Send>>> = OnceLock::new();

fn get_telemetry_chan() -> &'static mpsc::Sender<Box<dyn FnOnce() + Send>> {
    TELEMETRY_CHAN.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Box<dyn FnOnce() + Send>>(10000);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));

        for _ in 0..16 {
            let rx = rx.clone();
            tokio::spawn(async move {
                loop {
                    let job = {
                        let mut rx = rx.lock().await;
                        rx.recv().await
                    };
                    if let Some(job) = job {
                        job();
                    } else {
                        break;
                    }
                }
            });
        }
        tx
    })
}

pub fn record_telemetry<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    let tx = get_telemetry_chan();
    let _ = tx.try_send(Box::new(f));
}

pub fn is_standalone_runtime() -> bool {
    server_builder::builder::configured_multi_tenant()
        .map(|multi_tenant| !multi_tenant)
        .unwrap_or_else(|| crate::config::get().standalone)
}

pub fn workflow_agent_binary() -> String {
    std::env::var("OMNISOLO_BUILTIN_AGENT_BINARY")
        .or_else(|_| std::env::var("OMNISOLO_AGENT_BINARY"))
        .unwrap_or_else(|_| {
            if is_standalone_runtime()
                && let Ok(exe_path) = std::env::current_exe()
            {
                return exe_path.to_string_lossy().to_string();
            }
            if let Ok(exe_path) = std::env::current_exe() {
                let agent_name = if cfg!(windows) {
                    "omnisolo-builtin-agent.exe"
                } else {
                    "omnisolo-builtin-agent"
                };
                exe_path
                    .with_file_name(agent_name)
                    .to_string_lossy()
                    .to_string()
            } else if cfg!(windows) {
                "omnisolo-builtin-agent.exe".to_string()
            } else {
                "omnisolo-builtin-agent".to_string()
            }
        })
}

pub fn workflow_agent_task(workflow: &str, task: &str) -> String {
    let workflow = if workflow.trim().is_empty() {
        "ohc_review_branch"
    } else {
        workflow.trim()
    };
    let args = serde_json::json!({
        "workflow": workflow,
        "task": task,
    });
    format!(
        "Use the built-in RunWorkflow tool. Arguments: {}. Return the final synthesized report.",
        args
    )
}

pub(crate) struct RegisteredWorkflowAgent {
    pub(crate) hub: Arc<hub::Hub>,
    pub(crate) agent_id: String,
    pub(crate) tenant_id: String,
}

impl workflow_execution::RegistrationLease for RegisteredWorkflowAgent {
    fn is_active(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            self.hub
                .get_agent(&self.agent_id)
                .await
                .is_some_and(|agent| agent.organization_id == self.tenant_id)
        })
    }

    fn finished<'a>(
        &'a self,
        status: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        Box::pin(async move {
            self.hub
                .update_agent_status(&self.agent_id, &self.tenant_id, status)
                .await;
        })
    }
}

// Boots the open-source single-tenant standalone server runtime.
pub async fn run_server() -> Result<(), Box<dyn std::error::Error>> {
    crate::utils::fs::cleanup_stale_temp_files();
    let use_json = std::env::var("LOG_FORMAT").unwrap_or_default() == "json";
    let subscriber = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::from_default_env()
            .add_directive(tracing::Level::INFO.into()),
    );
    if use_json {
        subscriber.json().init();
    } else {
        subscriber.init();
    }

    let db_path = crate::config::get_safe_user_dir().join("omnisolo-standalone.db");
    let sqlite_url = std::env::var("OMNISOLO_DATABASE_URL")
        .ok()
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .filter(|value| value.trim().starts_with("sqlite"))
        .unwrap_or_else(|| format!("sqlite://{}?mode=rwc", db_path.display()));

    let workspace_id = "default_tenant".to_string();
    let deployment = Arc::new(::ohc_core::defaults::SingleTenantRuntime::new(sqlite_url)?);
    let tenant_ctx = Arc::new(::ohc_core::defaults::SingleTenantContext::new(
        workspace_id.clone(),
    )?);
    let auth_policy = Arc::new(::ohc_core::defaults::LocalOwnerAuthPolicy::new(
        workspace_id,
    )?);
    let billing_platform = Arc::new(::ohc_core::defaults::NoOpPlatformBilling);
    let sync_transport = Arc::new(::ohc_core::defaults::InProcessLocalTransport::default());

    let shutdown = crate::runtime_shutdown::CancellationToken::new();
    crate::runtime_shutdown::install_signal_handler(shutdown.clone());

    OhcServerBuilder::new(
        deployment,
        tenant_ctx,
        auth_policy,
        billing_platform,
        sync_transport,
    )
    .serve_with_graceful_shutdown(shutdown)
    .await?;
    Ok(())
}

pub mod crypto;

#[cfg(test)]
mod triage_cache_tests {
    use super::server_builder::ui_caches::UI_TRIAGE_CACHE;

    #[tokio::test]
    async fn test_ui_triage_cache_initialization() {
        let cache = UI_TRIAGE_CACHE
            .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));
        let _ = cache.get("test_key").await;
        let _ = cache.get_with_swr("test_key").await;
    }
}

#[cfg(test)]
mod triage_create_tests {
    #[test]
    fn test_create_triage_payload_deserialization() {
        let json_data = r#"
        {
            "source": "Instagram DM",
            "priority": "Urgent",
            "context": "Customer wants a cake",
            "action_type": "Draft Reply",
            "action_payload": "Yes, we can make it!"
        }
        "#;

        let payload: super::CreateTriageItemPayload = serde_json::from_str(json_data).unwrap();
        assert_eq!(payload.source, Some("Instagram DM".to_string()));
        assert_eq!(payload.priority, Some("Urgent".to_string()));
        assert_eq!(payload.context, Some("Customer wants a cake".to_string()));
        assert_eq!(payload.action_type, Some("Draft Reply".to_string()));
        assert_eq!(
            payload.action_payload,
            Some("Yes, we can make it!".to_string())
        );
    }
}

#[cfg(test)]
mod tests {
    use super::server_builder::builder::{grpc_bind_host, grpc_tls_config_from_pem};
    use super::server_builder::http_gateway_helpers::{
        agent_rpc_available, agent_rpc_url, allowed_agent_rpc_method, current_help_link,
        default_help_chat_links, extend_agent_rpc_body, legacy_agent_rpc_router,
        protected_bearer_auth_middleware,
    };
    use super::server_builder::metrics_handlers::{
        dashboard_sales_query, dashboard_top_product_query,
    };
    use super::server_builder::ui_caches::{
        OmniInboxActionPayload, load_ui_omni_inbox_from_db, strict_ui_claim_tenant,
    };
    use super::settings::Store;
    use axum::routing::get;
    use std::sync::Arc;

    #[tokio::test]
    async fn router_composition_preserves_cart_and_builder_publications() {
        use axum::{Router, body::Body, http::Request, http::StatusCode};
        use omnisolo_builtin_agent::mesh::transport::{InProcessTransport, MeshTransport};
        use tower::ServiceExt;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://postgres@127.0.0.1:1/ohc_router_composition_test")
            .unwrap();
        let auth = Arc::new(server_auth::Store::new());
        let transport: Arc<dyn MeshTransport> = Arc::new(InProcessTransport::new());
        let db = Arc::new(crate::db::DB {
            pool: pool.clone(),
            store: crate::db::DbStore::Postgres,
        });
        let app: Router<Arc<dyn MeshTransport>> = Router::new()
            .nest(
                "/api/v1/field-ops",
                crate::api::field_ops::configured_router(
                    pool.clone(),
                    Some(pool.clone()),
                    transport.clone(),
                    auth.clone(),
                ),
            )
            .merge(crate::builder::publication_http::router(pool, auth));
        let _ = db;
        let app = app.with_state(transport);

        let publication = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/builder/publications")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(publication.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn rust_server_does_not_register_legacy_browser_application_routes() {
        let http_routes_source = include_str!("server_builder/http_routes.rs");
        let subrouters_source = include_str!("server_builder/http_subrouters.rs");
        let production_source = format!("{http_routes_source}\n{subrouters_source}");
        let legacy_routes = [
            "/api/v1/ui/help.html",
            "/help",
            "/api/v1/ui/help_article.html",
            "/api/v1/ui/api-docs.html",
            "/api-docs",
            "/api/v1/ui/swagger-ui.css",
            "/api/v1/ui/swagger-ui-bundle.js",
            "/kairos",
            "/kairos.html",
            "/tooltip-registry.html",
            "/api/v1/ui/hybrid-landing.html",
            "/api/v1/ui/changelog.html",
            "/",
            "/index.html",
            "/login",
            "/changelog",
            "/onboarding",
            "/chaos-report",
            "/api/v1/ui/dashboard.html",
            "/dashboard",
            "/dashboard.html",
            "/birthday-club-generator.html",
            "/agent-audit-dashboard.html",
            "/api/v1/ui/pos.html",
            "/pos.html",
            "/api/v1/ui/help-widget.mjs",
            "/api/v1/ui/voice-assistant.mjs",
            "/api/v1/ui/assistant.html",
            "/assistant.html",
            "/tasks",
            "/calendar",
            "/referrals",
            "/plan",
            "/cost-dashboard",
            "/gift-cards",
            "/pricing",
            "/social-share-widget.html",
        ];

        for path in legacy_routes {
            let route_registration = format!(".route({path:?},");
            let multiline_route_registration = format!(".route(\n            {path:?},");
            assert!(
                !production_source.contains(&route_registration)
                    && !production_source.contains(&multiline_route_registration),
                "Rust production code must not retain legacy browser path {path}"
            );
        }

        for api_route in ["/api/v1/help", "/api/v1/changelog", "/api/v1/api-docs-spec"] {
            let path_literal = format!("{api_route:?}");
            assert!(
                production_source.contains(&path_literal),
                "expected API route {api_route} to remain registered"
            );
        }
        assert!(production_source.contains(".fallback(api_not_found_handler)"));
    }

    #[test]
    fn first_party_agent_launch_contract_uses_omnisolo_binary_names() {
        let production_source = include_str!("lib.rs")
            .rsplit_once("\n#[cfg(test)]")
            .expect("server source must retain its test-module boundary")
            .0;
        let main_source = include_str!("main.rs");

        assert!(production_source.contains("omnisolo-builtin-agent"));
        assert!(!production_source.contains(concat!("oh", "c-builtin-agent")));
        assert!(main_source.contains("omnisolo-builtin-agent"));
        assert!(!main_source.contains(concat!("oh", "c-builtin-agent")));
    }

    #[test]
    fn production_router_inserts_the_shared_database_extension_without_double_wrapping() {
        let production_source = include_str!("server_builder/http_routes.rs");
        assert!(
            !production_source
                .contains(".layer(axum::extract::Extension(std::sync::Arc::new(db.clone())))"),
            "db is already Arc<DB>; double wrapping inserts Arc<Arc<DB>> and breaks handlers"
        );
        assert!(
            production_source.contains("legacy_db_compatibility_layer(db.clone())")
                || production_source.contains(".layer(axum::extract::Extension(db.clone()))")
        );
    }

    #[test]
    fn dashboard_sales_and_top_product_use_qualifying_orders() {
        for sql in [dashboard_sales_query(true), dashboard_sales_query(false)] {
            assert!(sql.contains("paid"));
            assert!(sql.contains("completed"));
        }
        for sql in [
            dashboard_top_product_query(true),
            dashboard_top_product_query(false),
        ] {
            assert!(sql.contains("paid"));
            assert!(sql.contains("completed"));
            assert!(sql.contains("SUM(oi.quantity)"));
            assert!(sql.contains("oi.tenant_id"));
        }
        assert_eq!(dashboard_top_product_query(false).matches('?').count(), 3);
    }

    #[tokio::test]
    async fn dashboard_metrics_use_real_tenant_scoped_order_data() {
        let sqlite = crate::db::create_sqlite_pool_for_test().await;
        for statement in [
            "CREATE TABLE orders (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, total_amount REAL NOT NULL, status TEXT NOT NULL)",
            "CREATE TABLE products (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, title TEXT NOT NULL)",
            "CREATE TABLE order_items (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, order_id TEXT NOT NULL, product_id TEXT NOT NULL, quantity INTEGER NOT NULL)",
            "INSERT INTO products VALUES ('product-a', 'tenant-a', 'Starter Kit')",
            "INSERT INTO products VALUES ('product-b', 'tenant-a', 'Premium Kit')",
            "INSERT INTO products VALUES ('product-other', 'tenant-b', 'Other Tenant Kit')",
            "INSERT INTO orders VALUES ('paid-a', 'tenant-a', 10.0, 'PAID')",
            "INSERT INTO orders VALUES ('completed-a', 'tenant-a', 20.0, 'completed')",
            "INSERT INTO orders VALUES ('pending-a', 'tenant-a', 30.0, 'pending')",
            "INSERT INTO orders VALUES ('cancelled-a', 'tenant-a', 1000.0, 'cancelled')",
            "INSERT INTO orders VALUES ('paid-other', 'tenant-b', 5000.0, 'paid')",
            "INSERT INTO order_items VALUES ('item-paid', 'tenant-a', 'paid-a', 'product-a', 2)",
            "INSERT INTO order_items VALUES ('item-completed', 'tenant-a', 'completed-a', 'product-b', 5)",
            "INSERT INTO order_items VALUES ('item-cancelled', 'tenant-a', 'cancelled-a', 'product-a', 100)",
            "INSERT INTO order_items VALUES ('item-other', 'tenant-b', 'paid-other', 'product-other', 500)",
        ] {
            sqlx::query(statement).execute(&sqlite).await.unwrap();
        }

        let total_sales = sqlx::query_scalar::<_, f64>(dashboard_sales_query(false))
            .bind("tenant-a")
            .fetch_one(&sqlite)
            .await
            .unwrap();
        let top_product = sqlx::query_scalar::<_, String>(dashboard_top_product_query(false))
            .bind("tenant-a")
            .bind("tenant-a")
            .bind("tenant-a")
            .fetch_optional(&sqlite)
            .await
            .unwrap();

        assert_eq!(total_sales, 30.0);
        assert_eq!(top_product.as_deref(), Some("Premium Kit"));
    }

    #[test]
    fn help_chat_links_only_to_current_next_routes() {
        for (legacy, expected) in [
            (
                "/help_article.html?id=getting-started-1",
                "/help?article=getting-started-1",
            ),
            ("/api-docs.html", "/api-docs"),
            ("/inbox.html", "/inbox"),
        ] {
            assert_eq!(current_help_link(legacy), expected);
        }
        for link in default_help_chat_links() {
            assert!(
                !link.contains(".html"),
                "legacy browser link remains: {link}"
            );
        }
    }

    #[test]
    fn agent_rpc_gateway_confines_methods_and_destination() {
        assert!(allowed_agent_rpc_method("run_agent"));
        assert!(allowed_agent_rpc_method("ap_list_tasks"));
        assert!(allowed_agent_rpc_method("aider_repomap"));
        assert!(!allowed_agent_rpc_method("admin_delete_everything"));
        assert!(agent_rpc_available(false));
        assert!(!agent_rpc_available(true));
        assert_eq!(
            agent_rpc_url("http://127.0.0.1:18789").unwrap().as_str(),
            "http://127.0.0.1:18789/rpc",
        );
        assert!(agent_rpc_url("file:///etc/passwd").is_err());
        assert!(agent_rpc_url("http://user:pass@127.0.0.1:18789").is_err());
        assert!(agent_rpc_url("http://127.0.0.1:18789?target=evil").is_err());

        let mut body = Vec::new();
        assert!(extend_agent_rpc_body(&mut body, b"1234", 4).is_ok());
        assert!(extend_agent_rpc_body(&mut body, b"5", 4).is_err());
    }

    #[tokio::test]
    async fn protected_http_routes_require_signed_bearer_identity() {
        use ::ohc_core::defaults::SingleTenantContext;
        use ::ohc_core::extensions::TenantContextProvider;
        use axum::{Router, body::Body, http::Request};
        use tower::ServiceExt;

        let auth_store = Arc::new(::server_auth::Store::new());
        let user = auth_store
            .create_user(
                "user-a".to_string(),
                "user-a@example.com".to_string(),
                "user-a-password".to_string(),
                vec!["ADMIN".to_string()],
                "tenant-a".to_string(),
            )
            .await
            .unwrap();
        let token = auth_store.issue_token(&user).unwrap();
        let tenant_ctx = SingleTenantContext::new("tenant-a").unwrap();
        let protected = Router::new()
            .route("/api/v1/protected", get(|| async { "ok" }))
            .route("/api/v1/auth/login", get(|| async { "public" }));
        let app = tenant_ctx.apply_http_guard(protected).route_layer(
            axum::middleware::from_fn_with_state(
                auth_store.clone(),
                protected_bearer_auth_middleware,
            ),
        );

        let forged = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/protected")
                    .header("x-tenant-id", "attacker")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forged.status(), axum::http::StatusCode::UNAUTHORIZED);

        let valid = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/protected?tenant_id=tenant-a")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(valid.status(), axum::http::StatusCode::OK);

        let public = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(public.status(), axum::http::StatusCode::OK);

        let docs = Router::new()
            .route(
                "/api/v1/help/{article_id}",
                get(crate::api::docs::get_article_handler),
            )
            .route("/api/v1/changelog", get(crate::api::docs::get_changelog))
            .route(
                "/api/v1/api-docs-spec",
                get(crate::api::docs::get_api_docs_spec),
            )
            .route_layer(axum::middleware::from_fn_with_state(
                auth_store,
                ::server_auth::strict_bearer_auth_middleware,
            ));
        for path in [
            "/api/v1/help/getting-started",
            "/api/v1/changelog",
            "/api/v1/api-docs-spec",
        ] {
            let response = docs
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                axum::http::StatusCode::UNAUTHORIZED,
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn test_agent_feed_route_paths() {
        use tower::ServiceExt;
        let pool = crate::db::create_dummy_pg_pool().await;
        let auth_store = Arc::new(crate::auth::Store::new());
        let app = axum::Router::new().merge(
            crate::api::agent_feed::router()
                .with_state(pool)
                .route_layer(axum::middleware::from_fn_with_state(
                    auth_store,
                    ::server_auth::strict_bearer_auth_middleware,
                )),
        );

        let res_no_slash = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/agent-feed")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res_no_slash.status(), axum::http::StatusCode::UNAUTHORIZED);

        let res_query_no_slash = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/v1/agent-feed?limit=5&offset=0")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            res_query_no_slash.status(),
            axum::http::StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn grpc_tls_config_requires_all_cloud_credentials() {
        assert_eq!(grpc_bind_host(true), "127.0.0.1");
        assert_eq!(grpc_bind_host(false), "0.0.0.0");
        assert!(
            grpc_tls_config_from_pem(true, None, None, None)
                .unwrap()
                .is_none()
        );

        let pem = Some(b"non-empty test PEM".to_vec());
        assert!(grpc_tls_config_from_pem(false, None, pem.clone(), pem.clone()).is_err());
        assert!(grpc_tls_config_from_pem(false, pem.clone(), None, pem.clone()).is_err());
        assert!(grpc_tls_config_from_pem(false, pem.clone(), pem.clone(), None).is_err());
        assert!(grpc_tls_config_from_pem(false, pem.clone(), pem.clone(), pem).is_ok());
    }

    #[test]
    fn standalone_peer_policy_injects_fixed_workspace_identity() {
        use ::ohc_core::defaults::LocalOwnerAuthPolicy;
        use ::ohc_core::extensions::AuthPolicyProvider;

        let unconfigured = LocalOwnerAuthPolicy::new("default_tenant").unwrap();
        assert_eq!(
            unconfigured
                .verify_grpc_peer(&tonic::Request::new(()))
                .unwrap_err()
                .code(),
            tonic::Code::Unauthenticated
        );

        let policy = unconfigured
            .with_hmac_token(
                "local-owner-test-token",
                b"local-owner-test-hmac-key-32byte",
            )
            .unwrap();
        let mut req = tonic::Request::new(());
        req.metadata_mut().insert(
            "authorization",
            "Bearer local-owner-test-token".parse().unwrap(),
        );
        let peer = policy.verify_grpc_peer(&req).unwrap();
        assert_eq!(peer.workspace_id.as_deref(), Some("default_tenant"));
        assert_eq!(peer.auth_scheme, "hmac-bearer");
    }

    #[tokio::test]
    async fn legacy_agent_rpc_route_denies_unauthorized_and_requires_tenant_bound_runtime() {
        use axum::{body::Body, http::Request, http::StatusCode};
        use tower::ServiceExt;

        let tenant_ctx = ::ohc_core::defaults::SingleTenantContext::new("tenant-a").unwrap();
        let auth = Arc::new(::server_auth::Store::new());
        let router = legacy_agent_rpc_router(auth, &tenant_ctx);
        let unauthed = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/rpc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthed.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn ui_inbox_tenant_requires_a_non_system_signed_claim() {
        let mut claims = ::server_common::Claims {
            sub: "user-a".to_string(),
            exp: i64::MAX,
            iat: 0,
            organization_id: Some("tenant-a".to_string()),
            username: "user-a".to_string(),
            email: "user-a@example.com".to_string(),
            roles: vec![],
            session_id: None,
            jti: "ui-inbox-tenant-test".to_string(),
        };
        assert_eq!(strict_ui_claim_tenant(&claims).as_deref(), Some("tenant-a"));
        claims.organization_id = None;
        assert_eq!(strict_ui_claim_tenant(&claims), None);
        claims.organization_id = Some("system".to_string());
        assert_eq!(strict_ui_claim_tenant(&claims), None);
        assert!(
            serde_json::from_value::<OmniInboxActionPayload>(serde_json::json!({
                "message_id": "message-a",
                "approved": true,
                "tenant_id": "attacker"
            }))
            .is_err()
        );
    }

    #[tokio::test]
    async fn sqlite_omni_inbox_is_scoped_by_tenant() {
        let sqlite = crate::db::create_sqlite_pool_for_test().await;
        sqlx::query(
            "CREATE TABLE omni_inbox_messages (
                id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, source TEXT,
                original_content TEXT, draft_reply TEXT, status TEXT NOT NULL,
                sender_id TEXT, customer_id TEXT,
                created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             )",
        )
        .execute(&sqlite)
        .await
        .unwrap();
        sqlx::query("INSERT INTO omni_inbox_messages (id, tenant_id, source, original_content, status) VALUES ('m1', 'tenant-a', 'sms', 'visible', 'unread'), ('m2', 'tenant-b', 'sms', 'hidden', 'unread')")
            .execute(&sqlite)
            .await
            .unwrap();
        let db = crate::db::DB::from_sqlite_pool(sqlite).unwrap();
        let visible = load_ui_omni_inbox_from_db(&db, "tenant-a", false)
            .await
            .unwrap();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0]["id"], "m1");
    }

    #[tokio::test]
    async fn test_voice_settings_logic() {
        let store = Arc::new(Store::new());
        store
            .set_voice_settings(
                true,
                Some("+15551112222".to_string()),
                Some("Professional".to_string()),
                Some("Help out".to_string()),
            )
            .unwrap();

        let current = store.get();
        assert!(current.voice_receptionist_enabled);
        assert_eq!(
            current.voice_receptionist_number,
            Some("+15551112222".to_string())
        );
        assert_eq!(
            current.voice_receptionist_persona,
            Some("Professional".to_string())
        );
        assert_eq!(
            current.voice_receptionist_instructions,
            Some("Help out".to_string())
        );

        store.set_voice_settings(true, None, None, None).unwrap();
        let updated = store.get();
        assert!(updated.voice_receptionist_enabled);
        assert_eq!(updated.voice_receptionist_number, None);
        assert_eq!(updated.voice_receptionist_persona, None);
        assert_eq!(updated.voice_receptionist_instructions, None);
    }
}

#[cfg(test)]
mod health_test;
