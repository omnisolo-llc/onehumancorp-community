// SPDX-License-Identifier: Apache-2.0
//! Extracted UI, readiness, voice, and webhook subrouter builders.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::Extension,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};

use super::builder::ServerContext;
use super::http_gateway_helpers::protect_internal_ingress;
use super::ui_ops_handlers::{
    create_ui_bom_item_handler, create_ui_raw_material_handler, create_ui_supply_vendor_handler,
    list_ui_bookings_handler, list_ui_inbox_handler, list_ui_priority_tasks_handler,
    list_ui_supply_handler, ui_dashboard_metrics_handler,
};
use super::ui_triage_handlers::{
    create_ui_triage_item_handler, list_ui_omni_inbox_handler, list_ui_triage_handler,
    read_ui_omni_inbox_action_handler, ui_dashboard_analytics_briefing_handler,
    ui_dashboard_analytics_chat_handler, update_ui_omni_inbox_action_handler,
};
use super::ui_unified_feed_handlers::{
    ui_dashboard_unified_agent_feed_handler, ui_dashboard_unified_feed_handler,
};

pub(crate) async fn voice_incoming_handler(
    settings_store: Arc<crate::settings::Store>,
    db: Arc<crate::db::DB>,
    voice_engine: Arc<crate::voice::VoiceAIEdgeEngine>,
    voice_router: Arc<crate::voice::VoiceContextRouter>,
    claims: Option<Extension<::server_common::Claims>>,
    Json(req): Json<serde_json::Value>,
) -> Response {
    let caller_phone = req
        .get("caller_phone")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let user_text = req
        .get("user_text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let to_phone = req
        .get("To")
        .or_else(|| req.get("to"))
        .or_else(|| req.get("merchant_phone"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let claim_tenant =
        claims.and_then(|Extension(c)| ::server_common::auth_utils::signed_tenant_id(&c));
    let mapped_tenant: Option<String> = match to_phone {
        Some(phone) => match &db.store {
            crate::db::DbStore::Postgres => sqlx::query_scalar::<_, String>(
                "SELECT tenant_id FROM settings WHERE voice_receptionist_number = $1 LIMIT 1",
            )
            .bind(phone)
            .fetch_optional(&db.pool)
            .await
            .ok()
            .flatten(),
            crate::db::DbStore::Sqlite(pool) => sqlx::query_scalar::<_, String>(
                "SELECT tenant_id FROM settings WHERE voice_receptionist_number = ? LIMIT 1",
            )
            .bind(phone)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten(),
        }
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty() && !t.eq_ignore_ascii_case("system")),
        None => None,
    };
    let tenant_id = match (claim_tenant, mapped_tenant) {
        (Some(ct), Some(mt)) if ct != mt => {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "tenant_mismatch" })),
            )
                .into_response();
        }
        (Some(ct), _) => ct,
        (None, Some(mt)) => mt,
        (None, None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "unmapped_voice_tenant" })),
            )
                .into_response();
        }
    };
    let settings = settings_store.get();
    if !settings.voice_receptionist_enabled {
        return Json(
            serde_json::json!({ "reply": "The voice receptionist is currently disabled." }),
        )
        .into_response();
    }
    let merchant_phone = to_phone
        .map(str::to_string)
        .or_else(|| settings.voice_receptionist_number.clone())
        .unwrap_or_default();
    let session_id = voice_engine
        .handle_incoming_call(&tenant_id, &caller_phone)
        .await;
    let reply = voice_router
        .process_user_input(&session_id, &user_text, &merchant_phone)
        .await;
    voice_engine.end_call(&session_id).await;
    Json(serde_json::json!({ "reply": reply })).into_response()
}

pub(crate) async fn readiness_handler(
    database: Option<Arc<crate::persistence::AppDatabase>>,
) -> Response {
    let Some(database) = database else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "not_ready",
                "database": "unavailable"
            })),
        )
            .into_response();
    };
    match crate::persistence::commands::verify(database.as_ref()).await {
        Ok(verification) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "ready",
                "database": verification
            })),
        )
            .into_response(),
        Err(error) => {
            tracing::error!("database readiness check failed: {error}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "status": "not_ready",
                    "database": "query_failed"
                })),
            )
                .into_response()
        }
    }
}

