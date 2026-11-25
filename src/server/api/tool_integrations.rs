use crate::db::{DB, DbStore};
use ::server_harness::mcp::task_center_server::{
    CreateSpaceInput, ERR_PAYLOAD_TOO_LARGE, GetTaskArgs, ListApprovalsArgs, ListSpacesArgs,
    ListTasksArgs, MCP_PROTOCOL_VERSION, MCP_WRITE_OPT_IN_HEADER, McpAccessScope, McpError,
    McpResourceDescriptor, McpToolDescriptor, ProposeEmailDraftInput, ProposeTaskInput,
    TaskCenterMcpBackend, TaskCenterMcpServer, compute_payload_sha256, redact_secrets_in_str,
    resource_catalog, tool_catalog,
};
use ::server_harness::middleware::{
    connection_vault::{ConnectionInfo, ConnectionVault},
    usage_ledger::{LedgerError, UsageLedger},
};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Arc;

const CACHE_CONTROL_PRIVATE_NO_STORE: &str = "private, no-store";

fn usage_ledger(db: &DB) -> UsageLedger {
    match &db.store {
        DbStore::Postgres => UsageLedger::Postgres(db.pool.clone()),
        DbStore::Sqlite(pool) => UsageLedger::Sqlite(pool.clone()),
    }
}

pub fn connection_vault(db: &DB) -> Result<ConnectionVault, String> {
    ConnectionVault::from_environment(usage_ledger(db)).map_err(|error| error.to_string())
}

pub async fn stripe_key_for_tenant(db: &DB, tenant: &str) -> Result<String, String> {
    if std::env::var_os("OMNISOLO_CONNECTION_KEYS").is_some() {
        // A missing/revoked tenant key must never fall back to another payer.
        return connection_vault(db)?
            .read_key(tenant, "stripe")
            .await
            .map(|key| key.to_string())
            .map_err(|error| error.to_string());
    }
    if crate::is_standalone_runtime() {
        return std::env::var("STRIPE_API_KEY")
            .map_err(|_| "Payment connection is not configured".into());
    }
    Err("A verified tenant payment connection is required".into())
}

#[derive(Clone)]
pub struct ToolIntegrationsApiState {
    pub db: Arc<DB>,
    #[cfg(test)]
    test_vault: Option<ConnectionVault>,
}

impl ToolIntegrationsApiState {
    fn resolve_vault(&self) -> Result<ConnectionVault, String> {
        #[cfg(test)]
        if let Some(vault) = &self.test_vault {
            return Ok(vault.clone());
        }
        connection_vault(&self.db)
    }
}

fn private_json<T: Serialize>(status: StatusCode, body: T) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, CACHE_CONTROL_PRIVATE_NO_STORE)],
        Json(body),
    )
        .into_response()
}

fn is_owner_or_admin(user: &::server_common::Claims) -> bool {
    user.roles
        .iter()
        .any(|role| role.eq_ignore_ascii_case("owner") || role.eq_ignore_ascii_case("admin"))
}

fn require_tenant(user: &::server_common::Claims) -> Option<String> {
    user.organization_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectIntegrationRequest {
    pub bot_token: Option<String>,
    pub api_token: Option<String>,
    pub from_phone: Option<String>,
    pub integration_id: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Serialize)]
pub struct ConnectIntegrationResponse {
    pub success: bool,
    pub message: String,
    pub status: String,
    pub usable: bool,
}

fn connection_response(
    status_code: StatusCode,
    success: bool,
    message: &str,
    status: &str,
    usable: bool,
) -> Response {
    private_json(
        status_code,
        ConnectIntegrationResponse {
            success,
            message: message.to_string(),
            status: status.to_string(),
            usable,
        },
    )
}

struct ValidatedConnectIntegration {
    integration_id: String,
    bot_token: Option<String>,
    api_token: Option<String>,
    from_phone: Option<String>,
}

fn bounded_credential(value: Option<String>, maximum: usize) -> Result<Option<String>, ()> {
    match value {
        None => Ok(None),
        Some(value) => {
            let value = value.trim();
            if value.is_empty() {
                Ok(None)
            } else if value.len() <= maximum && !value.contains('\0') {
                Ok(Some(value.to_string()))
            } else {
                Err(())
            }
        }
    }
}

fn safe_integration_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn provider_credentials_present(
    integration_id: &str,
    bot_token: Option<&str>,
    api_token: Option<&str>,
) -> bool {
    let has_bot_token = bot_token.is_some_and(|value| !value.trim().is_empty());
    let has_api_token = api_token.is_some_and(|value| !value.trim().is_empty());
    match integration_id {
        "twilio" | "whatsapp" => has_bot_token && has_api_token,
        "whatsapp_cloud_api" => has_api_token,
        _ => has_bot_token || has_api_token,
    }
}

fn validate_connect_request(
    path_id: &str,
    payload: ConnectIntegrationRequest,
) -> Result<ValidatedConnectIntegration, ()> {
    if !safe_integration_id(path_id) {
        return Err(());
    }
    let bot_token = bounded_credential(payload.bot_token, 4096)?;
    let api_token = bounded_credential(payload.api_token, 4096)?;
    let from_phone = bounded_credential(payload.from_phone, 128)?;
    if !provider_credentials_present(path_id, bot_token.as_deref(), api_token.as_deref()) {
        return Err(());
    }
    Ok(ValidatedConnectIntegration {
        integration_id: path_id.to_string(),
        bot_token,
        api_token,
        from_phone,
    })
}

pub async fn connect_integration_handler(
    State(state): State<ToolIntegrationsApiState>,
    axum::extract::Extension(user): axum::extract::Extension<::server_common::Claims>,
    Path(id): Path<String>,
    Json(payload): Json<ConnectIntegrationRequest>,
) -> impl IntoResponse {
    if !is_owner_or_admin(&user) {
        return connection_response(
            StatusCode::FORBIDDEN,
            false,
            "Owner approval is required",
            "unavailable",
            false,
        );
    }
    let Some(tenant_id) = require_tenant(&user) else {
        return connection_response(
            StatusCode::UNAUTHORIZED,
            false,
            "Authenticated organization required",
            "unavailable",
            false,
        );
    };
    let Ok(validated) = validate_connect_request(&id, payload) else {
        return connection_response(
            StatusCode::BAD_REQUEST,
            false,
            "Valid provider credentials are required",
            "unavailable",
            false,
        );
    };
    if ::server_harness::middleware::connection_vault::supported(validated.integration_id.as_str())
    {
        let Some(secret) = validated.api_token.as_deref() else {
            return connection_response(
                StatusCode::BAD_REQUEST,
                false,
                "An API key or valid OAuth token is required; subscription tokens are not accepted",
                "unavailable",
                false,
            );
        };
        let Ok(vault) = state.resolve_vault() else {
            return connection_response(
                StatusCode::SERVICE_UNAVAILABLE,
                false,
                "Connection encryption is not configured",
                "unavailable",
                false,
            );
        };
        if vault.initialize().await.is_err() {
            return connection_response(
                StatusCode::SERVICE_UNAVAILABLE,
                false,
                "Connection storage is unavailable",
                "unavailable",
                false,
            );
        }
        return match vault
            .verify_and_store(&tenant_id, &validated.integration_id, secret)
            .await
        {
            Ok(_) => connection_response(
                StatusCode::OK,
                true,
                "Provider credential verified and encrypted; available to supported tenant-scoped routes",
                "verified",
                true,
            ),
            Err(LedgerError::Invalid) => connection_response(
                StatusCode::BAD_REQUEST,
                false,
                "A valid provider API key or OAuth bearer token is required; consumer subscription tokens are not accepted",
                "unavailable",
                false,
            ),
            Err(LedgerError::Conflict) => connection_response(
                StatusCode::CONFLICT,
                false,
                "Connection state changed concurrently; retry verification",
                "unavailable",
                false,
            ),
            Err(_) => connection_response(
                StatusCode::BAD_GATEWAY,
                false,
                "Provider verification failed; no connection was stored",
                "unavailable",
                false,
            ),
        };
    }
    tracing::info!(
        integration_id = %validated.integration_id,
        bot_token_supplied = validated.bot_token.is_some(),
        api_token_supplied = validated.api_token.is_some(),
        from_phone_supplied = validated.from_phone.is_some(),
        "Rejected integration credential storage because provider verification is unavailable"
    );
    connection_response(
        StatusCode::NOT_IMPLEMENTED,
        false,
        "Secure provider verification is not configured",
        "unavailable",
        false,
    )
}

