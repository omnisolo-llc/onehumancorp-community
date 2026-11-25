use crate::db::{DB, DbStore};
use crate::hub::Hub;
use crate::settings::Store as SettingsStore;
use ::server_common::{Claims, auth_utils::signed_tenant_id};
use axum::{
    Json, Router,
    extract::{Extension, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::sync::Arc;

pub async fn get_meetings_http_handler(
    State(hub): State<Arc<Hub>>,
    Extension(claims): Extension<Claims>,
) -> Response {
    let Some(tenant_id) = signed_tenant_id(&claims) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "authentication required" })),
        )
            .into_response();
    };
    let meetings = hub.get_meetings_by_org(&tenant_id).await;
    Json(meetings.as_ref().clone()).into_response()
}

pub async fn get_inbox_messages_handler(
    State(db): State<Arc<DB>>,
    Extension(user): Extension<Claims>,
) -> Response {
    let Some(org_id) = signed_tenant_id(&user) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "authentication required" })),
        )
            .into_response();
    };

    match &db.store {
        DbStore::Postgres => {
            let mut tx = match db.pool.begin().await {
                Ok(t) => t,
                Err(e) => {
                    ::server_telemetry::record_error_signal("[bug] Failed to begin transaction");
                    tracing::error!("Failed to begin transaction: {}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!([])),
                    )
                        .into_response();
                }
            };
            if let Err(e) = ::server_common::auth_utils::set_org_context(&mut *tx, &org_id).await {
                ::server_telemetry::record_error_signal("[bug] Failed to set org context");
                tracing::error!("Failed to set org context: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!([])),
                )
                    .into_response();
            }
            match sqlx::query(
                "SELECT id, tenant_id, source, content,
                    COALESCE(original_content, content) AS original_content,
                    COALESCE(translated_from_language, '') AS translated_from_language,
                    draft_reply, status, created_at
                 FROM inbox_messages
                 WHERE tenant_id = $1
                 ORDER BY created_at DESC",
            )
            .bind(&org_id)
            .fetch_all(&mut *tx)
            .await
            {
                Ok(rows) => {
                    let _ = tx.commit().await;
                    let messages: Vec<serde_json::Value> = rows
                        .into_iter()
                        .map(|row| {
                            let created_at: Option<chrono::NaiveDateTime> =
                                row.try_get("created_at").ok().flatten();
                            let created_at_str = created_at
                                .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
                                .unwrap_or_default();
                            serde_json::json!({
                                "id": row.get::<String, _>("id"),
                                "tenant_id": row.get::<String, _>("tenant_id"),
                                "source": row.get::<String, _>("source"),
                                "content": row.get::<String, _>("content"),
                                "original_message": row.get::<String, _>("original_content"),
                                "translated_from_language": row.get::<String, _>("translated_from_language"),
                                "generated_response": row.get::<String, _>("draft_reply"),
                                "status": row.get::<String, _>("status"),
                                "created_at": created_at_str,
                            })
                        })
                        .collect();
                    (StatusCode::OK, Json(messages)).into_response()
                }
                Err(e) => {
                    ::server_telemetry::record_error_signal("[bug] Failed to fetch inbox messages");
                    tracing::error!("Failed to fetch inbox messages: {}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!([])),
                    )
                        .into_response()
                }
            }
        }
        DbStore::Sqlite(pool) => match sqlx::query(
            "SELECT id, tenant_id, source, content,
                COALESCE(original_content, content) AS original_content,
                COALESCE(translated_from_language, '') AS translated_from_language,
                draft_reply, status, created_at
             FROM inbox_messages
             WHERE tenant_id = ?
             ORDER BY created_at DESC",
        )
        .bind(&org_id)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => {
                let messages: Vec<serde_json::Value> = rows
                    .into_iter()
                    .map(|row| {
                        serde_json::json!({
                            "id": row.get::<String, _>("id"),
                            "tenant_id": row.get::<String, _>("tenant_id"),
                            "source": row.get::<String, _>("source"),
                            "content": row.get::<String, _>("content"),
                            "original_message": row.get::<String, _>("original_content"),
                            "translated_from_language": row.get::<String, _>("translated_from_language"),
                            "generated_response": row.get::<String, _>("draft_reply"),
                            "status": row.get::<String, _>("status"),
                            "created_at": row.get::<String, _>("created_at"),
                        })
                    })
                    .collect();
                (StatusCode::OK, Json(messages)).into_response()
            }
            Err(e) => {
                ::server_telemetry::record_error_signal("[bug] Failed to fetch inbox messages");
                tracing::error!("Failed to fetch inbox messages: {}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!([])),
                )
                    .into_response()
            }
        },
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TenantDeliverySettings {
    pub delivery_enabled: bool,
    pub delivery_radius: Option<f64>,
    pub delivery_fee: Option<f64>,
}