pub(crate) fn build_ui_protected_router(
    db: Arc<crate::db::DB>,
    hub: Arc<crate::hub::Hub>,
    http_auth_store: Arc<::server_auth::Store>,
) -> Router {
    Router::new()
        .route(
            "/api/v1/ui/dashboard/metrics",
            get(ui_dashboard_metrics_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/dashboard/daily-work",
            get(crate::api::work_triage::get_daily_work_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/dashboard/daily-work/action/{id}",
            post(crate::api::work_triage::approve_daily_work_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/dashboard/unified-feed",
            get(ui_dashboard_unified_feed_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/dashboard/unified-agent-feed",
            get(ui_dashboard_unified_agent_feed_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/dashboard/analytics/briefing",
            get(ui_dashboard_analytics_briefing_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/dashboard/analytics/chat",
            post(ui_dashboard_analytics_chat_handler).with_state(db.clone()),
        )
        .merge(crate::api::ui_orders::router(db.clone()))
        .route(
            "/api/v1/ui/inventory",
            get(crate::api::pos::get_inventory_handler)
                .post(crate::api::pos::post_inventory_handler)
                .with_state(hub),
        )
        .route(
            "/api/v1/ui/bookings",
            get(list_ui_bookings_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/inbox",
            get(list_ui_inbox_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/inbox/messages",
            get(list_ui_inbox_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/omni_inbox",
            get(list_ui_omni_inbox_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/omni_inbox/action",
            post(update_ui_omni_inbox_action_handler)
                .get(read_ui_omni_inbox_action_handler)
                .with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/triage",
            get(list_ui_triage_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/triage/pending",
            get(list_ui_triage_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/triage/decisions/{id}",
            get(crate::api::legacy_triage::read_decision)
                .with_state(db.clone())
                .layer(Extension(http_auth_store.clone())),
        )
        .route(
            "/api/v1/ui/triage/action",
            post(crate::api::legacy_triage::action)
                .with_state(db.clone())
                .layer(Extension(http_auth_store.clone())),
        )
        .route(
            "/api/v1/triage/action",
            post(crate::api::legacy_triage::action)
                .with_state(db.clone())
                .layer(Extension(http_auth_store.clone())),
        )
        .route(
            "/api/v1/ui/triage/create",
            post(create_ui_triage_item_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/triage/create",
            post(create_ui_triage_item_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/supply",
            get(list_ui_supply_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/priority-tasks",
            get(list_ui_priority_tasks_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/supply/vendors",
            post(create_ui_supply_vendor_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/supply/raw-materials",
            post(create_ui_raw_material_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/supply/bom-items",
            post(create_ui_bom_item_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/opportunities",
            get(crate::api::crm::list_opportunities_handler).with_state(db.clone()),
        )
        .route(
            "/api/v1/ui/opportunities/stage",
            post(crate::api::crm::update_opportunity_stage_handler).with_state(db),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            http_auth_store,
            ::server_auth::strict_bearer_auth_middleware,
        ))
}

pub(crate) fn build_webhook_subrouters(ctx: &ServerContext) -> Router {
    let db = &ctx.db;
    let hub = &ctx.hub;
    let dept_orchestrator = &ctx.dept_orchestrator;
    let http_auth_store = &ctx.auth_store;

    let relay_webhook_router = Router::new()
        .route(
            "/api/v1/relay/webhook/{agent_id}",
            post(crate::api::mcp_webhook::handle_relay_webhook),
        )
        .with_state(ctx.reverse_tunnel_server.clone());

    let webhook_state = crate::api::billing_webhook::WebhookState {
        tenant_ctx: ctx.tenant_ctx.clone(),
        billing_platform: ctx.billing_platform.clone(),
        sync_transport: ctx.sync_transport.clone(),
        db_pool: db.pool.clone(),
        db: db.clone(),
        orchestrator: dept_orchestrator.clone(),
    };
    let webhook_router = Router::new()
        .route(
            "/api/v1/webhooks/stripe",
            post(crate::api::billing_webhook::stripe_webhook_handler),
        )
        .route(
            "/api/v1/webhooks/mercadopago",
            post(crate::api::billing_webhook::mercadopago_webhook_handler),
        )
        .route(
            "/api/v1/webhooks/razorpay",
            post(crate::api::billing_webhook::razorpay_webhook_handler),
        )
        .route(
            "/api/v1/webhooks/calcom",
            post(crate::api::billing_webhook::calcom_webhook_handler),
        )
        .route(
            "/api/v1/webhooks/resend",
            post(crate::api::billing_webhook::resend_webhook_handler),
        )
        .route(
            "/api/v1/webhooks/calendly",
            post(crate::api::billing_webhook::calendly_webhook_handler),
        )
        .route(
            "/api/v1/webhooks/mailchimp",
            post(crate::api::billing_webhook::mailchimp_webhook_handler),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            webhook_state.clone(),
            crate::api::billing_webhook::webhook_security_middleware,
        ))
        .with_state(webhook_state);

    let meta_webhook_router = Router::new()
        .route(
            "/api/v1/webhooks/meta",
            get(crate::api::meta_webhook::meta_webhook_get_handler),
        )
        .route(
            "/api/v1/webhooks/meta",
            post(crate::api::meta_webhook::meta_webhook_post_handler),
        )
        .with_state(crate::api::meta_webhook::MetaWebhookState {
            hub: hub.clone(),
            db: db.clone(),
            orchestrator: dept_orchestrator.clone(),
        });

    let omnichannel_webhook_router = Router::new()
        .route(
            "/api/v1/omnichannel/webhook",
            post(crate::api::omnichannel_webhook::handle_omnichannel_webhook),
        )
        .route(
            "/api/v1/webhooks/omnichannel",
            post(crate::api::omnichannel_webhook::handle_omnichannel_webhook),
        )
        .with_state(crate::api::omnichannel_webhook::AppState {
            orchestrator: dept_orchestrator.clone(),
            db: db.clone(),
        });

    let inbox_webhook_router =
        crate::api::inbox::webhook::router(crate::api::inbox::webhook::OmnichannelWebhookState {
            orchestrator: dept_orchestrator.clone(),
            db: db.clone(),
        });

    let twilio_voice_engine = Arc::new(crate::voice::VoiceAIEdgeEngine::new());
    let twilio_client = Arc::new(::server_integrations_twilio::provider::TwilioProvider::new(
        std::env::var("TWILIO_ACCOUNT_SID").unwrap_or_default(),
        std::env::var("TWILIO_AUTH_TOKEN").unwrap_or_default(),
    ));
    let twilio_voice_router = Arc::new(crate::voice::VoiceContextRouter::new(
        twilio_voice_engine.clone(),
        twilio_client,
    ));
    let twilio_webhook_router = Router::new()
        .route(
            "/api/v1/webhooks/twilio",
            post(crate::api::twilio_webhook::twilio_webhook_post_handler),
        )
        .route(
            "/api/v1/webhooks/twilio/voice",
            post(crate::api::twilio_webhook::twilio_voice_webhook_handler),
        )
        .route_layer(axum::middleware::from_fn(
            crate::api::twilio_webhook::twilio_signature_middleware,
        ))
        .with_state(crate::api::twilio_webhook::TwilioWebhookState {
            hub: hub.clone(),
            db: db.clone(),
            orchestrator: dept_orchestrator.clone(),
            voice_engine: twilio_voice_engine,
            voice_router: twilio_voice_router,
            voice_sessions: Arc::new(dashmap::DashMap::new()),
        });

    let twilio_voice_webhook_router = Router::new()
        .route(
            "/api/v1/webhooks/twilio_voice",
            post(crate::api::twilio_voice::twilio_voice_incoming_handler),
        )
        .route(
            "/api/v1/webhooks/twilio_voice/gather",
            post(crate::api::twilio_voice::twilio_voice_gather_handler),
        )
        .route(
            "/api/v1/webhooks/twilio_voice/status",
            post(crate::api::twilio_voice::twilio_voice_status_handler),
        )
        .route_layer(axum::middleware::from_fn(
            crate::api::twilio_webhook::twilio_signature_middleware,
        ))
        .with_state(crate::api::twilio_voice::TwilioVoiceWebhookState {
            hub: hub.clone(),
            db: db.clone(),
            orchestrator: dept_orchestrator.clone(),
            voice_engine: Arc::new(crate::voice::VoiceAIEdgeEngine::new()),
            twilio: Arc::new(::server_integrations_twilio::provider::TwilioProvider::new(
                std::env::var("TWILIO_ACCOUNT_SID").unwrap_or_default(),
                std::env::var("TWILIO_AUTH_TOKEN").unwrap_or_default(),
            )),
        });

    Router::new()
        .merge(webhook_router)
        .merge(protect_internal_ingress(
            relay_webhook_router,
            http_auth_store.clone(),
            ctx.tenant_ctx.as_ref(),
        ))
        .merge(meta_webhook_router)
        .merge(protect_internal_ingress(
            omnichannel_webhook_router,
            http_auth_store.clone(),
            ctx.tenant_ctx.as_ref(),
        ))
        .nest(
            "/api/v1/inbox",
            protect_internal_ingress(
                inbox_webhook_router,
                http_auth_store.clone(),
                ctx.tenant_ctx.as_ref(),
            ),
        )
        .merge(twilio_webhook_router)
        .merge(twilio_voice_webhook_router)
        .merge(protect_internal_ingress(
            crate::api::unified_inbox_webhook::router(db.clone()),
            http_auth_store.clone(),
            ctx.tenant_ctx.as_ref(),
        ))
}