#[derive(Serialize)]
pub struct IntegrationInfo {
    pub id: String,
    pub status: String,
    pub usable: bool,
}

#[derive(Serialize)]
pub struct GetIntegrationsResponse {
    pub success: bool,
    pub integrations: Vec<IntegrationInfo>,
    pub message: Option<String>,
}

pub async fn get_integrations_handler(
    State(state): State<ToolIntegrationsApiState>,
    axum::extract::Extension(user): axum::extract::Extension<::server_common::Claims>,
) -> impl IntoResponse {
    let Some(tenant_id) = require_tenant(&user) else {
        return private_json(
            StatusCode::UNAUTHORIZED,
            GetIntegrationsResponse {
                success: false,
                integrations: vec![],
                message: Some("Authenticated organization required".to_string()),
            },
        );
    };
    let rows = match &state.db.store {
        DbStore::Postgres => {
            sqlx::query_as::<_, (String, String)>(
                "SELECT id, status FROM tool_integrations WHERE tenant_id = $1",
            )
            .bind(&tenant_id)
            .fetch_all(&state.db.pool)
            .await
        }
        DbStore::Sqlite(pool) => {
            sqlx::query_as::<_, (String, String)>(
                "SELECT id, status FROM tool_integrations WHERE tenant_id = ?",
            )
            .bind(&tenant_id)
            .fetch_all(pool)
            .await
        }
    };
    let Ok(rows) = rows else {
        return private_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            GetIntegrationsResponse {
                success: false,
                integrations: vec![],
                message: Some("Integration storage is unavailable".to_string()),
            },
        );
    };
    let mut integrations: Vec<IntegrationInfo> = rows
        .into_iter()
        .map(|(id, status)| IntegrationInfo {
            status: if status == "connected" {
                "verification_required".to_string()
            } else {
                status
            },
            usable: false,
            id,
        })
        .collect();
    if let Ok(vault) = state.resolve_vault()
        && let Ok(verified) = vault.list_with_expiry_status(&tenant_id).await
    {
        for connection in verified {
            integrations.retain(|entry| entry.id != connection.provider);
            integrations.push(IntegrationInfo {
                id: connection.provider,
                usable: connection.state == "verified",
                status: connection.state,
            });
        }
    }
    private_json(
        StatusCode::OK,
        GetIntegrationsResponse {
            success: true,
            integrations,
            message: None,
        },
    )
}

async fn refresh_integration_handler(
    State(state): State<ToolIntegrationsApiState>,
    axum::extract::Extension(user): axum::extract::Extension<::server_common::Claims>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if !is_owner_or_admin(&user) {
        return connection_response(
            StatusCode::FORBIDDEN,
            false,
            "Owner approval is required",
            "unavailable",
            false,
        );
    }
    let Some(tenant) = require_tenant(&user) else {
        return connection_response(
            StatusCode::UNAUTHORIZED,
            false,
            "Authenticated organization required",
            "unavailable",
            false,
        );
    };
    if !::server_harness::middleware::connection_vault::supported(id.as_str()) {
        return connection_response(
            StatusCode::NOT_IMPLEMENTED,
            false,
            "Provider revalidation is not supported",
            "unavailable",
            false,
        );
    }
    let Ok(vault) = state.resolve_vault() else {
        return connection_response(
            StatusCode::SERVICE_UNAVAILABLE,
            false,
            "Connection encryption is not configured",
            "unavailable",
            false,
        );
    };
    match vault.refresh(&tenant, &id).await {
        Ok(_) => connection_response(
            StatusCode::OK,
            true,
            "Connection reverified with the provider",
            "verified",
            true,
        ),
        Err(_) => connection_response(
            StatusCode::BAD_GATEWAY,
            false,
            "Connection could not be reverified; no new authorization was granted",
            "verification_required",
            false,
        ),
    }
}

async fn revoke_integration_handler(
    State(state): State<ToolIntegrationsApiState>,
    axum::extract::Extension(user): axum::extract::Extension<::server_common::Claims>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if !is_owner_or_admin(&user) {
        return connection_response(
            StatusCode::FORBIDDEN,
            false,
            "Owner approval is required",
            "unavailable",
            false,
        );
    }
    let Some(tenant) = require_tenant(&user) else {
        return connection_response(
            StatusCode::UNAUTHORIZED,
            false,
            "Authenticated organization required",
            "unavailable",
            false,
        );
    };
    let Ok(vault) = state.resolve_vault() else {
        return connection_response(
            StatusCode::SERVICE_UNAVAILABLE,
            false,
            "Connection vault unavailable",
            "unavailable",
            false,
        );
    };
    match vault.revoke(&tenant, &id).await {
        Ok(()) => connection_response(
            StatusCode::OK,
            true,
            "Connection revoked for new requests; accepted provider requests may still finish",
            "revoked",
            false,
        ),
        Err(_) => connection_response(
            StatusCode::SERVICE_UNAVAILABLE,
            false,
            "Unable to revoke connection",
            "unavailable",
            false,
        ),
    }
}

// ── Connector Catalog (`/api/v1/integrations/catalog`) ─────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ConnectorCatalogItem {
    pub id: String,
    pub name: String,
    pub category: String,
    pub auth_mode: String,
    pub read_scopes: Vec<String>,
    pub write_scopes: Vec<String>,
    pub status: String,
    pub usable: bool,
    pub verified_at: Option<i64>,
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpServerCatalogInfo {
    pub endpoint: String,
    pub protocol_version: String,
    pub default_mode: String,
    pub write_opt_in_header: String,
    pub tool_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectorCatalogResponse {
    pub success: bool,
    pub connectors: Vec<ConnectorCatalogItem>,
    pub mcp_server: McpServerCatalogInfo,
}

struct ConnectorDefinition {
    id: &'static str,
    name: &'static str,
    category: &'static str,
    auth_mode: &'static str,
    read_scopes: &'static [&'static str],
    write_scopes: &'static [&'static str],
}

const CONNECTOR_CATALOG_DEFS: &[ConnectorDefinition] = &[
    ConnectorDefinition {
        id: "openai_api",
        name: "OpenAI API",
        category: "ai_provider",
        auth_mode: "api_key",
        read_scopes: &["models:read"],
        write_scopes: &["responses:create", "chat_completions:create"],
    },
    ConnectorDefinition {
        id: "anthropic_api",
        name: "Anthropic API",
        category: "ai_provider",
        auth_mode: "api_key",
        read_scopes: &["models:read"],
        write_scopes: &["messages:create"],
    },
    ConnectorDefinition {
        id: "stripe",
        name: "Stripe",
        category: "payments",
        auth_mode: "api_key",
        read_scopes: &["balance:read", "invoices:read"],
        write_scopes: &["payment_links:create", "invoices:create"],
    },
    ConnectorDefinition {
        id: "resend",
        name: "Resend",
        category: "email",
        auth_mode: "api_key",
        read_scopes: &["domains:read"],
        write_scopes: &["emails:send"],
    },
    ConnectorDefinition {
        id: "google_workspace",
        name: "Google Workspace",
        category: "workspace",
        auth_mode: "oauth_bearer",
        read_scopes: &[
            "drive.metadata.readonly",
            "gmail.readonly",
            "spreadsheets.readonly",
        ],
        write_scopes: &["drive.file", "gmail.send"],
    },
];

fn normalize_connector_status(raw: &str) -> &'static str {
    match raw {
        "verified" => "verified",
        "revoked" => "revoked",
        _ => "verification_required",
    }
}

async fn build_connector_catalog_items(
    vault: Result<ConnectionVault, String>,
    tenant_id: &str,
) -> Vec<ConnectorCatalogItem> {
    let mut by_provider: HashMap<String, ConnectionInfo> = HashMap::new();
    if let Ok(vault) = vault
        && let Ok(rows) = vault.list_with_expiry_status(tenant_id).await
    {
        for row in rows {
            by_provider.insert(row.provider.clone(), row);
        }
    }
    CONNECTOR_CATALOG_DEFS
        .iter()
        .map(|def| {
            let (status, usable, verified_at, expires_at) = match by_provider.get(def.id) {
                Some(info) => {
                    let s = normalize_connector_status(&info.state);
                    let (v, e) = if s == "revoked" {
                        (None, None)
                    } else {
                        (Some(info.verified_at), Some(info.expires_at))
                    };
                    (s.to_string(), s == "verified", v, e)
                }
                None => ("not_connected".to_string(), false, None, None),
            };
            ConnectorCatalogItem {
                id: def.id.into(),
                name: def.name.into(),
                category: def.category.into(),
                auth_mode: def.auth_mode.into(),
                read_scopes: def.read_scopes.iter().map(|s| (*s).into()).collect(),
                write_scopes: def.write_scopes.iter().map(|s| (*s).into()).collect(),
                status,
                usable,
                verified_at,
                expires_at,
            }
        })
        .collect()
}

