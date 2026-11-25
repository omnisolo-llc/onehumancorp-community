use crate::hub::Hub;
use axum::http::HeaderMap;
use axum::{
    Json,
    extract::{Extension, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const MAX_CHECKOUT_AMOUNT_CENTS: i64 = 100_000_000;
const MAX_ITEM_QUANTITY: i64 = 10_000;

#[derive(Deserialize)]
pub struct CreateCheckoutSessionRequest {
    #[serde(default)]
    pub tenant_id: Option<String>,
    pub r#type: String,
    pub amount_cents: i64,
    pub device_id: Option<String>,
    pub cart_payload: Option<serde_json::Value>,
    pub discount_code: Option<String>,
}

#[derive(Serialize)]
pub struct CreateCheckoutSessionResponse {
    pub session_id: String,
    pub success: bool,
    pub error_message: Option<String>,
}

fn checkout_error(status: StatusCode, message: &'static str) -> axum::response::Response {
    (
        status,
        Json(CreateCheckoutSessionResponse {
            session_id: String::new(),
            success: false,
            error_message: Some(message.to_string()),
        }),
    )
        .into_response()
}

#[allow(clippy::result_large_err)]
fn validate_checkout_request(
    claims: Option<&::server_common::Claims>,
    req_data: &CreateCheckoutSessionRequest,
) -> Result<String, axum::response::Response> {
    let Some(claims) = claims else {
        return Err(checkout_error(
            StatusCode::UNAUTHORIZED,
            "authentication required",
        ));
    };
    let Some(tenant_id) = ::server_common::auth_utils::signed_tenant_id(claims) else {
        return Err(checkout_error(
            StatusCode::UNAUTHORIZED,
            "authentication required",
        ));
    };

    if let Some(body_tenant) = req_data.tenant_id.as_deref().map(str::trim)
        && !body_tenant.is_empty()
        && body_tenant != tenant_id
    {
        return Err(checkout_error(StatusCode::FORBIDDEN, "tenant mismatch"));
    }

    let checkout_type = req_data.r#type.trim();
    if checkout_type.is_empty() || checkout_type.len() > 64 {
        return Err(checkout_error(
            StatusCode::BAD_REQUEST,
            "invalid checkout type",
        ));
    }

    if req_data.amount_cents <= 0 || req_data.amount_cents > MAX_CHECKOUT_AMOUNT_CENTS {
        return Err(checkout_error(
            StatusCode::BAD_REQUEST,
            "amount_cents must be a positive bounded integer",
        ));
    }

    if let Some(cart) = &req_data.cart_payload
        && !cart.is_null()
    {
        if let Some(items) = cart.as_array() {
            for item in items {
                let product_id = item
                    .get("product")
                    .and_then(|p| p.get("id"))
                    .and_then(|id| id.as_str())
                    .map(str::trim)
                    .unwrap_or("");
                let quantity = item.get("quantity").and_then(|q| q.as_i64());
                if product_id.is_empty()
                    || !matches!(quantity, Some(q) if (1..=MAX_ITEM_QUANTITY).contains(&q))
                {
                    return Err(checkout_error(
                        StatusCode::BAD_REQUEST,
                        "cart items must specify a valid product id and positive quantity",
                    ));
                }
            }
        } else if let Some(items) = cart.get("items").and_then(|i| i.as_array()) {
            for item in items {
                let product_id = item
                    .get("product_id")
                    .and_then(|id| id.as_str())
                    .map(str::trim)
                    .unwrap_or("");
                let quantity = item.get("quantity").and_then(|q| q.as_i64()).unwrap_or(1);
                if product_id.is_empty() || !(1..=MAX_ITEM_QUANTITY).contains(&quantity) {
                    return Err(checkout_error(
                        StatusCode::BAD_REQUEST,
                        "cart items must specify a valid product id and positive quantity",
                    ));
                }
            }
        } else {
            return Err(checkout_error(
                StatusCode::BAD_REQUEST,
                "invalid cart_payload format",
            ));
        }
    }

    Ok(tenant_id)
}

async fn lookup_catalog_unit_price_cents(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: &str,
    product_id: &str,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COALESCE(NULLIF(price_cents, 0), ROUND(COALESCE(price, 0) * 100)::BIGINT) \
         FROM products WHERE id = $1 AND tenant_id = $2",
    )
    .bind(product_id)
    .bind(tenant_id)
    .fetch_optional(&mut **tx)
    .await
}

pub async fn create_checkout_session_handler(
    _headers: HeaderMap,
    claims: Option<Extension<::server_common::Claims>>,
    State(hub): State<Arc<Hub>>,
    req_data: axum::extract::Json<CreateCheckoutSessionRequest>,
) -> axum::response::Response {
    let tenant_id =
        match validate_checkout_request(claims.as_ref().map(|Extension(c)| c), &req_data) {
            Ok(id) => id,
            Err(resp) => return resp,
        };
    let session_id = uuid::Uuid::new_v4().to_string();

    let mut db_tx = match hub.pool.begin().await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::error!("checkout session transaction begin failed: {}", e);
            return checkout_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to create checkout session",
            );
        }
    };

    if let Err(e) = ::server_common::auth_utils::set_org_context(&mut *db_tx, &tenant_id).await {
        tracing::error!("checkout session set_org_context failed: {}", e);
        let _ = db_tx.rollback().await;
        return checkout_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to create checkout session",
        );
    }

    let mut reserved_locks: Vec<(String, String, i32)> = Vec::new();
    let mut updated_cart_payload = req_data.cart_payload.clone();
    let inventory_service =
        crate::services::inventory::InventoryService::new(Some(hub.sync_transport()));
    let mut server_computed_cents: Option<i64> = None;

    if let Some(cart) = &mut updated_cart_payload
        && let Some(items) = cart.as_array_mut()
    {
        let ttl = if req_data.r#type == "IN_PERSON" {
            15
        } else {
            300
        };
        let mut running_total: i64 = 0;

        for item in items {
            if let (Some(product_obj), Some(quantity)) = (
                item.get("product").cloned(),
                item.get("quantity").and_then(|q| q.as_i64()),
            ) && let Some(product_id) = product_obj.get("id").and_then(|id| id.as_str())
            {
                let unit_price =
                    match lookup_catalog_unit_price_cents(&mut db_tx, &tenant_id, product_id).await
                    {
                        Ok(Some(price)) if price > 0 => price,
                        Ok(_) => {
                            for (pid, lid, qty) in reserved_locks {
                                let _ = inventory_service
                                    .release_inventory(&tenant_id, &pid, qty, &lid)
                                    .await;
                            }
                            let _ = db_tx.rollback().await;
                            return checkout_error(
                                StatusCode::BAD_REQUEST,
                                "invalid product or pricing in cart",
                            );
                        }
                        Err(e) => {
                            tracing::error!("checkout product lookup failed: {}", e);
                            for (pid, lid, qty) in reserved_locks {
                                let _ = inventory_service
                                    .release_inventory(&tenant_id, &pid, qty, &lid)
                                    .await;
                            }
                            let _ = db_tx.rollback().await;
                            return checkout_error(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "failed to create checkout session",
                            );
                        }
                    };
                running_total = match unit_price
                    .checked_mul(quantity)
                    .and_then(|line| running_total.checked_add(line))
                {
                    Some(sum) if sum <= MAX_CHECKOUT_AMOUNT_CENTS => sum,
                    _ => {
                        for (pid, lid, qty) in reserved_locks {
                            let _ = inventory_service
                                .release_inventory(&tenant_id, &pid, qty, &lid)
                                .await;
                        }
                        let _ = db_tx.rollback().await;
                        return checkout_error(
                            StatusCode::BAD_REQUEST,
                            "cart total exceeds maximum allowed amount",
                        );
                    }
                };

                let reserve_result = inventory_service
                    .reserve_inventory(&tenant_id, product_id, quantity as i32, ttl)
                    .await;
                match reserve_result {
                    Ok(res) if res.success => {
                        reserved_locks.push((
                            product_id.to_string(),
                            res.lock_id.clone(),
                            quantity as i32,
                        ));
                        if let Some(obj) = item.as_object_mut() {
                            obj.insert(
                                "lock_id".to_string(),
                                serde_json::Value::String(res.lock_id),
                            );
                        }
                    }
                    _ => {
                        for (pid, lid, qty) in reserved_locks {
                            let _ = inventory_service
                                .release_inventory(&tenant_id, &pid, qty, &lid)
                                .await;
                        }
                        let _ = db_tx.rollback().await;
                        return checkout_error(
                            StatusCode::CONFLICT,
                            "Item is currently being checked out by another customer.",
                        );
                    }
                }
            }
        }
        if running_total > 0 {
            server_computed_cents = Some(running_total);
        }
    }

    // Redlock inventory reservation + server-side pricing for object-shaped cart payload
    if let Some(cart) = &req_data.cart_payload
        && let Some(items) = cart.get("items").and_then(|i| i.as_array())
    {
        let mut running_total: i64 = 0;
        for item in items {
            if let Some(product_id) = item.get("product_id").and_then(|p| p.as_str()) {
                let quantity = item.get("quantity").and_then(|q| q.as_i64()).unwrap_or(1);
                let unit_price =
                    match lookup_catalog_unit_price_cents(&mut db_tx, &tenant_id, product_id).await
                    {
                        Ok(Some(price)) if price > 0 => price,
                        Ok(_) => {
                            for (pid, lid, qty) in reserved_locks {
                                let _ = inventory_service
                                    .release_inventory(&tenant_id, &pid, qty, &lid)
                                    .await;
                            }
                            let _ = db_tx.rollback().await;
                            return checkout_error(
                                StatusCode::BAD_REQUEST,
                                "invalid product or pricing in cart",
                            );
                        }
                        Err(e) => {
                            tracing::error!("checkout product lookup failed: {}", e);
                            for (pid, lid, qty) in reserved_locks {
                                let _ = inventory_service
                                    .release_inventory(&tenant_id, &pid, qty, &lid)
                                    .await;
                            }
                            let _ = db_tx.rollback().await;
                            return checkout_error(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "failed to create checkout session",
                            );
                        }
                    };
                running_total = match unit_price
                    .checked_mul(quantity)
                    .and_then(|line| running_total.checked_add(line))
                {
                    Some(sum) if sum <= MAX_CHECKOUT_AMOUNT_CENTS => sum,
                    _ => {
                        for (pid, lid, qty) in reserved_locks {
                            let _ = inventory_service
                                .release_inventory(&tenant_id, &pid, qty, &lid)
                                .await;
                        }
                        let _ = db_tx.rollback().await;
                        return checkout_error(
                            StatusCode::BAD_REQUEST,
                            "cart total exceeds maximum allowed amount",
                        );
                    }
                };

                match inventory_service
                    .reserve_inventory(&tenant_id, product_id, quantity as i32, 300)
                    .await
                {
                    Ok(res) if res.success => {
                        reserved_locks.push((product_id.to_string(), res.lock_id, quantity as i32));
                    }
                    Ok(_) => {
                        for (pid, lid, qty) in reserved_locks {
                            let _ = inventory_service
                                .release_inventory(&tenant_id, &pid, qty, &lid)
                                .await;
                        }
                        let _ = db_tx.rollback().await;
                        return checkout_error(
                            StatusCode::CONFLICT,
                            "Item is currently being checked out by another customer.",
                        );
                    }
                    Err(e) => {
                        tracing::error!("checkout inventory reservation failed: {}", e);
                        for (pid, lid, qty) in reserved_locks {
                            let _ = inventory_service
                                .release_inventory(&tenant_id, &pid, qty, &lid)
                                .await;
                        }
                        let _ = db_tx.rollback().await;
                        return checkout_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "failed to reserve inventory",
                        );
                    }
                }
            }
        }
        if running_total > 0 {
            server_computed_cents = Some(running_total);
        }
    }

    let mut final_amount = if let Some(computed) = server_computed_cents {
        if req_data.amount_cents != computed {
            for (pid, lid, qty) in reserved_locks {
                let _ = inventory_service
                    .release_inventory(&tenant_id, &pid, qty, &lid)
                    .await;
            }
            let _ = db_tx.rollback().await;
            return checkout_error(
                StatusCode::BAD_REQUEST,
                "checkout amount does not match catalog pricing",
            );
        }
        computed
    } else {
        req_data.amount_cents
    };

    if let Some(discount_code) = &req_data.discount_code {
        let redeemed: Result<Option<String>, sqlx::Error> = sqlx::query_scalar(
            "UPDATE reward_claims SET status = 'Used' \
             WHERE tenant_id = $1 AND discount_code = $2 AND status = 'Active' \
             RETURNING discount_code",
        )
        .bind(&tenant_id)
        .bind(discount_code)
        .fetch_optional(&mut *db_tx)
        .await;

        if let Ok(Some(_)) = redeemed {
            final_amount = ((final_amount * 85) / 100).max(1);
        }
    }

    let query = sqlx::query(
        "INSERT INTO checkout_sessions (id, tenant_id, type, amount_cents, device_id, cart_payload, status)
         VALUES ($1, $2, $3, $4, $5, $6, 'PENDING')"
    )
    .bind(&session_id)
    .bind(&tenant_id)
    .bind(&req_data.r#type)
    .bind(final_amount)
    .bind(&req_data.device_id)
    .bind(&updated_cart_payload);

    match query.execute(&mut *db_tx).await {
        Ok(_) => {
            if let Err(e) = db_tx.commit().await {
                tracing::error!("checkout session commit failed: {}", e);
                for (pid, lid, qty) in reserved_locks {
                    let _ = inventory_service
                        .release_inventory(&tenant_id, &pid, qty, &lid)
                        .await;
                }
                return checkout_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to create checkout session",
                );
            }
            (
                StatusCode::OK,
                Json(CreateCheckoutSessionResponse {
                    session_id,
                    success: true,
                    error_message: None,
                }),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("checkout session insert failed: {}", e);
            for (pid, lid, qty) in reserved_locks {
                let _ = inventory_service
                    .release_inventory(&tenant_id, &pid, qty, &lid)
                    .await;
            }
            let _ = db_tx.rollback().await;
            checkout_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to create checkout session",
            )
        }
    }
}