#[derive(Clone)]
struct DeliverySettingsState {
    settings_store: Arc<SettingsStore>,
    isolated_scopes: bool,
}

fn delivery_settings_key(tenant_id: &str) -> String {
    format!("delivery_settings:{tenant_id}")
}

pub fn authorize_delivery_admin(claims: &Claims) -> Result<String, (StatusCode, &'static str)> {
    let Some(tenant_id) = signed_tenant_id(claims) else {
        return Err((StatusCode::UNAUTHORIZED, "tenant_required"));
    };
    let is_privileged = claims
        .roles
        .iter()
        .any(|role| role.eq_ignore_ascii_case("ADMIN") || role.eq_ignore_ascii_case("OWNER"));
    if !is_privileged {
        return Err((StatusCode::FORBIDDEN, "admin_or_owner_required"));
    }
    Ok(tenant_id)
}

async fn get_delivery_settings_handler(
    State(state): State<DeliverySettingsState>,
    Extension(claims): Extension<Claims>,
) -> Response {
    let tenant_id = match authorize_delivery_admin(&claims) {
        Ok(id) => id,
        Err((status, reason)) => {
            return (status, Json(serde_json::json!({ "error": reason }))).into_response();
        }
    };
    let settings = state.settings_store.get();
    let resolved = settings
        .extras
        .get(&delivery_settings_key(&tenant_id))
        .and_then(|raw| serde_json::from_str::<TenantDeliverySettings>(raw).ok())
        .unwrap_or(if state.isolated_scopes {
            TenantDeliverySettings {
                delivery_enabled: false,
                delivery_radius: None,
                delivery_fee: None,
            }
        } else {
            TenantDeliverySettings {
                delivery_enabled: settings.delivery_enabled,
                delivery_radius: settings.delivery_radius,
                delivery_fee: settings.delivery_fee,
            }
        });
    Json(resolved).into_response()
}

async fn update_delivery_settings_handler(
    State(state): State<DeliverySettingsState>,
    Extension(claims): Extension<Claims>,
    Json(req): Json<serde_json::Value>,
) -> Response {
    let tenant_id = match authorize_delivery_admin(&claims) {
        Ok(id) => id,
        Err((status, reason)) => {
            return (
                status,
                Json(serde_json::json!({ "success": false, "error": reason })),
            )
                .into_response();
        }
    };
    let enabled = req
        .get("delivery_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let radius = req.get("delivery_radius").and_then(|v| v.as_f64());
    let fee = req.get("delivery_fee").and_then(|v| v.as_f64());
    if radius.is_some_and(|v| !v.is_finite() || v < 0.0)
        || fee.is_some_and(|v| !v.is_finite() || v < 0.0)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "success": false, "error": "invalid_delivery_parameters" })),
        )
            .into_response();
    }

    let payload = TenantDeliverySettings {
        delivery_enabled: enabled,
        delivery_radius: radius,
        delivery_fee: fee,
    };
    let serialized = match serde_json::to_string(&payload) {
        Ok(s) => s,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "success": false })),
            )
                .into_response();
        }
    };
    if let Err(e) = state
        .settings_store
        .set_extra(delivery_settings_key(&tenant_id), serialized)
    {
        ::server_telemetry::record_error_signal("[bug] Failed to save delivery settings");
        tracing::error!("Failed to save tenant delivery settings: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "success": false })),
        )
            .into_response();
    }
    if !state.isolated_scopes
        && let Err(e) = state
            .settings_store
            .set_delivery_settings(enabled, radius, fee)
    {
        ::server_telemetry::record_error_signal("[bug] Failed to save delivery settings");
        tracing::error!("Failed to save standalone delivery settings: {}", e);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "success": false })),
        )
            .into_response();
    }
    Json(serde_json::json!({ "success": true })).into_response()
}

pub fn delivery_settings_router<S: Clone + Send + Sync + 'static>(
    settings_store: Arc<SettingsStore>,
    auth_store: Arc<::server_auth::Store>,
) -> Router<S> {
    delivery_settings_router_with_policy(settings_store, false).route_layer(
        axum::middleware::from_fn_with_state(
            auth_store,
            ::server_auth::strict_bearer_auth_middleware,
        ),
    )
}

pub fn delivery_settings_router_with_policy<S: Clone + Send + Sync + 'static>(
    settings_store: Arc<SettingsStore>,
    isolated_scopes: bool,
) -> Router<S> {
    Router::new()
        .route(
            "/api/v1/settings/delivery",
            get(get_delivery_settings_handler).post(update_delivery_settings_handler),
        )
        .with_state(DeliverySettingsState {
            settings_store,
            isolated_scopes,
        })
}
