// SPDX-License-Identifier: Apache-2.0
//! Composable HTTP router assembly separating OSS core endpoints from injected extensions.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::Extension,
    http::StatusCode,
    routing::{delete, get, post},
};

use super::builder::ServerContext;
use super::capabilities::capabilities_router;
use super::http_gateway_helpers::{
    AGENT_RPC_REQUEST_LIMIT_BYTES, api_not_found_handler, cancel_workflow_handler,
    create_workflow_handler, help_chat_handler, legacy_agent_rpc_router,
    legacy_db_compatibility_layer, list_workflows_handler, protected_bearer_auth_middleware,
    proxy_agent_rpc_handler, workflow_receipt_handler, workflow_request_receipt_handler,
};
use super::metrics_handlers::{
    DraftReplyRequest, HttpMetricsRequest, advisory_insights_handler, draft_reply_handler,
    http_metrics_handler,
};

pub type ExtraRouterFn = Arc<dyn Fn(&ServerContext) -> Router + Send + Sync>;
pub type RouterLayerFn = Arc<dyn Fn(&ServerContext, Router) -> Router + Send + Sync>;

use super::http_subrouters::{
    build_ui_protected_router, build_webhook_subrouters, readiness_handler, voice_incoming_handler,
};

/// Builds the full HTTP/WebSocket Axum router using injected OSS/Cloud extensions.
pub async fn build_http_router(
    ctx: &ServerContext,
    extra_protected_http_routes: Option<&ExtraRouterFn>,
    extra_tier_layer: Option<&RouterLayerFn>,
    extra_public_http_routes: Option<&ExtraRouterFn>,
) -> Result<Router, Box<dyn std::error::Error>> {
    let db = &ctx.db;
    let hub = &ctx.hub;
    let http_auth_store = &ctx.auth_store;
    let mesh_transport = &ctx.mesh_transport;
    let dept_orchestrator = &ctx.dept_orchestrator;
    let settings_store = crate::settings::Store::global();

    let sms_service = crate::api::sms_settings::SmsService::configured(http_auth_store.clone());
    crate::api::sms_settings::install_global(sms_service.clone())?;
    let _order_sms_worker = sms_service.start_order_notifications();

    let timecard_access =
        crate::api::staff_timecards::TimecardAccess::configured(db, http_auth_store.clone()).await;
    let shipping_access = Arc::new(
        crate::api::shipping::authority::ShippingAccess::configured(db, http_auth_store.clone())
            .await,
    );
    let field_ops_pool =
        crate::api::field_ops::canonical_pool(http_auth_store, db.postgres_pool()).await;
    let sync_events_state = crate::api::offline_sync::SyncEventsState::new(
        db.pool.clone(),
        field_ops_pool.clone(),
        http_auth_store.clone(),
    )
    .await;
    let sync_write_state =
        crate::api::offline_sync::SyncWriteState::new(http_auth_store.clone(), db.postgres_pool())
            .await;

    let voice_engine = Arc::new(crate::voice::VoiceAIEdgeEngine::new());
    let twilio_client = Arc::new(::server_integrations_twilio::provider::TwilioProvider::new(
        std::env::var("TWILIO_ACCOUNT_SID").unwrap_or_default(),
        std::env::var("TWILIO_AUTH_TOKEN").unwrap_or_default(),
    ));
    let voice_router = Arc::new(crate::voice::VoiceContextRouter::new(
        voice_engine.clone(),
        twilio_client,
    ));

    let bearer_layer = axum::middleware::from_fn_with_state(
        http_auth_store.clone(),
        ::server_auth::strict_bearer_auth_middleware,
    );

    let builder_pool = match ctx.portable_database.as_ref() {
        Some(database) => {
            super::builder::canonical_builder_pool(database, db.postgres_pool()).await
        }
        None => None,
    };

    let mut protected_core = Router::new()
        .nest(
            "/api/v1/field-ops",
            crate::api::field_ops::configured_router(
                db.pool.clone(),
                field_ops_pool.clone(),
                mesh_transport.clone(),
                http_auth_store.clone(),
            ),
        )
        .merge(crate::api::sms_settings::router(sms_service).route_layer(bearer_layer.clone()))
        .merge(
            crate::api::workspace_views::delivery_settings_router_with_policy(
                settings_store.clone(),
                ctx.edition.multi_tenant,
            )
            .route_layer(bearer_layer.clone()),
        )
        .merge(crate::api::telemetry_settings::router(settings_store.clone()))
        .merge(crate::api::voice_provisioning::router(settings_store.clone()))
        .route(
            "/api/v1/voice/incoming",
            post({
                let settings_store = settings_store.clone();
                let db = db.clone();
                let voice_engine = voice_engine.clone();
                let voice_router = voice_router.clone();
                move |claims, body| {
                    voice_incoming_handler(
                        settings_store.clone(),
                        db.clone(),
                        voice_engine.clone(),
                        voice_router.clone(),
                        claims,
                        body,
                    )
                }
            }),
        )
        .route(
            "/api/v1/checkout/mercadopago",
            post(|| async {
                (
                    StatusCode::NOT_IMPLEMENTED,
                    Json(serde_json::json!({
                        "success": false,
                        "code": "provider_connection_required",
                        "message": "A verified Mercado Pago checkout connection is required; no payment session was created."
                    })),
                )
            }),
        )
        .route(
            "/api/v1/checkout/delivery-quote",
            post({
                let settings_store = settings_store.clone();
                move |Json(_req): Json<serde_json::Value>| {
                    let settings = settings_store.get();
                    async move {
                        if !settings.delivery_enabled {
                            return Json(serde_json::json!({ "success": false, "message": "Delivery is not enabled." }));
                        }
                        let fee = settings.delivery_fee.unwrap_or(8.50);
                        Json(serde_json::json!({
                            "success": true,
                            "fee": fee,
                            "dropoff_eta": (chrono::Utc::now() + chrono::Duration::minutes(45)).to_rfc3339(),
                            "pickup_eta": (chrono::Utc::now() + chrono::Duration::minutes(15)).to_rfc3339()
                        }))
                    }
                }
            }),
        )
        .nest("/api/v1/integrations", crate::api::tool_integrations::router(db.clone()))
        .route(
            "/api/v1/walkup",
            post(crate::api::walkup::handle_walkup)
                .with_state(crate::api::walkup::AppState { db: db.clone() })
                .route_layer(bearer_layer.clone()),
        )
        .merge(build_ui_protected_router(
            db.clone(),
            hub.clone(),
            http_auth_store.clone(),
        ))
        .route(
            "/api/v1/inbox/messages",
            get(crate::api::workspace_views::get_inbox_messages_handler)
                .with_state(db.clone())
                .route_layer(bearer_layer.clone()),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route("/health", get(|| async { "ok" }))
        .route(
            "/readyz",
            get({
                let database = ctx.portable_database.clone();
                move || readiness_handler(database.clone())
            }),
        )
        .route("/api/v1/dashboard", get(|| async { crate::api::production_readiness::unavailable("organization_dashboard") }))
        .route(
            "/api/v1/meetings",
            get(crate::api::workspace_views::get_meetings_http_handler)
                .with_state(hub.clone())
                .route_layer(bearer_layer.clone()),
        )
        .route("/api/v1/costs", get(|| async { crate::api::production_readiness::unavailable("cost_summary") }))
        .route("/api/v1/approvals/request", post(|| async { crate::api::production_readiness::unavailable("approval_request") }))
        .route("/api/v1/approvals/decide", axum::routing::put(|| async { crate::api::production_readiness::unavailable("approval_decision") }))
        .route("/api/v1/handoffs", post(|| async { crate::api::production_readiness::unavailable("handoff_creation") }))
        .route("/api/v1/skills/import", post(|| async { crate::api::production_readiness::unavailable("skill_import") }))
        .route("/api/v1/snapshots/create", post(|| async { crate::api::production_readiness::unavailable("snapshot_creation") }))
        .route(
            "/api/v1/ai/draft-reply",
            post({
                let db = db.clone();
                let store = http_auth_store.clone();
                move |headers, Json(payload): Json<DraftReplyRequest>| {
                    draft_reply_handler(db.clone(), store.clone(), headers, payload)
                }
            }),
        )
        .route(
            "/api/v1/dashboard/metrics",
            post({
                let db = db.clone();
                let store = http_auth_store.clone();
                move |headers, Json(payload): Json<HttpMetricsRequest>| {
                    http_metrics_handler(db.clone(), store.clone(), headers, Json(payload))
                }
            }),
        )
        .route(
            "/api/v1/sync/events",
            post({
                let state = sync_events_state.clone();
                move |headers, payload| {
                    crate::api::offline_sync::sync_events_handler(
                        axum::extract::State(state.clone()),
                        headers,
                        payload,
                    )
                }
            }),
        )
        .route(
            "/api/v1/sync/offline",
            post({
                let state = sync_write_state.clone();
                let mesh = mesh_transport.clone();
                move |headers, payload| {
                    crate::api::offline_sync::offline_sync_handler(
                        axum::extract::State((state.clone(), mesh.clone())),
                        headers,
                        payload,
                    )
                }
            }),
        )
        .route(
            "/api/v1/sync/operation-intents",
            post({
                let state = sync_write_state.clone();
                move |headers, payload| {
                    crate::api::offline_sync::operation_intents_handler(
                        axum::extract::State(state.clone()),
                        headers,
                        payload,
                    )
                }
            }),
        )
        .route(
            "/api/v1/advisory/insights",
            get({
                let db = db.clone();
                let store = http_auth_store.clone();
                move |headers| advisory_insights_handler(db.clone(), store.clone(), headers)
            }),
        )
        .nest("/api/v1/autodream", crate::api::autodream::router(ctx.autodream_worker.clone()))
        .nest(
            "/api/v1/dynamic-workflows",
            crate::api::dynamic_workflows::router(
                ctx.dynamic_workflow_manager.clone(),
                http_auth_store.clone(),
            ),
        )
        .nest(
            "/api/v1/assistant",
            crate::api::assistant::router(db.clone())
                .layer(Extension(ctx.workflow_execution.clone())),
        )
        .nest(
            "/api/v1/task-center",
            crate::api::task_center::router(
                db.clone(),
                Arc::new(crate::api::task_center::executor::TaskCenterExecutor::from_environment(db.clone())),
            )
            .route_layer(bearer_layer.clone()),
        )
        .nest(
            "/api/v1/subscriptions",
            crate::api::subscription::router_with_orchestrator(
                hub.clone(),
                Some(dept_orchestrator.clone()),
            ),
        )
        .nest(
            "/api/v1/fulfillment",
            crate::api::fulfillment::router(shipping_access.clone())
                .merge(crate::api::shipping::router(shipping_access.clone()))
                .route_layer(bearer_layer.clone()),
        )
        .nest(
            "/api/v1/staff",
            crate::api::staff_mesh::router(db.clone()).layer(Extension(timecard_access)),
        )
        .nest(
            "/api/v1/builder",
            crate::builder::api::router(builder_pool).layer(Extension(Arc::new(
                crate::builder::generation::GenerationContext::from_environment(
                    ctx.workflow_execution.clone(),
                ),
            ))),
        )
        .route(
            "/api/v1/agents/workflows",
            get(list_workflows_handler)
                .post(create_workflow_handler)
                .layer(Extension(ctx.workflow_execution.clone())),
        )
        .route(
            "/api/v1/agents/workflows/{id}",
            get(workflow_receipt_handler).layer(Extension(ctx.workflow_execution.clone())),
        )
        .route(
            "/api/v1/agents/workflows/by-request/{id}",
            get(workflow_request_receipt_handler)
                .layer(Extension(ctx.workflow_execution.clone())),
        )
        .route(
            "/api/v1/agents/workflows/{id}/cancel",
            post(cancel_workflow_handler).layer(Extension(ctx.workflow_execution.clone())),
        )
        .nest(
            "/api/v1/agents",
            crate::api::agents::hire::router(hub.clone())
                .layer(Extension(ctx.workflow_execution.clone())),
        )
        .merge(crate::api::agents::definitions::router(
            match &db.store {
                crate::db::DbStore::Postgres => {
                    crate::persistence::agent_definitions::DefinitionStore::Database(
                        crate::persistence::AppDatabase::from_connection(
                            sea_orm::SqlxPostgresConnector::from_sqlx_postgres_pool(
                                db.pool.clone(),
                            ),
                        ),
                    )
                }
                crate::db::DbStore::Sqlite(pool) => {
                    crate::persistence::agent_definitions::DefinitionStore::Database(
                        crate::persistence::AppDatabase::from_connection(
                            sea_orm::SqlxSqliteConnector::from_sqlx_sqlite_pool(pool.clone()),
                        ),
                    )
                }
            },
            http_auth_store.clone(),
        ))
        .route(
            "/api/v1/gateway/run",
            post(crate::api::onboarding::gateway_run_handler)
                .layer(axum::middleware::from_fn(::server_auth::api_key_auth_middleware))
                .with_state(Arc::new(
                    crate::services::onboarding::onboarding_agent::OnboardingAgent::new(
                        db.clone(),
                        hub.clone(),
                    ),
                )),
        )
        .nest(
            "/api/v1/onboarding",
            crate::api::onboarding::router(
                Arc::new(
                    crate::services::onboarding::onboarding_agent::OnboardingAgent::new(
                        db.clone(),
                        hub.clone(),
                    ),
                ),
                http_auth_store.clone(),
            )
            .with_state(mesh_transport.clone()),
        )
        .nest(
            "/api/v1/growth",
            crate::api::growth::router(
                db.pool.clone(),
                hub.clone(),
                Arc::new(crate::services::growth::viral_loop::ViralLoopTracker::new()),
            ),
        )
        .nest(
            "/api/v1/catalog",
            crate::api::catalog::router(hub.clone(), ctx.catalog_repository.clone()).route_layer(
                axum::middleware::from_fn_with_state(
                    http_auth_store.clone(),
                    ::server_auth::strict_bearer_auth_middleware,
                ),
            ),
        )
        .nest(
            "/api/v1/shipping",
            crate::api::shipping::router(shipping_access).route_layer(bearer_layer.clone()),
        )
        .nest(
            "/api/v1/checkout",
            crate::api::checkout_api::router(hub.clone()).with_state(mesh_transport.clone()),
        )
        .nest(
            "/api/v1/payments/terminal",
            crate::api::terminal_api::router(hub.clone()).with_state(mesh_transport.clone()),
        )
        .nest(
            "/api/v1/payments/ledger",
            crate::api::payment_ledger::router().with_state(crate::api::payment_ledger::AppState {
                db: db.clone(),
                hub: hub.clone(),
            }),
        )
        .nest(
            "/api/v1/pos",
            crate::api::pos::pos_routes(hub.clone()).route_layer(bearer_layer.clone()),
        )
        .nest(
            "/api/v1/cart",
            crate::api::cart::router(hub.clone()).with_state(mesh_transport.clone()),
        )
        .nest(
            "/api/v1/storefront",
            crate::api::storefront_delivery::router(db.pool.clone(), http_auth_store.clone()),
        )
        .route(
            "/api/v1/voice/command",
            post(crate::api::audio_command::handle_voice_command).with_state(
                crate::api::audio_command::VoiceCommandState {
                    orchestrator: dept_orchestrator.clone(),
                    semantic_router: ctx.semantic_router.clone(),
                },
            ),
        )
        .nest("/api/v1/agents/approvals", crate::api::agents::approvals::router(dept_orchestrator.clone()))
        .nest("/api/v1/agents/settings", crate::api::agents::settings::router(dept_orchestrator.clone()))
        .nest(
            "/api/v1/agents/chat",
            crate::api::agents::chat::router(
                dept_orchestrator.clone(),
                ctx.semantic_router.clone(),
            ),
        )
        .route(
            "/api/v1/agents/order-interceptor",
            post(crate::api::agents::order_interceptor::intercept_order_handler)
                .with_state(db.pool.clone()),
        )
        .nest("/api/v1/agents/pydantic", crate::api::agents::pydantic::router())
        .nest("/api/v1/agents/webhook", crate::api::agents::webhook::router(dept_orchestrator.clone()))
        .nest("/api/v1/perplexity", crate::api::perplexity::router())
        .route(
            "/api/v1/settings/integrations/whatsapp_cloud_api",
            post(crate::api::settings::integrations::whatsapp::connect_whatsapp_cloud_api)
                .with_state(hub.clone()),
        )
        .route(
            "/api/v1/settings/integrations/whatsapp",
            post(crate::api::settings::integrations::whatsapp::connect_whatsapp_twilio)
                .with_state(hub.clone()),
        )
        .merge(crate::api::agent_stream::router(hub.clone()))
        .merge(crate::api::agent_metrics::router(hub.clone()))
        .merge(crate::api::agent_orchestrate::router(hub.clone()))
        .route("/api/v1/feed/ws", get(crate::api::agent_feed::ws_feed_handler))
        .route("/ws", get(crate::api::unified_ws::unified_ws_handler))
        .route(
            "/api/v1/auth/realtime-ticket",
            post(crate::api::realtime::realtime_ticket_handler).route_layer(bearer_layer.clone()),
        )
        .merge(crate::api::realtime::router())
        .merge(
            crate::api::agent_feed::router()
                .with_state(db.pool.clone())
                .layer(Extension(http_auth_store.clone()))
                .route_layer(bearer_layer.clone()),
        )
        .nest(
            "/api/v1/ohc_job_queue",
            crate::api::omnisolo_job_queue::handler::router()
                .with_state(mesh_transport.clone())
                .layer(legacy_db_compatibility_layer(db.clone())),
        )
        .nest(
            "/api/v1/ohc-job-queue",
            crate::api::omnisolo_job_queue::handler::router()
                .with_state(mesh_transport.clone())
                .layer(legacy_db_compatibility_layer(db.clone())),
        )
        .nest("/api/v1/incidents", crate::api::incidents::router().with_state(db.pool.clone()))
        .nest("/api/v1/invoices", crate::api::invoice::router(hub.clone()))
        .nest("/api/v1/inquiries", crate::api::inquiries::router().with_state(db.pool.clone()))
        .nest("/api/v1/quotes", crate::api::quotes::router().with_state(db.pool.clone()))
        .nest(
            "/api/v1/field-service-routing",
            crate::api::field_service_routing::router(
                db.clone(),
                hub.clone(),
                http_auth_store.clone(),
                field_ops_pool,
            ),
        )
        .nest(
            "/api/v1/work-intake/submit",
            crate::api::agents::client_intake::router(dept_orchestrator.clone()),
        )
        .nest(
            "/api/v1/proposals",
            crate::api::proposals::router()
                .with_state(db.pool.clone())
                .route_layer(bearer_layer.clone()),
        )
        .nest(
            "/api/v1/booking/request",
            crate::api::booking::request::router(dept_orchestrator.clone(), db.pool.clone())
                .route_layer(bearer_layer.clone()),
        )
        .nest(
            "/api/v1/booking/reserve",
            crate::api::booking::reserve::router(db.clone()).route_layer(bearer_layer.clone()),
        )
        .nest(
            "/api/v1/booking/available_slots",
            crate::api::booking::available_slots::router(db.clone())
                .route_layer(bearer_layer.clone()),
        )
        .nest(
            "/api/v1/booking/services",
            crate::api::booking::create_service::router(db.clone())
                .route_layer(bearer_layer.clone()),
        )
        .nest(
            "/api/v1/booking/proposed",
            crate::api::booking::proposed::router().route_layer(bearer_layer.clone()),
        )
        .nest(
            "/api/v1/agents/mission",
            crate::api::agents::mission::handoff::router(Arc::new(crate::sip::SipDB::new(
                db.pool.clone(),
                "default".to_string(),
            ))),
        )
        .route(
            "/api/v1/rpc",
            post(proxy_agent_rpc_handler)
                .layer(axum::extract::DefaultBodyLimit::max(AGENT_RPC_REQUEST_LIMIT_BYTES))
                .with_state(mesh_transport.clone()),
        )
        .route(
            "/api/v1/usage",
            get(crate::api::usage_api::overview_summary).with_state(hub.clone()),
        )
        .nest(
            "/api/v1/billing",
            crate::api::usage_api::router()
                .with_state(hub.clone())
                .merge(crate::api::merchant_checkout::router(
                    crate::api::merchant_checkout::MerchantCheckoutState {
                        db: db.clone(),
                        tenant_ctx: ctx.tenant_ctx.clone(),
                        billing_platform: ctx.billing_platform.clone(),
                        sync_transport: ctx.sync_transport.clone(),
                    },
                )),
        );

    if let Some(extra_fn) = extra_protected_http_routes {
        protected_core = protected_core.merge(extra_fn(ctx));
    }
    if let Some(billing_routes) = ctx.billing_platform.platform_billing_routes() {
        protected_core = protected_core.merge(billing_routes);
    }
    if let Some(sync_routes) = ctx.sync_transport.extra_sync_routes() {
        protected_core = protected_core.merge(sync_routes);
    }

    protected_core = ctx.tenant_ctx.apply_http_guard(protected_core);
    if let Some(tier_fn) = extra_tier_layer {
        protected_core = tier_fn(ctx, protected_core);
    }
    let protected_core = protected_core.route_layer(axum::middleware::from_fn_with_state(
        http_auth_store.clone(),
        protected_bearer_auth_middleware,
    ));

    let health_router = Router::new()
        .route("/api/v1/health", get(crate::api::health::health_handler))
        .with_state(hub.clone());
    let setup_router: Router =
        Router::new().nest("/api/v1/setup", crate::setup::router(db.clone()));
    let widget_router = Router::new().nest(
        "/api/widget",
        crate::api::widget::router(db.clone(), http_auth_store.clone()),
    );
    let oauth_callback_router: Router = Router::new()
        .nest("/api/v1/oauth", crate::api::oauth::proxy::router())
        .with_state(mesh_transport.clone());
    let http_auth_router = crate::auth::http::router_with_auth_policy(
        http_auth_store.clone(),
        ctx.auth_policy.clone(),
    )
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;

    let mut app = protected_core
        .nest(
            "/api/v1/fulfillment",
            crate::api::fulfillment::webhook_router(db.pool.clone()),
        )
        .merge(crate::builder::publication_http::router(
            db.pool.clone(),
            http_auth_store.clone(),
        ))
        .nest(
            "/api/v1/local_seo",
            crate::api::local_seo::router()
                .layer(legacy_db_compatibility_layer(db.clone()))
                .route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/ledger/accounts",
            get(crate::api::payment_ledger::get_accounts).route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/ledger/entries",
            get(crate::api::payment_ledger::get_entries).route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/ledger/record",
            post(crate::api::payment_ledger::record_ledger_entry).route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/user/usage",
            get(crate::api::payment_ledger::get_user_usage).route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/settings",
            get(crate::api::settings::global_commerce::get_settings)
                .put(crate::api::settings::global_commerce::update_settings)
                .layer(legacy_db_compatibility_layer(db.clone()))
                .route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/settings/global-commerce",
            get(crate::api::settings::global_commerce::get_settings)
                .put(crate::api::settings::global_commerce::update_settings)
                .layer(legacy_db_compatibility_layer(db.clone()))
                .route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/help",
            get(crate::api::docs::list_articles).layer(legacy_db_compatibility_layer(db.clone())),
        )
        .route(
            "/api/v1/help/search",
            get(crate::api::docs::search_articles).layer(legacy_db_compatibility_layer(db.clone())),
        )
        .route(
            "/api/v1/help/{article_id}",
            get(crate::api::docs::get_article_handler).route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/tooltips",
            get(crate::api::docs::get_tooltips)
                .layer(legacy_db_compatibility_layer(db.clone()))
                .route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/tooltips",
            post(crate::api::docs::update_tooltip)
                .layer(legacy_db_compatibility_layer(db.clone()))
                .route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/tooltips/{id}",
            delete(crate::api::docs::delete_tooltip)
                .layer(legacy_db_compatibility_layer(db.clone()))
                .route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/walkthrough/{page}",
            get(crate::api::docs::get_walkthrough)
                .layer(legacy_db_compatibility_layer(db.clone()))
                .route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/videos",
            get(crate::api::docs::list_videos)
                .layer(legacy_db_compatibility_layer(db.clone()))
                .route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/changelog",
            get(crate::api::docs::get_changelog).route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/api-docs-spec",
            get(crate::api::docs::get_api_docs_spec).route_layer(bearer_layer.clone()),
        )
        .route(
            "/api/v1/chat",
            post({
                let db = db.clone();
                move |claims, payload| help_chat_handler(db.clone(), claims, payload)
            })
            .route_layer(bearer_layer.clone()),
        )
        .merge(build_webhook_subrouters(ctx))
        .merge(
            omnisolo_builtin_agent::visual_workflow_client::create_router(Arc::new(
                omnisolo_builtin_agent::visual_workflow_client::VisualWorkflowState {
                    default_agent: Arc::new(omnisolo_builtin_agent::agent::Agent::new(
                        Arc::new(omnisolo_builtin_agent::llm::ollama::OllamaClient::new(
                            "http://localhost:11434",
                        )),
                        vec![],
                    )),
                    tools: vec![],
                    sub_agents: std::collections::HashMap::new(),
                    default_config: omnisolo_builtin_agent::agent::AgentRunConfig::default(),
                },
            ))
            .route_layer(bearer_layer.clone()),
        )
        .merge(legacy_agent_rpc_router(
            http_auth_store.clone(),
            ctx.tenant_ctx.as_ref(),
        ))
        .merge(crate::api::search::router(
            db.pool.clone(),
            http_auth_store.clone(),
        ))
        .nest(
            "/api/v1/memory",
            crate::api::inbox::customer_memory::router(db.clone(), http_auth_store.clone()),
        )
        .nest(
            "/api/v1/inbox/action_required",
            crate::api::inbox::action_required::router(db.clone(), http_auth_store.clone()),
        )
        .merge(health_router)
        .merge(capabilities_router(
            ctx.deployment.clone(),
            ctx.auth_policy.clone(),
        ))
        .merge(http_auth_router)
        .merge(setup_router)
        .merge(widget_router)
        .merge(oauth_callback_router);

    let deploy_routes = ctx.deployment.mount_extra_http_routes();
    app = app.merge(deploy_routes);
    if let Some(public_fn) = extra_public_http_routes {
        app = app.merge(public_fn(ctx));
    }

    Ok(app.fallback(api_not_found_handler))
}