pub async fn get_catalog_handler(
    State(state): State<ToolIntegrationsApiState>,
    axum::extract::Extension(user): axum::extract::Extension<::server_common::Claims>,
) -> impl IntoResponse {
    let Some(tenant_id) = require_tenant(&user) else {
        return private_json(
            StatusCode::UNAUTHORIZED,
            json!({"success": false, "message": "Authenticated organization required"}),
        );
    };
    let connectors = build_connector_catalog_items(state.resolve_vault(), &tenant_id).await;
    private_json(
        StatusCode::OK,
        ConnectorCatalogResponse {
            success: true,
            connectors,
            mcp_server: McpServerCatalogInfo {
                endpoint: "/api/v1/integrations/mcp".into(),
                protocol_version: MCP_PROTOCOL_VERSION.into(),
                default_mode: "read_only".into(),
                write_opt_in_header: MCP_WRITE_OPT_IN_HEADER.into(),
                tool_count: tool_catalog().len(),
            },
        },
    )
}

// ── Resend (`POST /api/v1/integrations/resend/send`) ───────────────────────

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResendSendRequest {
    pub to: String,
    pub from: String,
    pub subject: String,
    pub html_body: String,
}

#[derive(Serialize)]
pub struct ResendSendResponse {
    pub success: bool,
    pub provider: String,
    pub status: String,
    pub message: String,
}

fn valid_single_email(value: &str) -> bool {
    let t = value.trim();
    if t.len() < 3
        || t.len() > 254
        || t.chars().any(|c| c.is_control() || c.is_whitespace())
        || t.contains(',')
        || t.contains(';')
    {
        return false;
    }
    let mut parts = t.split('@');
    let (local, domain) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    !local.is_empty()
        && domain.len() >= 3
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && parts.next().is_none()
}

pub async fn resend_send_handler(
    State(state): State<ToolIntegrationsApiState>,
    axum::extract::Extension(user): axum::extract::Extension<::server_common::Claims>,
    Json(payload): Json<ResendSendRequest>,
) -> impl IntoResponse {
    let reply = |code, ok, st: &str, msg: String| {
        private_json(
            code,
            ResendSendResponse {
                success: ok,
                provider: "resend".into(),
                status: st.into(),
                message: msg,
            },
        )
    };
    if !is_owner_or_admin(&user) {
        return reply(
            StatusCode::FORBIDDEN,
            false,
            "forbidden",
            "Owner or admin authorization is required to send email".into(),
        );
    }
    let Some(tenant_id) = require_tenant(&user) else {
        return reply(
            StatusCode::UNAUTHORIZED,
            false,
            "unauthorized",
            "Authenticated organization required".into(),
        );
    };
    let (to, from, subject, html_body) = (
        payload.to.trim(),
        payload.from.trim(),
        payload.subject.trim(),
        payload.html_body.trim(),
    );
    if !valid_single_email(to)
        || !valid_single_email(from)
        || subject.is_empty()
        || subject.len() > 200
        || subject.chars().any(char::is_control)
        || html_body.is_empty()
        || html_body.len() > 64 * 1024
        || html_body.contains('\0')
    {
        return reply(
            StatusCode::BAD_REQUEST,
            false,
            "invalid_request",
            "Valid to, from, subject, and html_body fields are required".into(),
        );
    }
    let Ok(vault) = state.resolve_vault() else {
        return reply(
            StatusCode::PRECONDITION_FAILED,
            false,
            "verification_required",
            "A verified tenant Resend connection is required".into(),
        );
    };
    let Ok(api_key) = vault.read_key(&tenant_id, "resend").await else {
        return reply(StatusCode::PRECONDITION_FAILED, false, "verification_required", "A verified tenant Resend connection is required; connection is missing, expired, or revoked".into());
    };
    let provider = ::server_integrations_resend::provider::ResendProvider::new(api_key.to_string());
    match provider.send_email(to, from, subject, html_body).await {
        Ok(()) => reply(
            StatusCode::OK,
            true,
            "sent",
            "Email accepted by Resend".into(),
        ),
        Err(err) => {
            let safe = redact_secrets_in_str(&err.replace(api_key.as_str(), "[REDACTED]"));
            tracing::warn!(tenant_id = %tenant_id, error = %safe, "Resend email send failed");
            reply(
                StatusCode::BAD_GATEWAY,
                false,
                "failed",
                format!("Resend delivery failed: {safe}"),
            )
        }
    }
}

// ── Google Workspace (`GET /api/v1/integrations/google-workspace/files`) ───

fn default_drive_folder() -> String {
    "root".into()
}
fn default_drive_page_size() -> u32 {
    20
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleWorkspaceFilesQuery {
    #[serde(default = "default_drive_folder")]
    pub folder_id: String,
    #[serde(default = "default_drive_page_size")]
    pub page_size: u32,
}

#[derive(Serialize)]
pub struct GoogleWorkspaceFilesResponse {
    pub success: bool,
    pub provider: String,
    pub status: String,
    pub folder_id: String,
    pub files: Vec<::server_integrations_google_workspace::client::DriveFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

fn valid_drive_folder_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

pub async fn google_workspace_files_handler(
    State(state): State<ToolIntegrationsApiState>,
    axum::extract::Extension(user): axum::extract::Extension<::server_common::Claims>,
    Query(query): Query<GoogleWorkspaceFilesQuery>,
) -> impl IntoResponse {
    let err_reply = |code, st: &str, folder: &str, msg: String| {
        private_json(
            code,
            GoogleWorkspaceFilesResponse {
                success: false,
                provider: "google_workspace".into(),
                status: st.into(),
                folder_id: folder.into(),
                files: vec![],
                message: Some(msg),
            },
        )
    };
    let Some(tenant_id) = require_tenant(&user) else {
        return err_reply(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "",
            "Authenticated organization required".into(),
        );
    };
    let folder_id = query.folder_id.trim();
    if !valid_drive_folder_id(folder_id) {
        return err_reply(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "",
            "Invalid Google Drive folder_id".into(),
        );
    }
    let page_size = query.page_size.clamp(1, 50);
    let Ok(vault) = state.resolve_vault() else {
        return err_reply(
            StatusCode::PRECONDITION_FAILED,
            "verification_required",
            folder_id,
            "A verified tenant Google Workspace connection is required".into(),
        );
    };
    let Ok(access_token) = vault.read_key(&tenant_id, "google_workspace").await else {
        return err_reply(StatusCode::PRECONDITION_FAILED, "verification_required", folder_id, "A verified tenant Google Workspace connection is required; connection is missing, expired, or revoked".into());
    };
    let provider = ::server_integrations_google_workspace::provider::GoogleWorkspaceProvider::new(
        access_token.to_string(),
    );
    match provider.drive_list_files(folder_id, page_size).await {
        Ok(files) => private_json(
            StatusCode::OK,
            GoogleWorkspaceFilesResponse {
                success: true,
                provider: "google_workspace".into(),
                status: "verified".into(),
                folder_id: folder_id.into(),
                files,
                message: None,
            },
        ),
        Err(err) => {
            let safe = redact_secrets_in_str(&err.replace(access_token.as_str(), "[REDACTED]"));
            tracing::warn!(tenant_id = %tenant_id, error = %safe, "Google Workspace Drive list_files failed");
            err_reply(
                StatusCode::BAD_GATEWAY,
                "failed",
                folder_id,
                format!("Google Workspace request failed: {safe}"),
            )
        }
    }
}

// ── Inbound Task Center MCP Server (`/api/v1/integrations/mcp`) ────────────

fn is_missing_table_error(error: &sqlx::Error) -> bool {
    if error.as_database_error().and_then(|d| d.code()).as_deref() == Some("42P01") {
        return true;
    }
    let msg = error.to_string().to_ascii_lowercase();
    msg.contains("no such table") || msg.contains("does not exist")
}

fn parse_vec(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

macro_rules! tenant_tx {
    ($db:expr, $tenant:expr, $tx:ident, $body:block) => {{
        match &$db.store {
            DbStore::Postgres => {
                let mut $tx = $db
                    .pool
                    .begin()
                    .await
                    .map_err(|_| McpError::internal("Task Center storage unavailable"))?;
                sqlx::query("SELECT set_config('app.current_tenant', $1, true)")
                    .bind($tenant)
                    .execute(&mut *$tx)
                    .await
                    .map_err(|_| McpError::internal("Failed to bind tenant RLS context"))?;
                $body
            }
            DbStore::Sqlite(pool) => {
                let mut $tx = pool
                    .begin()
                    .await
                    .map_err(|_| McpError::internal("Task Center storage unavailable"))?;
                $body
            }
        }
    }};
}

fn space_row_json<'r, R: Row>(r: &'r R) -> Value
where
    &'static str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    bool: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    json!({
        "id": r.get::<String, _>("id"),
        "name": r.get::<String, _>("name"),
        "description": r.get::<String, _>("description"),
        "kind": r.get::<String, _>("kind"),
        "prompt_template": r.get::<String, _>("prompt_template"),
        "department": r.get::<String, _>("department"),
        "reads": parse_vec(&r.get::<String, _>("reads_json")),
        "shows": parse_vec(&r.get::<String, _>("shows_json")),
        "cannot": parse_vec(&r.get::<String, _>("cannot_json")),
        "source_health": r.get::<String, _>("source_health"),
        "pinned_artifact_ids": parse_vec(&r.get::<String, _>("pinned_artifact_ids_json")),
        "archived": r.get::<bool, _>("archived"),
        "created_at_unix": r.get::<i64, _>("created_at_unix"),
        "updated_at_unix": r.get::<i64, _>("updated_at_unix"),
    })
}

