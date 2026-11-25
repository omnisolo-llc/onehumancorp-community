use axum::{
    body::Body,
    extract::{Json, Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

use crate::db::DbStore;
use crate::orchestration::departments::orchestrator::DepartmentOrchestrator;
use ::ohc_core::extensions::{
    BillingPlatformProvider, SyncTransportProvider, TenantContextProvider,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutKind {
    PlatformPlan,
    MerchantOrder,
    ProposalDeposit,
    InvoicePayment,
    Unknown,
}

#[derive(Debug, Clone)]
pub enum PostCommitAction {
    ReleaseInventoryLocks(Value),
    CommitInventory {
        tenant_id: String,
        product_id: String,
        quantity: i32,
        lock_id: String,
    },
    DispatchDepartmentEvent {
        tenant_id: String,
        event_type: String,
        payload: Value,
    },
    ResolveInvoiceTriage {
        invoice_id: String,
    },
    SendFailedPaymentDunning {
        tenant_id: String,
        subscriber_id: String,
        business_name: String,
        event_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessOutcome {
    Processed,
    Duplicate,
}

pub fn valid_event_id(id: &str) -> bool {
    let trimmed = id.trim();
    !trimmed.is_empty()
        && trimmed.len() <= 255
        && !trimmed.chars().any(char::is_control)
        && trimmed
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub fn classify_checkout_kind(object: &Value) -> CheckoutKind {
    let meta = |key: &str| {
        object
            .get("metadata")?
            .get(key)?
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    if let Some(kind) = meta("checkout_kind") {
        return match kind {
            "platform_plan" => CheckoutKind::PlatformPlan,
            "merchant_order" => CheckoutKind::MerchantOrder,
            "proposal_deposit" => CheckoutKind::ProposalDeposit,
            "invoice_payment" => CheckoutKind::InvoicePayment,
            _ => CheckoutKind::Unknown,
        };
    }
    if meta("proposal_id").is_some() {
        return CheckoutKind::ProposalDeposit;
    }
    if meta("invoice_id").is_some() {
        return CheckoutKind::InvoicePayment;
    }
    const MERCHANT_KEYS: &[&str] = &[
        "product_id",
        "order_id",
        "conversational_intake_id",
        "estimate_id",
        "deposit_requirement_id",
        "service_lead_id",
        "proposed_slot_id",
    ];
    if MERCHANT_KEYS.iter().any(|k| meta(k).is_some()) {
        return CheckoutKind::MerchantOrder;
    }
    CheckoutKind::Unknown
}

#[derive(Clone)]
pub struct WebhookState {
    pub tenant_ctx: Arc<dyn TenantContextProvider>,
    pub billing_platform: Arc<dyn BillingPlatformProvider>,
    pub sync_transport: Arc<dyn SyncTransportProvider>,
    pub db_pool: sqlx::Pool<sqlx::Postgres>,
    pub db: std::sync::Arc<crate::db::DB>,
    pub orchestrator: std::sync::Arc<DepartmentOrchestrator>,
}

async fn begin_webhook_system_transaction<'a>(
    tenant_ctx: &dyn TenantContextProvider,
    pool: &'a sqlx::PgPool,
) -> Result<sqlx::Transaction<'a, sqlx::Postgres>, String> {
    let mut transaction = pool.begin().await.map_err(|e| e.to_string())?;
    tenant_ctx
        .bind_pg_system_transaction(&mut transaction)
        .await
        .map_err(|e| e.to_string())?;
    Ok(transaction)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StripeEvent {
    pub id: String,
    pub r#type: String,
    pub data: StripeEventData,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StripeEventData {
    pub object: Value,
}

#[async_trait::async_trait]
pub trait PaymentFailureNotifier: Send + Sync {
    async fn send_payment_failure_sms(
        &self,
        tenant_id: &str,
        subscriber_id: &str,
        message: &str,
    ) -> Result<(), String>;
}

#[async_trait::async_trait]
pub trait PaymentFailureMessageGenerator: Send + Sync {
    async fn generate_payment_failure_message(
        &self,
        subscriber_id: &str,
        business_name: &str,
    ) -> String;
}

pub struct CriticalSmsPaymentFailureNotifier {
    pub event_id: String,
}
pub struct LlmPaymentFailureMessageGenerator;

#[async_trait::async_trait]
impl PaymentFailureNotifier for CriticalSmsPaymentFailureNotifier {
    async fn send_payment_failure_sms(
        &self,
        tenant_id: &str,
        _subscriber_id: &str,
        message: &str,
    ) -> Result<(), String> {
        crate::api::sms_settings::dispatch_critical_sms(
            tenant_id,
            &self.event_id,
            "failed_payment",
            message,
        )
        .await
        .map(|_| ())
    }
}

#[async_trait::async_trait]
impl PaymentFailureMessageGenerator for LlmPaymentFailureMessageGenerator {
    async fn generate_payment_failure_message(
        &self,
        subscriber_id: &str,
        business_name: &str,
    ) -> String {
        let fallback = format!(
            "{} subscription payment could not be processed. Please update the saved payment method to keep the subscription active.",
            business_name
        );
        let prompt = format!(
            "Write a concise, helpful SMS for subscription payment recovery. Business: {}. Subscriber id: {}. Mention the payment could not be processed and ask them to update their saved payment method. Avoid blame and keep it under 240 characters.",
            business_name, subscriber_id
        );

        match std::env::var("OMNISOLO_LLM_PROVIDER").as_deref() {
            Ok("minimax") => {
                let api_key = std::env::var("MINIMAX_API_KEY").unwrap_or_default();
                crate::minimax::MinimaxClient::new(api_key)
                    .reason(&prompt)
                    .await
                    .unwrap_or(fallback)
            }
            _ => crate::minimax::LocalLLMClient::new()
                .reason(&prompt)
                .await
                .unwrap_or(fallback),
        }
    }
}

pub async fn send_payment_failure_dunning<N, G>(
    notifier: &N,
    generator: &G,
    tenant_id: &str,
    subscriber_id: &str,
    business_name: &str,
) -> Result<(), String>
where
    N: PaymentFailureNotifier,
    G: PaymentFailureMessageGenerator,
{
    let message = generator
        .generate_payment_failure_message(subscriber_id, business_name)
        .await;
    notifier
        .send_payment_failure_sms(tenant_id, subscriber_id, &message)
        .await
}

pub fn inventory_locks_for_payment_success(object: &Value) -> Vec<String> {
    let Some(metadata) = object.get("metadata") else {
        return Vec::new();
    };
    let mut locks = Vec::new();
    if let Some(lock_id) = metadata.get("inventory_lock_id").and_then(|v| v.as_str())
        && !lock_id.trim().is_empty()
    {
        locks.push(lock_id.to_string());
    }
    if let Some(lock_ids) = metadata
        .get("inventory_lock_ids")
        .and_then(|v| v.as_array())
    {
        for lock_id in lock_ids.iter().filter_map(|v| v.as_str()) {
            if !lock_id.trim().is_empty() && !locks.iter().any(|existing| existing == lock_id) {
                locks.push(lock_id.to_string());
            }
        }
    }
    locks
}

async fn release_inventory_locks_for_payment(webhook_state: &WebhookState, object: &Value) {
    let locks = inventory_locks_for_payment_success(object);
    if locks.is_empty() {
        return;
    }
    let inventory_service = crate::services::inventory::InventoryService::new(Some(
        webhook_state.sync_transport.clone(),
    ));
    for lock_id in locks {
        inventory_service.clear_lock_key(&lock_id).await;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentFailureLookup {
    pub stripe_subscription_id: Option<String>,
    pub customer_id: Option<String>,
}

pub fn payment_failure_lookup(object: &Value) -> PaymentFailureLookup {
    let stripe_subscription_id = object
        .get("subscription")
        .and_then(|value| value.as_str())
        .or_else(|| {
            object
                .get("parent")
                .and_then(|parent| parent.get("subscription_details"))
                .and_then(|details| details.get("subscription"))
                .and_then(|value| value.as_str())
        })
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);

    let customer_id = object
        .get("customer")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);

    PaymentFailureLookup {
        stripe_subscription_id,
        customer_id,
    }
}

async fn find_subscriber_for_payment_failure(
    webhook_state: &WebhookState,
    lookup: &PaymentFailureLookup,
) -> Result<Option<(String, String)>, String> {
    let subscription_id = lookup.stripe_subscription_id.as_deref().unwrap_or("");
    let customer_id = lookup.customer_id.as_deref().unwrap_or("");
    if subscription_id.is_empty() && customer_id.is_empty() {
        return Ok(None);
    }

    match &webhook_state.db.store {
        DbStore::Sqlite(pool) => {
            let rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT id, tenant_id FROM subscribers \
                 WHERE (?1 != '' AND stripe_subscription_id = ?1 AND (?2 = '' OR customer_id = ?2)) \
                    OR (?1 = '' AND ?2 != '' AND customer_id = ?2) \
                 LIMIT 2",
            )
            .bind(subscription_id)
            .bind(customer_id)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("Failed to lookup failed-payment subscriber: {e}"))?;
            if rows.len() > 1 {
                Err("Failed-payment subscriber identity is ambiguous".into())
            } else {
                Ok(rows.into_iter().next())
            }
        }
        DbStore::Postgres => {
            let mut transaction = begin_webhook_system_transaction(
                webhook_state.tenant_ctx.as_ref(),
                &webhook_state.db.pool,
            )
            .await?;
            let rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT id, tenant_id FROM subscribers \
                 WHERE ($1 != '' AND stripe_subscription_id = $1 AND ($2 = '' OR customer_id = $2)) \
                    OR ($1 = '' AND $2 != '' AND customer_id = $2) \
                 LIMIT 2",
            )
            .bind(subscription_id)
            .bind(customer_id)
            .fetch_all(&mut *transaction)
            .await
            .map_err(|e| format!("Failed to lookup failed-payment subscriber: {e}"))?;
            transaction.commit().await.map_err(|e| e.to_string())?;
            if rows.len() > 1 {
                Err("Failed-payment subscriber identity is ambiguous".into())
            } else {
                Ok(rows.into_iter().next())
            }
        }
    }
}

async fn mark_subscriber_past_due(
    webhook_state: &WebhookState,
    subscriber_id: &str,
    tenant_id: &str,
) -> Result<(), String> {
    match &webhook_state.db.store {
        DbStore::Sqlite(pool) => {
            sqlx::query("UPDATE subscribers SET status = 'PAST_DUE' WHERE id = ? AND tenant_id = ?")
                .bind(subscriber_id)
                .bind(tenant_id)
                .execute(pool)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        DbStore::Postgres => {
            let mut transaction = begin_webhook_system_transaction(
                webhook_state.tenant_ctx.as_ref(),
                &webhook_state.db.pool,
            )
            .await?;
            sqlx::query(
                "UPDATE subscribers SET status = 'PAST_DUE' WHERE id = $1 AND tenant_id = $2",
            )
            .bind(subscriber_id)
            .bind(tenant_id)
            .execute(&mut *transaction)
            .await
            .map_err(|e| e.to_string())?;
            transaction.commit().await.map_err(|e| e.to_string())
        }
    }
    .map_err(|e| format!("Failed to mark subscriber past due: {e}"))
}

pub async fn process_invoice_payment_failed<N, G>(
    webhook_state: &WebhookState,
    object: &Value,
    notifier: &N,
    generator: &G,
) -> Result<Option<String>, String>
where
    N: PaymentFailureNotifier,
    G: PaymentFailureMessageGenerator,
{
    let lookup = payment_failure_lookup(object);
    let Some((subscriber_id, tenant_id)) =
        find_subscriber_for_payment_failure(webhook_state, &lookup).await?
    else {
        return Ok(None);
    };

    if tenant_id.trim().is_empty() {
        return Err("Failed-payment subscriber has no authoritative tenant".into());
    }
    mark_subscriber_past_due(webhook_state, &subscriber_id, &tenant_id).await?;
    let business_name = object
        .get("metadata")
        .and_then(|metadata| metadata.get("business_name"))
        .and_then(|value| value.as_str())
        .unwrap_or("Your business");
    send_payment_failure_dunning(
        notifier,
        generator,
        &tenant_id,
        &subscriber_id,
        business_name,
    )
    .await?;

    Ok(Some(subscriber_id))
}

pub async fn webhook_security_middleware(
    State(_webhook_state): State<WebhookState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // SEC-03: Fail closed BEFORE buffering any request body for all non-Stripe
    // webhook routes until provider-native constant-time verifiers are implemented.
    if req.uri().path() != "/api/v1/webhooks/stripe" {
        return Err(StatusCode::NOT_IMPLEMENTED);
    }

    // Authenticates raw bytes with constant-time HMAC-SHA256, +/-300s timestamp
    // tolerance, and a strict <= 1 MiB body limit.
    let req = super::stripe_webhook_security::verify_request(
        req,
        super::stripe_webhook_security::Endpoint::Billing,
    )
    .await?;

    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(
        body,
        crate::integrations::stripe::webhook_signature::MAX_BODY_BYTES,
    )
    .await
    .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;

    let event: StripeEvent = serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
    if !valid_event_id(&event.id) {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Synchronously run the handler so durable DB idempotency and business
    // updates commit BEFORE acknowledging HTTP 200 OK to Stripe.
    let new_req = Request::from_parts(parts, Body::from(bytes));
    Ok(next.run(new_req).await)
}

pub async fn execute_post_commit_actions(state: &WebhookState, actions: Vec<PostCommitAction>) {
    for action in actions {
        match action {
            PostCommitAction::ReleaseInventoryLocks(obj) => {
                release_inventory_locks_for_payment(state, &obj).await;
            }
            PostCommitAction::CommitInventory {
                tenant_id,
                product_id,
                quantity,
                lock_id,
            } => {
                let inventory_service = crate::services::inventory::InventoryService::new(Some(
                    state.sync_transport.clone(),
                ));
                let _ = inventory_service
                    .commit_inventory(&tenant_id, &product_id, quantity, &lock_id)
                    .await;
            }
            PostCommitAction::DispatchDepartmentEvent {
                tenant_id,
                event_type,
                payload,
            } => {
                let orch = state.orchestrator.clone();
                tokio::spawn(async move {
                    let evt = crate::orchestration::departments::types::DepartmentEvent {
                        id: uuid::Uuid::new_v4().to_string(),
                        tenant_id,
                        event_type,
                        payload,
                    };
                    let _ = orch.dispatch_event(evt).await;
                });
            }
            PostCommitAction::ResolveInvoiceTriage { invoice_id } => match &state.db.store {
                DbStore::Postgres => {
                    if let Ok(mut tx) =
                        begin_webhook_system_transaction(state.tenant_ctx.as_ref(), &state.db.pool)
                            .await
                    {
                        let _ = sqlx::query(
                            "UPDATE triage_items SET status = 'resolved' \
                             WHERE action_type = 'Approve Draft' AND action_payload LIKE '%' || $1 || '%'",
                        )
                        .bind(&invoice_id)
                        .execute(&mut *tx)
                        .await;
                        let _ = tx.commit().await;
                    }
                }
                DbStore::Sqlite(pool) => {
                    let _ = sqlx::query(
                        "UPDATE triage_items SET status = 'resolved' \
                         WHERE action_type = 'Approve Draft' AND action_payload LIKE '%' || ? || '%'",
                    )
                    .bind(&invoice_id)
                    .execute(pool)
                    .await;
                }
            },
            PostCommitAction::SendFailedPaymentDunning {
                tenant_id,
                subscriber_id,
                business_name,
                event_id,
            } => {
                let notifier = CriticalSmsPaymentFailureNotifier { event_id };
                let _ = send_payment_failure_dunning(
                    &notifier,
                    &LlmPaymentFailureMessageGenerator,
                    &tenant_id,
                    &subscriber_id,
                    &business_name,
                )
                .await;
            }
        }
    }
}

pub async fn stripe_webhook_handler(
    axum::extract::State(webhook_state): axum::extract::State<WebhookState>,
    Json(payload): Json<StripeEvent>,
) -> impl IntoResponse {
    if crate::api::merchant_webhooks::is_merchant_stripe_event(&payload) {
        return match crate::api::merchant_webhooks::process_merchant_stripe_event(
            &webhook_state.db,
            &payload,
            None,
            Some(webhook_state.tenant_ctx.as_ref()),
        )
        .await
        {
            Ok((_, actions)) => {
                execute_post_commit_actions(&webhook_state, actions).await;
                StatusCode::OK.into_response()
            }
            Err(status) => status.into_response(),
        };
    }

    let Ok(event_json) = serde_json::to_value(&payload) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match webhook_state
        .billing_platform
        .reconcile_platform_webhook_event(event_json)
        .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(::ohc_core::extensions::BillingError::InvalidInput(_)) => {
            StatusCode::BAD_REQUEST.into_response()
        }
        Err(::ohc_core::extensions::BillingError::Unavailable(_)) => {
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct MercadoPagoEvent {
    pub id: i64,
    pub live_mode: bool,
    pub r#type: String,
    pub date_created: String,
    pub application_id: i64,
    pub user_id: i64,
    pub version: i32,
    pub api_version: String,
    pub action: String,
    pub data: MercadoPagoEventData,
}

#[derive(Debug, Deserialize)]
pub struct MercadoPagoEventData {
    pub id: String,
}

pub async fn mercadopago_webhook_handler(
    axum::extract::State(_webhook_state): axum::extract::State<WebhookState>,
    Json(_payload): Json<MercadoPagoEvent>,
) -> impl IntoResponse {
    StatusCode::NOT_IMPLEMENTED.into_response()
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub struct RazorpayEvent {
    pub event: String,
    pub payload: RazorpayPayload,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub struct RazorpayPayload {
    pub payment: RazorpayPaymentEntity,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub struct RazorpayPaymentEntity {
    pub entity: RazorpayEntity,
}

#[derive(Debug, Deserialize, serde::Serialize)]
pub struct RazorpayEntity {
    pub id: String,
    pub status: String,
    pub order_id: String,
}

pub async fn razorpay_webhook_handler(
    _headers: axum::http::HeaderMap,
    axum::extract::State(_webhook_state): axum::extract::State<WebhookState>,
    Json(_payload): Json<RazorpayEvent>,
) -> impl IntoResponse {
    StatusCode::NOT_IMPLEMENTED.into_response()
}

#[derive(Debug, Deserialize)]
pub struct CalComEvent {
    pub trigger_event: String,
    pub payload: CalComPayload,
}

#[derive(Debug, Deserialize)]
pub struct CalComPayload {
    pub uid: String,
    pub title: String,
    pub start_time: String,
    pub end_time: String,
    pub attendees: Vec<CalComAttendee>,
}

#[derive(Debug, Deserialize)]
pub struct CalComAttendee {
    pub email: String,
    pub name: String,
}

pub async fn calcom_webhook_handler(
    axum::extract::State(_webhook_state): axum::extract::State<WebhookState>,
    Json(_payload): Json<CalComEvent>,
) -> impl IntoResponse {
    StatusCode::NOT_IMPLEMENTED.into_response()
}

#[derive(Debug, Deserialize)]
pub struct ResendEvent {
    #[serde(rename = "type")]
    pub type_: String,
    pub data: ResendEventData,
}

#[derive(Debug, Deserialize)]
pub struct ResendEventData {
    pub email_id: String,
    pub to: Vec<String>,
}

pub async fn resend_webhook_handler(
    axum::extract::State(_webhook_state): axum::extract::State<WebhookState>,
    Json(_payload): Json<ResendEvent>,
) -> impl IntoResponse {
    StatusCode::NOT_IMPLEMENTED.into_response()
}

#[derive(Debug, Deserialize)]
pub struct AyrshareEvent {
    pub action: String,
    pub message: String,
    pub platform: String,
    pub profile_key: String,
}

pub async fn ayrshare_webhook_handler(
    axum::extract::State(_webhook_state): axum::extract::State<WebhookState>,
    Json(_payload): Json<AyrshareEvent>,
) -> impl IntoResponse {
    StatusCode::NOT_IMPLEMENTED.into_response()
}

#[derive(Debug, Deserialize)]
pub struct ManychatEvent {
    pub status: String,
    pub messages: Vec<ManychatMessage>,
}

#[derive(Debug, Deserialize)]
pub struct ManychatMessage {
    pub id: String,
    pub text: String,
}

pub async fn manychat_webhook_handler(
    axum::extract::State(_webhook_state): axum::extract::State<WebhookState>,
    Json(_payload): Json<ManychatEvent>,
) -> impl IntoResponse {
    StatusCode::NOT_IMPLEMENTED.into_response()
}

#[derive(Debug, serde::Deserialize)]
pub struct CalendlyEvent {
    pub event: String,
    pub payload: serde_json::Value,
}

pub async fn calendly_webhook_handler(
    axum::extract::State(_webhook_state): axum::extract::State<WebhookState>,
    axum::Json(_payload): axum::Json<CalendlyEvent>,
) -> impl axum::response::IntoResponse {
    axum::http::StatusCode::NOT_IMPLEMENTED.into_response()
}

#[derive(Debug, serde::Deserialize)]
pub struct MailchimpEvent {
    pub r#type: String,
    pub data: serde_json::Value,
}

pub async fn mailchimp_webhook_handler(
    axum::extract::State(_webhook_state): axum::extract::State<WebhookState>,
    axum::Json(_payload): axum::Json<MailchimpEvent>,
) -> impl axum::response::IntoResponse {
    axum::http::StatusCode::NOT_IMPLEMENTED.into_response()
}
