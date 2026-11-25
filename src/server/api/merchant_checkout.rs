//! Solo-owner storefront checkout and BillingPlatformProvider delegation.
use ::ohc_core::extensions::{
    BillingError, BillingPlatformProvider, SyncTransportProvider, TenantContextProvider,
    WorkspaceScope,
};
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use std::sync::Arc;

use crate::db::{DB, DbStore};
use crate::integrations::stripe::client::StripeClient;

#[derive(Clone)]
pub struct MerchantCheckoutState {
    pub db: Arc<DB>,
    pub tenant_ctx: Arc<dyn TenantContextProvider>,
    pub billing_platform: Arc<dyn BillingPlatformProvider>,
    pub sync_transport: Arc<dyn SyncTransportProvider>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct CreateCheckoutSessionRequest {
    pub tier: Option<String>,
    pub is_subscription: Option<bool>,
    pub subscription_interval: Option<String>,
    pub product_id: Option<String>,
    pub quantity: Option<i32>,
    pub ttl_seconds: Option<i32>,
}

#[derive(serde::Serialize)]
pub struct CreateCheckoutSessionResponse {
    pub checkout_url: String,
}

pub fn router<S: Clone + Send + Sync + 'static>(state: MerchantCheckoutState) -> Router<S> {
    Router::new()
        .route(
            "/create-checkout-session",
            post(create_checkout_session_handler),
        )
        .with_state(state)
}