fn task_row_json<'r, R: Row>(r: &'r R) -> Value
where
    &'static str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<String>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    bool: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<i64>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    json!({
        "id": r.get::<String, _>("id"),
        "space_id": r.get::<Option<String>, _>("space_id"),
        "title": r.get::<String, _>("title"),
        "prompt": r.get::<String, _>("prompt"),
        "department": r.get::<String, _>("department"),
        "status": r.get::<String, _>("status"),
        "provenance": r.get::<String, _>("provenance"),
        "attention": r.get::<String, _>("attention"),
        "risk": r.get::<String, _>("risk"),
        "payer_mode": r.get::<String, _>("payer_mode"),
        "estimated_cost_micros": r.get::<i64, _>("estimated_cost_micros"),
        "actual_cost_micros": r.get::<Option<i64>, _>("actual_cost_micros"),
        "idempotency_key": r.get::<String, _>("idempotency_key"),
        "receipt_id": r.get::<Option<String>, _>("receipt_id"),
        "output_summary": r.get::<Option<String>, _>("output_summary"),
        "error_message": r.get::<Option<String>, _>("error_message"),
        "archived": r.get::<bool, _>("archived"),
        "created_at_unix": r.get::<i64, _>("created_at_unix"),
        "updated_at_unix": r.get::<i64, _>("updated_at_unix"),
    })
}

fn approval_row_json<'r, R: Row>(r: &'r R) -> Value
where
    &'static str: sqlx::ColumnIndex<R>,
    String: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<String>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    i64: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
    Option<i64>: sqlx::Decode<'r, R::Database> + sqlx::Type<R::Database>,
{
    let preview: Value = serde_json::from_str(&r.get::<String, _>("payload_preview_json"))
        .unwrap_or_else(|_| json!({}));
    json!({
        "approval_id": r.get::<String, _>("approval_id"),
        "task_id": r.get::<String, _>("task_id"),
        "action_kind": r.get::<String, _>("action_kind"),
        "department": r.get::<String, _>("department"),
        "risk": r.get::<String, _>("risk"),
        "summary": r.get::<String, _>("summary"),
        "payload_preview": preview,
        "payload_sha256": r.get::<String, _>("payload_sha256"),
        "estimated_cost_micros": r.get::<i64, _>("estimated_cost_micros"),
        "standing_limit_micros": r.get::<i64, _>("standing_limit_micros"),
        "status": r.get::<String, _>("status"),
        "decided_by": r.get::<Option<String>, _>("decided_by"),
        "decided_at_unix": r.get::<Option<i64>, _>("decided_at_unix"),
    })
}

pub struct DbTaskCenterMcpBackend {
    db: Arc<DB>,
    #[cfg(test)]
    test_vault: Option<ConnectionVault>,
}

struct ProposalInsert<'a> {
    task_id: &'a str,
    approval_id: &'a str,
    space_id: Option<&'a str>,
    title: &'a str,
    prompt: &'a str,
    department: &'a str,
    action_kind: &'a str,
    summary: &'a str,
    payload_preview: &'a Value,
    payload_sha256: &'a str,
    estimated_cost_micros: i64,
    idempotency_key: &'a str,
    now: i64,
}

impl DbTaskCenterMcpBackend {
    pub fn new(db: Arc<DB>) -> Self {
        Self {
            db,
            #[cfg(test)]
            test_vault: None,
        }
    }

    fn resolve_vault(&self) -> Result<ConnectionVault, String> {
        #[cfg(test)]
        if let Some(vault) = &self.test_vault {
            return Ok(vault.clone());
        }
        connection_vault(&self.db)
    }

    async fn insert_proposal(
        &self,
        scope: &McpAccessScope,
        p: &ProposalInsert<'_>,
    ) -> Result<bool, McpError> {
        let preview_json = serde_json::to_string(p.payload_preview)
            .map_err(|_| McpError::internal("Serialize failed"))?;
        let digest = compute_payload_sha256(&json!({
            "tenant_id": scope.tenant_id,
            "action_kind": p.action_kind,
            "idempotency_key": p.idempotency_key,
            "payload_sha256": p.payload_sha256,
        }));
        tenant_tx!(self.db, &scope.tenant_id, tx, {
            let task_res = sqlx::query(
                "INSERT INTO task_center_tasks (id,tenant_id,actor_id,space_id,title,prompt,department,requested_action,status,provenance,attention,risk,payer_mode,estimated_cost_micros,idempotency_key,request_digest,archived,created_at_unix,updated_at_unix)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'awaiting_approval','awaiting_approval','permission_required','draft_for_review','unconfigured',$9,$10,$11,$12,$13,$13)",
            )
            .bind(p.task_id)
            .bind(&scope.tenant_id)
            .bind(&scope.actor_id)
            .bind(p.space_id)
            .bind(p.title)
            .bind(p.prompt)
            .bind(p.department)
            .bind(p.action_kind)
            .bind(p.estimated_cost_micros)
            .bind(p.idempotency_key)
            .bind(&digest)
            .bind(false)
            .bind(p.now)
            .execute(&mut *tx)
            .await;
            if let Err(err) = task_res {
                let _ = tx.rollback().await;
                return if is_missing_table_error(&err) {
                    Ok(false)
                } else {
                    Err(McpError::internal("Failed to persist proposal"))
                };
            }
            let appr_res = sqlx::query(
                "INSERT INTO task_center_approvals (approval_id,tenant_id,task_id,action_kind,department,risk,summary,payload_preview_json,payload_sha256,estimated_cost_micros,standing_limit_micros,status,created_at_unix)
                 VALUES ($1,$2,$3,$4,$5,'draft_for_review',$6,$7,$8,$9,0,'pending',$10)",
            )
            .bind(p.approval_id)
            .bind(&scope.tenant_id)
            .bind(p.task_id)
            .bind(p.action_kind)
            .bind(p.department)
            .bind(p.summary)
            .bind(&preview_json)
            .bind(p.payload_sha256)
            .bind(p.estimated_cost_micros)
            .bind(p.now)
            .execute(&mut *tx)
            .await;
            if let Err(err) = appr_res {
                let _ = tx.rollback().await;
                return if is_missing_table_error(&err) {
                    Ok(false)
                } else {
                    Err(McpError::internal("Failed to persist approval"))
                };
            }
            tx.commit()
                .await
                .map_err(|_| McpError::internal("Failed to commit proposal"))?;
            Ok(true)
        })
    }
}