pub fn router(
    hub: Arc<Hub>,
) -> axum::Router<Arc<dyn omnisolo_builtin_agent::mesh::transport::MeshTransport>> {
    axum::Router::new()
        .route(
            "/session",
            axum::routing::post(create_checkout_session_handler),
        )
        .with_state(hub)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_claims(org_id: Option<&str>) -> ::server_common::Claims {
        ::server_common::Claims {
            sub: "user-1".to_string(),
            username: "buyer".to_string(),
            email: "buyer@example.test".to_string(),
            roles: vec!["VIEWER".to_string()],
            organization_id: org_id.map(str::to_string),
            session_id: None,
            iat: 0,
            exp: chrono::Utc::now().timestamp() + 3600,
            jti: "jti-checkout".to_string(),
        }
    }

    fn base_req() -> CreateCheckoutSessionRequest {
        CreateCheckoutSessionRequest {
            tenant_id: Some("tenant-a".to_string()),
            r#type: "ONLINE".to_string(),
            amount_cents: 2500,
            device_id: None,
            cart_payload: None,
            discount_code: None,
        }
    }

    #[test]
    fn checkout_requires_signed_tenant_claims_and_rejects_mismatched_body_tenant() {
        let req = base_req();

        let missing = validate_checkout_request(None, &req).unwrap_err();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let system_claims = test_claims(Some("system"));
        let sys_err = validate_checkout_request(Some(&system_claims), &req).unwrap_err();
        assert_eq!(sys_err.status(), StatusCode::UNAUTHORIZED);

        let attacker_claims = test_claims(Some("tenant-a"));
        let mut cross_tenant_req = base_req();
        cross_tenant_req.tenant_id = Some("tenant-victim".to_string());
        let mismatch =
            validate_checkout_request(Some(&attacker_claims), &cross_tenant_req).unwrap_err();
        assert_eq!(mismatch.status(), StatusCode::FORBIDDEN);

        // Matching or omitted body tenant resolves exclusively to signed tenant
        assert_eq!(
            validate_checkout_request(Some(&attacker_claims), &req).unwrap(),
            "tenant-a"
        );
        let mut omitted_req = base_req();
        omitted_req.tenant_id = None;
        assert_eq!(
            validate_checkout_request(Some(&attacker_claims), &omitted_req).unwrap(),
            "tenant-a"
        );
    }

    #[test]
    fn checkout_rejects_non_positive_amounts_and_invalid_quantities() {
        let claims = test_claims(Some("tenant-a"));
        for bad_amount in [0, -1, -5000, MAX_CHECKOUT_AMOUNT_CENTS + 1] {
            let mut req = base_req();
            req.amount_cents = bad_amount;
            let err = validate_checkout_request(Some(&claims), &req).unwrap_err();
            assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        }

        let mut bad_qty = base_req();
        bad_qty.cart_payload = Some(serde_json::json!({
            "items": [{ "product_id": "prod-1", "quantity": 0 }]
        }));
        let err = validate_checkout_request(Some(&claims), &bad_qty).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);

        let mut neg_qty = base_req();
        neg_qty.cart_payload = Some(serde_json::json!([
            { "product": { "id": "prod-1" }, "quantity": -2 }
        ]));
        let err = validate_checkout_request(Some(&claims), &neg_qty).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);

        // Malformed cart items without product id cannot silently bypass server pricing checks
        let mut missing_prod = base_req();
        missing_prod.cart_payload = Some(serde_json::json!([
            { "product": { "id": "" }, "quantity": 1 }
        ]));
        assert_eq!(
            validate_checkout_request(Some(&claims), &missing_prod)
                .unwrap_err()
                .status(),
            StatusCode::BAD_REQUEST
        );

        let mut malformed_shape = base_req();
        malformed_shape.cart_payload = Some(serde_json::json!({ "unexpected": true }));
        assert_eq!(
            validate_checkout_request(Some(&claims), &malformed_shape)
                .unwrap_err()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
}