fn require_checkout_owner(request: &axum::extract::Request) -> Result<WorkspaceScope, StatusCode> {
    let claims = request
        .extensions()
        .get::<::server_common::Claims>()
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let tenant_id =
        ::server_common::auth_utils::signed_tenant_id(claims).ok_or(StatusCode::UNAUTHORIZED)?;
    if !claims
        .roles
        .iter()
        .any(|role| role.eq_ignore_ascii_case("owner") || role.eq_ignore_ascii_case("admin"))
    {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(WorkspaceScope {
        workspace_id: tenant_id,
        actor_id: claims.sub.clone(),
        roles: claims.roles.clone(),
    })
}

fn validated_checkout_quantity(quantity: Option<i32>) -> Result<i32, StatusCode> {
    match quantity.unwrap_or(1) {
        value @ 1..=100 => Ok(value),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

fn validated_subscription_interval(interval: Option<&str>) -> Result<&str, StatusCode> {
    match interval.unwrap_or("month") {
        value @ ("day" | "week" | "month" | "year") => Ok(value),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

pub async fn create_checkout_session_handler(
    State(state): State<MerchantCheckoutState>,
    request: axum::extract::Request,
) -> Result<Json<CreateCheckoutSessionResponse>, StatusCode> {
    let scope = require_checkout_owner(&request)?;
    let tenant_id = scope.workspace_id.clone();

    let body_bytes = axum::body::to_bytes(request.into_body(), 1024 * 64)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let req: CreateCheckoutSessionRequest =
        serde_json::from_slice(&body_bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
    if req.tier.is_some() && req.product_id.is_some() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let quantity = validated_checkout_quantity(req.quantity)?;
    let requested_interval = validated_subscription_interval(req.subscription_interval.as_deref())?;

    if req.tier.is_some() {
        if quantity != 1 {
            return Err(StatusCode::BAD_REQUEST);
        }
        let payload = serde_json::to_value(&req).map_err(|_| StatusCode::BAD_REQUEST)?;
        return match state
            .billing_platform
            .create_platform_checkout_session(&scope, payload)
            .await
        {
            Ok(Some(checkout_url)) => Ok(Json(CreateCheckoutSessionResponse { checkout_url })),
            Ok(None) => Err(StatusCode::NOT_IMPLEMENTED),
            Err(BillingError::InvalidInput(_)) => Err(StatusCode::BAD_REQUEST),
            Err(BillingError::Unavailable(_)) => Err(StatusCode::BAD_GATEWAY),
        };
    }

    let Some(product_id) = &req.product_id else {
        return Err(StatusCode::BAD_REQUEST);
    };

    let (mut amount_usd, item_name, actual_interval) = load_merchant_product_terms(
        &state,
        &scope,
        product_id,
        quantity,
        req.is_subscription.unwrap_or(false),
        requested_interval,
    )
    .await?;

    let _ = &mut amount_usd;
    let amount_cents = crate::integrations::stripe::safe_checkout::money_minor_units(amount_usd)
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let api_key = std::env::var("STRIPE_API_KEY").map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let client = StripeClient::new(api_key);
    client
        .require_api_key()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    let mut metadata = std::collections::BTreeMap::<String, String>::new();
    metadata.insert("checkout_kind".into(), "merchant_order".into());
    metadata.insert("tenant_id".into(), tenant_id.clone());
    metadata.insert("product_id".into(), product_id.clone());
    metadata.insert("quantity".into(), quantity.to_string());

    let ttl = req.ttl_seconds.unwrap_or(300);
    let inventory_service = crate::services::inventory::InventoryService::with_db(
        state.db.clone(),
        Some(state.sync_transport.clone()),
    );
    let acquired_lock_id = match inventory_service
        .reserve_inventory(&tenant_id, product_id, quantity, ttl)
        .await
    {
        Ok(result) => {
            if !result.success {
                return Err(StatusCode::CONFLICT);
            }
            if !result.lock_id.is_empty() {
                metadata.insert("inventory_lock_id".into(), result.lock_id.clone());
            }
            result.lock_id
        }
        Err(error) => {
            tracing::error!(
                tenant_id = %tenant_id,
                product_id = %product_id,
                %error,
                "merchant checkout failed to reserve inventory"
            );
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };

    let operation_id = format!("billing_checkout:{}", uuid::Uuid::new_v4());
    match client
        .create_checkout_session_with_metadata(
            crate::integrations::stripe::safe_checkout::CheckoutRequest {
                name: &item_name,
                reference: &tenant_id,
                amount_cents,
                interval: actual_interval.as_deref(),
                product: Some(product_id.as_str()),
                currency: "usd",
                operation_id: &operation_id,
            },
            &metadata,
            false,
        )
        .await
    {
        Ok(receipt) => Ok(Json(CreateCheckoutSessionResponse {
            checkout_url: receipt.url,
        })),
        Err(error) => {
            tracing::error!(
                tenant_id = %tenant_id,
                product_id = %product_id,
                %error,
                "merchant checkout Stripe session creation failed; releasing inventory lock"
            );
            let _ = inventory_service
                .release_inventory(&tenant_id, product_id, quantity, &acquired_lock_id)
                .await;
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

async fn load_merchant_product_terms(
    state: &MerchantCheckoutState,
    scope: &WorkspaceScope,
    product_id: &str,
    quantity: i32,
    is_subscription: bool,
    requested_interval: &str,
) -> Result<(f64, String, Option<String>), StatusCode> {
    use sqlx::Row;
    let tenant_id = &scope.workspace_id;
    match &state.db.store {
        DbStore::Postgres => {
            let mut tx = state.db.pool.begin().await.map_err(|error| {
                tracing::error!(%error, "merchant checkout failed to begin PostgreSQL transaction");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            if crate::server_builder::builder::configured_multi_tenant().unwrap_or(false) {
                state
                    .tenant_ctx
                    .bind_pg_transaction(&mut tx, scope)
                    .await
                    .map_err(|error| {
                        tracing::error!(
                            tenant_id = %tenant_id,
                            %error,
                            "merchant checkout failed to bind multi-tenant PostgreSQL transaction"
                        );
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;
            } else {
                ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id)
                    .await
                    .map_err(|error| {
                        tracing::error!(
                            tenant_id = %tenant_id,
                            %error,
                            "merchant checkout failed to set PostgreSQL tenant context"
                        );
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;
            }
            let row = sqlx::query(
                "SELECT title, price_cents, is_subscribable, subscription_frequency, subscription_discount_percent \
                 FROM products WHERE id = $1 AND tenant_id = $2",
            )
            .bind(product_id)
            .bind(tenant_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
            let price_cents: i64 = row.try_get("price_cents").unwrap_or(0);
            let title: String = row
                .try_get("title")
                .unwrap_or_else(|_| "Product".to_string());
            let is_subscribable: bool = row.try_get("is_subscribable").unwrap_or(false);
            let subscription_frequency: Option<String> =
                row.try_get("subscription_frequency").unwrap_or(None);
            let subscription_discount_percent: i32 =
                row.try_get("subscription_discount_percent").unwrap_or(0);
            let mut amount_usd = (price_cents as f64 / 100.0) * quantity as f64;
            let mut actual_interval = None;
            if is_subscription {
                let plan_row = sqlx::query(
                    "SELECT interval, discount_percentage FROM subscription_plans WHERE product_id = $1 AND tenant_id = $2 ORDER BY created_at DESC LIMIT 1",
                )
                .bind(product_id)
                .bind(tenant_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                if let Some(plan) = plan_row {
                    let interval: String = plan
                        .try_get("interval")
                        .map_err(|_| StatusCode::BAD_REQUEST)?;
                    let interval = validated_subscription_interval(Some(&interval))?.to_string();
                    let discount_percentage: i32 = plan.try_get("discount_percentage").unwrap_or(0);
                    actual_interval = Some(interval);
                    if discount_percentage > 0 {
                        amount_usd *= 1.0 - (discount_percentage as f64 / 100.0);
                    }
                } else if is_subscribable {
                    actual_interval = Some(
                        validated_subscription_interval(subscription_frequency.as_deref())?
                            .to_string(),
                    );
                    if subscription_discount_percent > 0 {
                        amount_usd *= 1.0 - (subscription_discount_percent as f64 / 100.0);
                    }
                } else {
                    actual_interval = Some(requested_interval.to_string());
                }
            }
            tx.commit()
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok((amount_usd, title, actual_interval))
        }
        DbStore::Sqlite(pool) => {
            let row = sqlx::query(
                "SELECT title, price_cents, is_subscribable, subscription_frequency, subscription_discount_percent \
                 FROM products WHERE id = ? AND tenant_id = ?",
            )
            .bind(product_id)
            .bind(tenant_id)
            .fetch_one(pool)
            .await
            .map_err(|_| StatusCode::NOT_FOUND)?;
            let price_cents: i64 = row.try_get("price_cents").unwrap_or(0);
            let title: String = row
                .try_get("title")
                .unwrap_or_else(|_| "Product".to_string());
            let is_subscribable: bool = row.try_get("is_subscribable").unwrap_or(false);
            let subscription_frequency: Option<String> =
                row.try_get("subscription_frequency").unwrap_or(None);
            let subscription_discount_percent: i32 =
                row.try_get("subscription_discount_percent").unwrap_or(0);
            let mut amount_usd = (price_cents as f64 / 100.0) * quantity as f64;
            let mut actual_interval = None;
            if is_subscription {
                let plan_row = sqlx::query(
                    "SELECT interval, discount_percentage FROM subscription_plans WHERE product_id = ? AND tenant_id = ? ORDER BY created_at DESC LIMIT 1",
                )
                .bind(product_id)
                .bind(tenant_id)
                .fetch_optional(pool)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
                if let Some(plan) = plan_row {
                    let interval: String = plan
                        .try_get("interval")
                        .map_err(|_| StatusCode::BAD_REQUEST)?;
                    let interval = validated_subscription_interval(Some(&interval))?.to_string();
                    let discount_percentage: i32 = plan.try_get("discount_percentage").unwrap_or(0);
                    actual_interval = Some(interval);
                    if discount_percentage > 0 {
                        amount_usd *= 1.0 - (discount_percentage as f64 / 100.0);
                    }
                } else if is_subscribable {
                    actual_interval = Some(
                        validated_subscription_interval(subscription_frequency.as_deref())?
                            .to_string(),
                    );
                    if subscription_discount_percent > 0 {
                        amount_usd *= 1.0 - (subscription_discount_percent as f64 / 100.0);
                    }
                } else {
                    actual_interval = Some(requested_interval.to_string());
                }
            }
            Ok((amount_usd, title, actual_interval))
        }
    }
}