#[async_trait]
impl TaskCenterMcpBackend for DbTaskCenterMcpBackend {
    async fn list_spaces(
        &self,
        scope: &McpAccessScope,
        args: ListSpacesArgs,
    ) -> Result<Value, McpError> {
        let limit = i64::from(args.limit.unwrap_or(50).clamp(1, 100));
        tenant_tx!(self.db, &scope.tenant_id, tx, {
            match sqlx::query(
                "SELECT id,name,description,kind,prompt_template,department,reads_json,shows_json,cannot_json,source_health,pinned_artifact_ids_json,archived,created_at_unix,updated_at_unix
                 FROM task_center_spaces WHERE tenant_id = $1 AND ($2 OR archived = $3) ORDER BY updated_at_unix DESC LIMIT $4",
            )
            .bind(&scope.tenant_id)
            .bind(args.include_archived)
            .bind(false)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            {
                Ok(rows) => Ok(json!({ "spaces": rows.iter().map(space_row_json).collect::<Vec<_>>(), "schema_ready": true })),
                Err(err) if is_missing_table_error(&err) => Ok(json!({ "spaces": [], "schema_ready": false })),
                Err(_) => Err(McpError::internal("Failed to query spaces")),
            }
        })
    }

    async fn list_tasks(
        &self,
        scope: &McpAccessScope,
        args: ListTasksArgs,
    ) -> Result<Value, McpError> {
        let limit = i64::from(args.limit.unwrap_or(50).clamp(1, 100));
        tenant_tx!(self.db, &scope.tenant_id, tx, {
            match sqlx::query(
                "SELECT id,space_id,title,prompt,department,status,provenance,attention,risk,payer_mode,estimated_cost_micros,actual_cost_micros,idempotency_key,receipt_id,output_summary,error_message,archived,created_at_unix,updated_at_unix
                 FROM task_center_tasks WHERE tenant_id = $1 AND ($2 IS NULL OR space_id = $2) AND ($3 IS NULL OR status = $3) AND (NOT $4 OR attention <> 'none')
                 ORDER BY updated_at_unix DESC LIMIT $5",
            )
            .bind(&scope.tenant_id)
            .bind(args.space_id.as_deref())
            .bind(args.status.as_deref())
            .bind(args.attention_only)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            {
                Ok(rows) => Ok(json!({ "tasks": rows.iter().map(task_row_json).collect::<Vec<_>>(), "schema_ready": true })),
                Err(err) if is_missing_table_error(&err) => Ok(json!({ "tasks": [], "schema_ready": false })),
                Err(_) => Err(McpError::internal("Failed to query tasks")),
            }
        })
    }

    async fn get_task(&self, scope: &McpAccessScope, args: GetTaskArgs) -> Result<Value, McpError> {
        tenant_tx!(self.db, &scope.tenant_id, tx, {
            match sqlx::query(
                "SELECT id,space_id,title,prompt,department,status,provenance,attention,risk,payer_mode,estimated_cost_micros,actual_cost_micros,idempotency_key,receipt_id,output_summary,error_message,archived,created_at_unix,updated_at_unix
                 FROM task_center_tasks WHERE tenant_id = $1 AND id = $2",
            )
            .bind(&scope.tenant_id)
            .bind(&args.task_id)
            .fetch_optional(&mut *tx)
            .await
            {
                Ok(Some(row)) => Ok(task_row_json(&row)),
                Ok(None) => Err(McpError::not_found("Task not found")),
                Err(err) if is_missing_table_error(&err) => Err(McpError::not_found("Task not found")),
                Err(_) => Err(McpError::internal("Failed to fetch task")),
            }
        })
    }

    async fn list_approvals(
        &self,
        scope: &McpAccessScope,
        args: ListApprovalsArgs,
    ) -> Result<Value, McpError> {
        let limit = i64::from(args.limit.unwrap_or(50).clamp(1, 100));
        tenant_tx!(self.db, &scope.tenant_id, tx, {
            match sqlx::query(
                "SELECT approval_id,task_id,action_kind,department,risk,summary,payload_preview_json,payload_sha256,estimated_cost_micros,standing_limit_micros,status,decided_by,decided_at_unix
                 FROM task_center_approvals WHERE tenant_id = $1 AND ($2 IS NULL OR status = $2) ORDER BY created_at_unix DESC LIMIT $3",
            )
            .bind(&scope.tenant_id)
            .bind(args.status.as_deref())
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            {
                Ok(rows) => Ok(json!({ "approvals": rows.iter().map(approval_row_json).collect::<Vec<_>>(), "schema_ready": true })),
                Err(err) if is_missing_table_error(&err) => Ok(json!({ "approvals": [], "schema_ready": false })),
                Err(_) => Err(McpError::internal("Failed to query approvals")),
            }
        })
    }

    async fn get_usage_budget(&self, scope: &McpAccessScope) -> Result<Value, McpError> {
        match usage_ledger(&self.db).summary(&scope.tenant_id).await {
            Ok(s) => Ok(json!({
                "configured": true,
                "limit_micros": s.limit_micros,
                "spent_micros": s.spent_micros,
                "reserved_micros": s.reserved_micros,
                "currency": "USD",
                "unit": "micro_usd",
                "customer_direct_inference_is_not_rebilled": true
            })),
            Err(LedgerError::Unconfigured | LedgerError::Database) => Ok(json!({
                "configured": false,
                "limit_micros": 0,
                "spent_micros": 0,
                "reserved_micros": 0,
                "currency": "USD",
                "unit": "micro_usd",
                "customer_direct_inference_is_not_rebilled": true
            })),
            Err(_) => Err(McpError::internal("Failed to read usage budget")),
        }
    }

    async fn list_connectors(&self, scope: &McpAccessScope) -> Result<Value, McpError> {
        Ok(
            json!({ "connectors": build_connector_catalog_items(self.resolve_vault(), &scope.tenant_id).await }),
        )
    }

    async fn create_space(
        &self,
        scope: &McpAccessScope,
        input: CreateSpaceInput,
    ) -> Result<Value, McpError> {
        let now = chrono::Utc::now().timestamp();
        let (space_id, task_id, approval_id, idem) = (
            format!("spc-{}", uuid::Uuid::new_v4()),
            format!("tsk-{}", uuid::Uuid::new_v4()),
            format!("apr-{}", uuid::Uuid::new_v4()),
            uuid::Uuid::new_v4().to_string(),
        );
        let preview = json!({
            "space_id": space_id, "name": input.name, "description": input.description, "kind": input.kind,
            "prompt_template": input.prompt_template, "department": input.department,
            "reads": input.reads, "shows": input.shows, "cannot": input.cannot
        });
        let sha = compute_payload_sha256(&preview);
        let title = format!("Create Space: {}", input.name);
        let summary = format!(
            "Approve declarative Space '{}' ({})",
            input.name, input.kind
        );
        let persisted = self
            .insert_proposal(
                scope,
                &ProposalInsert {
                    task_id: &task_id,
                    approval_id: &approval_id,
                    space_id: None,
                    title: &title,
                    prompt: &input.prompt_template,
                    department: &input.department,
                    action_kind: "create_space",
                    summary: &summary,
                    payload_preview: &preview,
                    payload_sha256: &sha,
                    estimated_cost_micros: 0,
                    idempotency_key: &idem,
                    now,
                },
            )
            .await?;
        Ok(json!({
            "proposal_type": "create_space", "id": task_id, "proposed_space_id": space_id,
            "status": "awaiting_approval", "provenance": "awaiting_approval",
            "attention": "permission_required", "risk": "draft_for_review",
            "persisted": persisted, "schema_ready": persisted,
            "pending_approval": {
                "approval_id": approval_id, "action_kind": "create_space", "department": input.department,
                "risk": "draft_for_review", "summary": summary, "payload_preview": preview, "payload_sha256": sha,
                "estimated_cost_micros": 0, "standing_limit_micros": 0, "status": "pending",
                "decided_by": Value::Null, "decided_at_unix": Value::Null
            }
        }))
    }

    async fn propose_task(
        &self,
        scope: &McpAccessScope,
        input: ProposeTaskInput,
    ) -> Result<Value, McpError> {
        let now = chrono::Utc::now().timestamp();
        let (task_id, approval_id) = (
            format!("tsk-{}", uuid::Uuid::new_v4()),
            format!("apr-{}", uuid::Uuid::new_v4()),
        );
        let idem = input
            .idempotency_key
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let preview = json!({
            "title": input.title, "prompt": input.prompt, "space_id": input.space_id,
            "department": input.department, "action_kind": input.action_kind,
            "estimated_cost_micros": input.estimated_cost_micros
        });
        let sha = compute_payload_sha256(&preview);
        let summary = format!(
            "Approve task '{}' (estimated cost {} micro-USD)",
            input.title, input.estimated_cost_micros
        );
        let persisted = self
            .insert_proposal(
                scope,
                &ProposalInsert {
                    task_id: &task_id,
                    approval_id: &approval_id,
                    space_id: input.space_id.as_deref(),
                    title: &input.title,
                    prompt: &input.prompt,
                    department: &input.department,
                    action_kind: &input.action_kind,
                    summary: &summary,
                    payload_preview: &preview,
                    payload_sha256: &sha,
                    estimated_cost_micros: input.estimated_cost_micros,
                    idempotency_key: &idem,
                    now,
                },
            )
            .await?;
        Ok(json!({
            "id": task_id, "space_id": input.space_id, "title": input.title, "prompt": input.prompt,
            "department": input.department, "status": "awaiting_approval", "provenance": "awaiting_approval",
            "attention": "permission_required", "risk": "draft_for_review", "payer_mode": "unconfigured",
            "estimated_cost_micros": input.estimated_cost_micros, "idempotency_key": idem,
            "persisted": persisted, "schema_ready": persisted,
            "pending_approval": {
                "approval_id": approval_id, "action_kind": input.action_kind, "department": input.department,
                "risk": "draft_for_review", "summary": summary, "payload_preview": preview, "payload_sha256": sha,
                "estimated_cost_micros": input.estimated_cost_micros, "standing_limit_micros": 0,
                "status": "pending", "decided_by": Value::Null, "decided_at_unix": Value::Null
            }
        }))
    }

    async fn propose_email_draft(
        &self,
        scope: &McpAccessScope,
        input: ProposeEmailDraftInput,
    ) -> Result<Value, McpError> {
        let now = chrono::Utc::now().timestamp();
        let (task_id, approval_id, idem) = (
            format!("tsk-{}", uuid::Uuid::new_v4()),
            format!("apr-{}", uuid::Uuid::new_v4()),
            uuid::Uuid::new_v4().to_string(),
        );
        let preview = json!({
            "to": input.to, "from": input.from, "subject": input.subject,
            "body": input.body, "space_id": input.space_id, "department": input.department
        });
        let sha = compute_payload_sha256(&preview);
        let title = format!("Email draft to {}: {}", input.to, input.subject);
        let summary = format!(
            "Approve sending email to {} ('{}')",
            input.to, input.subject
        );
        let persisted = self
            .insert_proposal(
                scope,
                &ProposalInsert {
                    task_id: &task_id,
                    approval_id: &approval_id,
                    space_id: input.space_id.as_deref(),
                    title: &title,
                    prompt: &input.body,
                    department: &input.department,
                    action_kind: "send_email",
                    summary: &summary,
                    payload_preview: &preview,
                    payload_sha256: &sha,
                    estimated_cost_micros: 0,
                    idempotency_key: &idem,
                    now,
                },
            )
            .await?;
        Ok(json!({
            "id": task_id, "space_id": input.space_id, "title": title, "department": input.department,
            "status": "awaiting_approval", "provenance": "awaiting_approval",
            "attention": "permission_required", "risk": "draft_for_review", "sent": false,
            "persisted": persisted, "schema_ready": persisted,
            "pending_approval": {
                "approval_id": approval_id, "action_kind": "send_email", "department": input.department,
                "risk": "draft_for_review", "summary": summary, "payload_preview": preview, "payload_sha256": sha,
                "estimated_cost_micros": 0, "standing_limit_micros": 0, "status": "pending",
                "decided_by": Value::Null, "decided_at_unix": Value::Null
            }
        }))
    }
}

#[derive(Serialize)]
pub struct McpToolsResponse {
    pub success: bool,
    pub endpoint: String,
    pub protocol_version: String,
    pub default_mode: String,
    pub write_opt_in_header: String,
    pub tools: Vec<McpToolDescriptor>,
    pub resources: Vec<McpResourceDescriptor>,
}

pub async fn mcp_tools_handler(
    axum::extract::Extension(user): axum::extract::Extension<::server_common::Claims>,
) -> impl IntoResponse {
    if require_tenant(&user).is_none() {
        return private_json(
            StatusCode::UNAUTHORIZED,
            json!({"success": false, "message": "Authenticated organization required"}),
        );
    }
    private_json(
        StatusCode::OK,
        McpToolsResponse {
            success: true,
            endpoint: "/api/v1/integrations/mcp".into(),
            protocol_version: MCP_PROTOCOL_VERSION.into(),
            default_mode: "read_only".into(),
            write_opt_in_header: MCP_WRITE_OPT_IN_HEADER.into(),
            tools: tool_catalog(),
            resources: resource_catalog(),
        },
    )
}

pub async fn mcp_jsonrpc_handler(
    State(state): State<ToolIntegrationsApiState>,
    axum::extract::Extension(user): axum::extract::Extension<::server_common::Claims>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let Some(tenant_id) = require_tenant(&user) else {
        return private_json(
            StatusCode::UNAUTHORIZED,
            json!({"jsonrpc":"2.0","id":Value::Null,"error":{"code":-32001,"message":"Authenticated organization required"}}),
        );
    };
    let allow_write = headers
        .get(MCP_WRITE_OPT_IN_HEADER)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
    let scope =
        McpAccessScope::read_only(tenant_id, user.sub, user.roles).with_write_opt_in(allow_write);

    #[cfg(test)]
    let backend = DbTaskCenterMcpBackend {
        db: state.db.clone(),
        test_vault: state.test_vault.clone(),
    };
    #[cfg(not(test))]
    let backend = DbTaskCenterMcpBackend::new(state.db.clone());

    let rpc_response = TaskCenterMcpServer::new(backend)
        .handle_raw_bytes(&scope, &body)
        .await;
    let status = if rpc_response
        .error
        .as_ref()
        .is_some_and(|e| e.code == ERR_PAYLOAD_TOO_LARGE)
    {
        StatusCode::PAYLOAD_TOO_LARGE
    } else {
        StatusCode::OK
    };
    private_json(status, rpc_response)
}

pub fn router<S: Clone + Send + Sync + 'static>(db: Arc<DB>) -> Router<S> {
    router_with_state(ToolIntegrationsApiState {
        db,
        #[cfg(test)]
        test_vault: None,
    })
}

fn router_with_state<S: Clone + Send + Sync + 'static>(
    state: ToolIntegrationsApiState,
) -> Router<S> {
    Router::new()
        .route("/", get(get_integrations_handler))
        .route("/catalog", get(get_catalog_handler))
        .route("/resend/send", post(resend_send_handler))
        .route(
            "/google-workspace/files",
            get(google_workspace_files_handler),
        )
        .route("/mcp", post(mcp_jsonrpc_handler))
        .route("/mcp/tools", get(mcp_tools_handler))
        .route("/{id}/connect", post(connect_integration_handler))
        .route("/{id}/verify", post(refresh_integration_handler))
        .route("/{id}", axum::routing::delete(revoke_integration_handler))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::server_common::Claims;
    use aes_gcm::{
        Aes256Gcm,
        aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
    };
    use axum::{body::Body, http::Request};
    use std::collections::BTreeMap;
    use tower::ServiceExt;

    fn claims(tenant: Option<&str>, roles: &[&str]) -> Claims {
        Claims {
            sub: "user-1".into(),
            exp: chrono::Utc::now().timestamp() + 3600,
            iat: chrono::Utc::now().timestamp(),
            organization_id: tenant.map(ToString::to_string),
            username: "owner".into(),
            email: "owner@example.com".into(),
            roles: roles.iter().map(|r| (*r).into()).collect(),
            session_id: None,
            jti: "jti-1".into(),
        }
    }

    async fn open_sqlite_env() -> (Arc<DB>, sqlx::SqlitePool, ConnectionVault) {
        let pool = crate::db::create_sqlite_pool_for_test().await;
        sqlx::query("CREATE TABLE IF NOT EXISTS tool_integrations (id TEXT NOT NULL, tenant_id TEXT NOT NULL, status TEXT NOT NULL, PRIMARY KEY (tenant_id, id))")
            .execute(&pool)
            .await
            .unwrap();
        let ledger = UsageLedger::Sqlite(pool.clone());
        ledger.initialize().await.unwrap();
        let vault = ConnectionVault::with_keys(
            ledger,
            BTreeMap::from([("v1".into(), "11".repeat(32))]),
            "v1".into(),
        )
        .unwrap();
        vault.initialize().await.unwrap();
        let db = Arc::new(DB {
            pool: crate::db::create_dummy_pg_pool().await,
            store: DbStore::Sqlite(pool.clone()),
        });
        (db, pool, vault)
    }

    async fn insert_encrypted_connection(
        pool: &sqlx::SqlitePool,
        tenant: &str,
        provider: &str,
        secret: &str,
        verified_at: i64,
    ) {
        let rev = uuid::Uuid::new_v4().to_string();
        let aad = serde_json::to_vec(&(tenant, provider, &rev, "v1")).unwrap();
        let cipher = Aes256Gcm::new_from_slice(&[0x11u8; 32]).unwrap();
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ct = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: secret.as_bytes(),
                    aad: &aad,
                },
            )
            .unwrap();
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        sqlx::query("INSERT INTO ohc_provider_connections (tenant_id,provider,revision,state,key_id,nonce_hex,ciphertext_hex,verified_at) VALUES (?1,?2,?3,'verified','v1',?4,?5,?6)")
            .bind(tenant).bind(provider).bind(&rev).bind(hex(&nonce)).bind(hex(&ct)).bind(verified_at).execute(pool).await.unwrap();
    }

    async fn create_task_center_tables(pool: &sqlx::SqlitePool) {
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS task_center_spaces (id TEXT PRIMARY KEY NOT NULL,tenant_id TEXT NOT NULL,name TEXT NOT NULL,description TEXT NOT NULL DEFAULT '',kind TEXT NOT NULL DEFAULT 'custom',prompt_template TEXT NOT NULL DEFAULT '',department TEXT NOT NULL DEFAULT 'operations',reads_json TEXT NOT NULL DEFAULT '[]',shows_json TEXT NOT NULL DEFAULT '[]',cannot_json TEXT NOT NULL DEFAULT '[]',source_health TEXT NOT NULL DEFAULT 'unconfigured',pinned_artifact_ids_json TEXT NOT NULL DEFAULT '[]',archived INTEGER NOT NULL DEFAULT 0,created_at_unix BIGINT NOT NULL,updated_at_unix BIGINT NOT NULL);
             CREATE TABLE IF NOT EXISTS task_center_tasks (id TEXT PRIMARY KEY NOT NULL,tenant_id TEXT NOT NULL,actor_id TEXT NOT NULL DEFAULT '',space_id TEXT,title TEXT NOT NULL,prompt TEXT NOT NULL,department TEXT NOT NULL DEFAULT 'operations',requested_action TEXT NOT NULL DEFAULT 'run_task',status TEXT NOT NULL,provenance TEXT NOT NULL,attention TEXT NOT NULL DEFAULT 'none',risk TEXT NOT NULL DEFAULT 'draft_for_review',payer_mode TEXT NOT NULL DEFAULT 'unconfigured',estimated_cost_micros BIGINT NOT NULL DEFAULT 0,actual_cost_micros BIGINT,idempotency_key TEXT NOT NULL,request_digest TEXT NOT NULL DEFAULT '',receipt_id TEXT,usage_event_id TEXT,output_summary TEXT,error_message TEXT,archived INTEGER NOT NULL DEFAULT 0,created_at_unix BIGINT NOT NULL,updated_at_unix BIGINT NOT NULL);
             CREATE TABLE IF NOT EXISTS task_center_approvals (approval_id TEXT PRIMARY KEY NOT NULL,tenant_id TEXT NOT NULL,task_id TEXT NOT NULL,action_kind TEXT NOT NULL,department TEXT NOT NULL DEFAULT 'operations',risk TEXT NOT NULL DEFAULT 'draft_for_review',summary TEXT NOT NULL,payload_preview_json TEXT NOT NULL DEFAULT '{}',payload_sha256 TEXT NOT NULL,estimated_cost_micros BIGINT NOT NULL DEFAULT 0,standing_limit_micros BIGINT NOT NULL DEFAULT 0,status TEXT NOT NULL DEFAULT 'pending',decided_by TEXT,decided_at_unix BIGINT,decision_reason TEXT,created_at_unix BIGINT NOT NULL);",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    async fn read_json(resp: Response) -> Value {
        serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn connection_validation_uses_the_path_id_and_requires_provider_credentials() {
        let payload = ConnectIntegrationRequest {
            bot_token: Some(" AC123 ".into()),
            api_token: Some(" secret ".into()),
            from_phone: Some(" +15550001111 ".into()),
            integration_id: Some("attacker-selected".into()),
            base_url: Some("https://attacker.test".into()),
        };
        let validated = validate_connect_request("twilio", payload).expect("valid credentials");
        assert_eq!(validated.integration_id, "twilio");
        assert_eq!(validated.bot_token.as_deref(), Some("AC123"));
        assert_eq!(validated.api_token.as_deref(), Some("secret"));
        assert_eq!(validated.from_phone.as_deref(), Some("+15550001111"));
        assert!(provider_credentials_present(
            "twilio",
            validated.bot_token.as_deref(),
            validated.api_token.as_deref()
        ));
    }

    #[test]
    fn connection_validation_accepts_google_workspace() {
        let req = ConnectIntegrationRequest {
            bot_token: None,
            api_token: Some("oauth-token".into()),
            from_phone: None,
            integration_id: None,
            base_url: None,
        };
        assert!(validate_connect_request("google_workspace", req).is_ok());
        assert!(provider_credentials_present(
            "google_workspace",
            None,
            Some("oauth-token")
        ));
    }

    #[test]
    fn connection_validation_rejects_missing_credentials_and_invalid_ids() {
        let empty = ConnectIntegrationRequest {
            bot_token: Some(" ".into()),
            api_token: None,
            from_phone: None,
            integration_id: None,
            base_url: None,
        };
        assert!(validate_connect_request("twilio", empty).is_err());
        let cloud = ConnectIntegrationRequest {
            bot_token: None,
            api_token: Some("meta-token".into()),
            from_phone: None,
            integration_id: None,
            base_url: None,
        };
        assert!(validate_connect_request("whatsapp_cloud_api", cloud).is_ok());
        assert!(!provider_credentials_present("twilio", Some("sid"), None));
        assert!(!provider_credentials_present("shippo", None, None));
        let invalid = ConnectIntegrationRequest {
            bot_token: Some("bot".into()),
            api_token: Some("token".into()),
            from_phone: None,
            integration_id: None,
            base_url: None,
        };
        assert!(validate_connect_request("../twilio", invalid).is_err());
    }

    #[tokio::test]
    async fn catalog_reports_five_connectors_expiry_revocation_and_tenant_isolation() {
        let (db, pool, vault) = open_sqlite_env().await;
        let now = chrono::Utc::now().timestamp();
        insert_encrypted_connection(&pool, "tenant-a", "resend", "re_test_valid_key_123", now)
            .await;
        insert_encrypted_connection(
            &pool,
            "tenant-a",
            "google_workspace",
            "google-oauth-token-expired-123",
            now - (8 * 24 * 60 * 60),
        )
        .await;
        insert_encrypted_connection(&pool, "tenant-a", "stripe", "sk_test_revoked_key_1234", now)
            .await;
        vault.revoke("tenant-a", "stripe").await.unwrap();
        insert_encrypted_connection(
            &pool,
            "tenant-b",
            "openai_api",
            "sk-tenant-b-secret-12345",
            now,
        )
        .await;

        let app: Router = router_with_state(ToolIntegrationsApiState {
            db,
            test_vault: Some(vault),
        });
        let mut req = Request::builder()
            .method("GET")
            .uri("/catalog")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut()
            .insert(claims(Some("tenant-a"), &["member"]));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some(CACHE_CONTROL_PRIVATE_NO_STORE)
        );
        let body = read_json(resp).await;
        assert_eq!(body["mcp_server"]["protocol_version"], "2025-03-26");
        assert_eq!(body["mcp_server"]["default_mode"], "read_only");
        assert_eq!(body["mcp_server"]["write_opt_in_header"], "x-ohc-mcp-write");
        assert_eq!(body["mcp_server"]["tool_count"], 9);

        let by_id: HashMap<&str, &Value> = body["connectors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["id"].as_str().unwrap(), c))
            .collect();
        assert_eq!(by_id.len(), 5);
        assert_eq!(by_id["resend"]["status"], "verified");
        assert_eq!(by_id["resend"]["usable"], true);
        assert_eq!(by_id["google_workspace"]["status"], "verification_required");
        assert_eq!(by_id["google_workspace"]["usable"], false);
        assert_eq!(by_id["stripe"]["status"], "revoked");
        assert_eq!(by_id["openai_api"]["status"], "not_connected");
        assert_eq!(by_id["anthropic_api"]["status"], "not_connected");
    }

    #[tokio::test]
    async fn resend_and_google_workspace_fail_closed_412_and_enforce_validation() {
        let (db, pool, vault) = open_sqlite_env().await;
        let app: Router = router_with_state(ToolIntegrationsApiState {
            db,
            test_vault: Some(vault.clone()),
        });
        let payload = json!({"to":"client@example.com","from":"ops@example.com","subject":"Update","html_body":"<p>Hi</p>"}).to_string();

        // Resend: member -> 403, owner unverified -> 412, expired -> 412, revoked -> 412.
        let mut r1 = Request::builder()
            .method("POST")
            .uri("/resend/send")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(payload.clone()))
            .unwrap();
        r1.extensions_mut()
            .insert(claims(Some("tenant-a"), &["member"]));
        assert_eq!(
            app.clone().oneshot(r1).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );

        let mut r2 = Request::builder()
            .method("POST")
            .uri("/resend/send")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(payload.clone()))
            .unwrap();
        r2.extensions_mut()
            .insert(claims(Some("tenant-a"), &["owner"]));
        assert_eq!(
            app.clone().oneshot(r2).await.unwrap().status(),
            StatusCode::PRECONDITION_FAILED
        );

        let now = chrono::Utc::now().timestamp();
        insert_encrypted_connection(
            &pool,
            "tenant-a",
            "resend",
            "re_verified_tenant_key_999",
            now,
        )
        .await;
        assert_eq!(
            vault.read_key("tenant-a", "resend").await.unwrap().as_str(),
            "re_verified_tenant_key_999"
        );
        assert!(vault.read_key("tenant-b", "resend").await.is_err());
        vault.revoke("tenant-a", "resend").await.unwrap();
        let mut r3 = Request::builder()
            .method("POST")
            .uri("/resend/send")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(payload))
            .unwrap();
        r3.extensions_mut()
            .insert(claims(Some("tenant-a"), &["owner"]));
        assert_eq!(
            app.clone().oneshot(r3).await.unwrap().status(),
            StatusCode::PRECONDITION_FAILED
        );

        // Google Workspace files: unverified -> 412, query injection -> 400, verified vault read + revoke -> 412.
        let mut g1 = Request::builder()
            .method("GET")
            .uri("/google-workspace/files")
            .body(Body::empty())
            .unwrap();
        g1.extensions_mut()
            .insert(claims(Some("tenant-a"), &["member"]));
        assert_eq!(
            app.clone().oneshot(g1).await.unwrap().status(),
            StatusCode::PRECONDITION_FAILED
        );

        let mut g2 = Request::builder()
            .method("GET")
            .uri("/google-workspace/files?folder_id=root%27%20or%201%3D1")
            .body(Body::empty())
            .unwrap();
        g2.extensions_mut()
            .insert(claims(Some("tenant-a"), &["member"]));
        assert_eq!(
            app.clone().oneshot(g2).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );

        let drive_token = format!("{}{}.verified-drive-token-001", "ya", "29");
        insert_encrypted_connection(&pool, "tenant-a", "google_workspace", &drive_token, now).await;
        assert_eq!(
            vault
                .read_key("tenant-a", "google_workspace")
                .await
                .unwrap()
                .as_str(),
            drive_token
        );
        vault.revoke("tenant-a", "google_workspace").await.unwrap();
        let mut g3 = Request::builder()
            .method("GET")
            .uri("/google-workspace/files")
            .body(Body::empty())
            .unwrap();
        g3.extensions_mut()
            .insert(claims(Some("tenant-a"), &["member"]));
        assert_eq!(
            app.oneshot(g3).await.unwrap().status(),
            StatusCode::PRECONDITION_FAILED
        );
    }

    #[tokio::test]
    async fn mcp_endpoints_handle_pre_merge_db_and_persisted_task_center_schema() {
        let (db, pool, vault) = open_sqlite_env().await;
        UsageLedger::Sqlite(pool.clone())
            .set_limit("tenant-a", 2_500_000)
            .await
            .unwrap();
        let app: Router = router_with_state(ToolIntegrationsApiState {
            db,
            test_vault: Some(vault),
        });

        // GET /mcp/tools
        let mut t_req = Request::builder()
            .method("GET")
            .uri("/mcp/tools")
            .body(Body::empty())
            .unwrap();
        t_req
            .extensions_mut()
            .insert(claims(Some("tenant-a"), &["member"]));
        let t_json = read_json(app.clone().oneshot(t_req).await.unwrap()).await;
        assert_eq!(t_json["protocol_version"], "2025-03-26");
        assert_eq!(t_json["tools"].as_array().unwrap().len(), 9);
        assert_eq!(t_json["resources"].as_array().unwrap().len(), 4);

        // Pre-merge DB (no task_center_* tables): graceful read + write proposal
        let mut pre_list = Request::builder().method("POST").uri("/mcp").header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"ohc_list_tasks","arguments":{}}}).to_string())).unwrap();
        pre_list
            .extensions_mut()
            .insert(claims(Some("tenant-a"), &["owner"]));
        let pre_list_json = read_json(app.clone().oneshot(pre_list).await.unwrap()).await;
        assert_eq!(
            pre_list_json["result"]["structuredContent"]["schema_ready"],
            false
        );

        let propose_body = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ohc_propose_email_draft","arguments":{"to":"vendor@example.com","subject":"Invoice #42","body":"Please confirm receipt."}}}).to_string();
        let mut blocked = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(propose_body.clone()))
            .unwrap();
        blocked
            .extensions_mut()
            .insert(claims(Some("tenant-a"), &["owner"]));
        assert_eq!(
            read_json(app.clone().oneshot(blocked).await.unwrap()).await["error"]["code"],
            -32003
        );

        let mut pre_write = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .header(MCP_WRITE_OPT_IN_HEADER, "true")
            .body(Body::from(propose_body.clone()))
            .unwrap();
        pre_write
            .extensions_mut()
            .insert(claims(Some("tenant-a"), &["owner"]));
        let pre_w_json = read_json(app.clone().oneshot(pre_write).await.unwrap()).await;
        assert_eq!(
            pre_w_json["result"]["structuredContent"]["status"],
            "awaiting_approval"
        );
        assert_eq!(
            pre_w_json["result"]["structuredContent"]["persisted"],
            false
        );

        let mut huge = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(vec![b' '; 65 * 1024]))
            .unwrap();
        huge.extensions_mut()
            .insert(claims(Some("tenant-a"), &["owner"]));
        let huge_resp = app.clone().oneshot(huge).await.unwrap();
        assert_eq!(huge_resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            read_json(huge_resp).await["error"]["code"],
            ERR_PAYLOAD_TOO_LARGE
        );

        // Post-merge task_center_* tables: persists proposals, reads resources, isolates tenants
        create_task_center_tables(&pool).await;
        let mut persisted = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .header(MCP_WRITE_OPT_IN_HEADER, "true")
            .body(Body::from(propose_body))
            .unwrap();
        persisted
            .extensions_mut()
            .insert(claims(Some("tenant-a"), &["owner"]));
        let p_json = read_json(app.clone().oneshot(persisted).await.unwrap()).await;
        assert_eq!(p_json["result"]["structuredContent"]["persisted"], true);
        let tid = p_json["result"]["structuredContent"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let mut appr = Request::builder().method("POST").uri("/mcp").header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"jsonrpc":"2.0","id":3,"method":"resources/read","params":{"uri":"ohc://approvals/pending"}}).to_string())).unwrap();
        appr.extensions_mut()
            .insert(claims(Some("tenant-a"), &["member"]));
        assert!(read_json(app.clone().oneshot(appr).await.unwrap()).await["result"]["contents"][0]["text"].as_str().unwrap().contains(&tid));

        let mut budget = Request::builder().method("POST").uri("/mcp").header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"jsonrpc":"2.0","id":4,"method":"resources/read","params":{"uri":"ohc://budget/summary"}}).to_string())).unwrap();
        budget
            .extensions_mut()
            .insert(claims(Some("tenant-a"), &["member"]));
        assert!(read_json(app.clone().oneshot(budget).await.unwrap()).await["result"]["contents"][0]["text"].as_str().unwrap().contains("2500000"));

        let mut cross = Request::builder().method("POST").uri("/mcp").header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"ohc_get_task","arguments":{"task_id": tid}}}).to_string())).unwrap();
        cross
            .extensions_mut()
            .insert(claims(Some("tenant-b"), &["owner"]));
        assert_eq!(
            read_json(app.oneshot(cross).await.unwrap()).await["error"]["code"],
            -32004
        );
    }
}
