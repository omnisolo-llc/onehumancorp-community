// SPDX-License-Identifier: Apache-2.0
// Copyright (c) OmniSolo LLC.

//! Extension trait seams separating single-tenant open-source core behavior
//! from externally supplied deployment policies, authentication adapters,
//! platform billing providers, and distributed transports.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

pub use crate::defaults::{
    CommunityEditionCapabilities, InProcessLocalTransport, LocalOwnerAuthPolicy,
    NoOpPlatformBilling, SingleTenantContext, SingleTenantRuntime,
};

/// Authenticated caller scope within a workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceScope {
    /// Fixed local workspace identifier in single-tenant mode, or signed
    /// organization identifier supplied by a deployment policy provider.
    pub workspace_id: String,
    pub actor_id: String,
    pub roles: Vec<String>,
}

/// Verified workload identity returned by gRPC peer authentication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerIdentity {
    pub principal: String,
    pub workspace_id: Option<String>,
    pub auth_scheme: String,
}

/// Runtime edition and capability metadata advertised to clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditionCapabilities {
    pub edition: String,
    pub multi_tenant: bool,
    pub sso_oidc: bool,
    pub platform_billing: bool,
    pub cloud_sync: bool,
    pub collectives: bool,
}

/// Authentication capabilities advertised to user-facing clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCapabilities {
    pub local_password: bool,
    pub api_keys: bool,
    pub oidc_sso: bool,
    pub mtls_peer_auth: bool,
}

/// Outcome of a platform-level quota check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaDecision {
    pub allowed: bool,
    pub soft_limit_reached: bool,
    pub warning_message: Option<String>,
}

impl QuotaDecision {
    pub fn allow() -> Self {
        Self {
            allowed: true,
            soft_limit_reached: false,
            warning_message: None,
        }
    }

    pub fn deny(message: impl Into<String>) -> Self {
        Self {
            allowed: false,
            soft_limit_reached: true,
            warning_message: Some(message.into()),
        }
    }
}

/// Platform-level usage event emitted after a metered operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageAccountingEvent {
    pub workspace_id: String,
    pub actor_id: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub estimated_cost_micro_usd: i64,
}

/// Storage backend handle returned by [`DeploymentRuntime::init_storage`].
#[derive(Clone, Debug)]
pub enum StorageHandle {
    Sqlite(sqlx::SqlitePool),
    Postgres(sqlx::PgPool),
}

/// RAII lease returned by [`SyncTransportProvider::acquire_lock`].
/// Dropping the guard automatically releases the lock.
pub struct LockGuard {
    key: String,
    release_fn: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl LockGuard {
    pub fn new(key: impl Into<String>, release_fn: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            key: key.into(),
            release_fn: Some(Box::new(release_fn)),
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// Explicitly releases the lock before drop.
    pub fn release(mut self) {
        if let Some(release) = self.release_fn.take() {
            release();
        }
    }
}

impl fmt::Debug for LockGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LockGuard")
            .field("key", &self.key)
            .field("active", &self.release_fn.is_some())
            .finish()
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Some(release) = self.release_fn.take() {
            release();
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthzError {
    Unauthenticated(String),
    Forbidden(String),
    InvalidConfiguration(String),
}

impl fmt::Display for AuthzError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthenticated(msg) => write!(f, "unauthenticated: {msg}"),
            Self::Forbidden(msg) => write!(f, "forbidden: {msg}"),
            Self::InvalidConfiguration(msg) => write!(f, "invalid authz configuration: {msg}"),
        }
    }
}

impl std::error::Error for AuthzError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageError {
    InvalidScope(String),
    Backend(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScope(msg) => write!(f, "invalid storage scope: {msg}"),
            Self::Backend(msg) => write!(f, "storage backend error: {msg}"),
        }
    }
}

impl std::error::Error for StorageError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthError {
    InvalidInput(String),
    Unavailable(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(msg) => write!(f, "invalid auth input: {msg}"),
            Self::Unavailable(msg) => write!(f, "auth service unavailable: {msg}"),
        }
    }
}

impl std::error::Error for AuthError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BillingError {
    InvalidInput(String),
    Unavailable(String),
}

impl fmt::Display for BillingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(msg) => write!(f, "invalid billing input: {msg}"),
            Self::Unavailable(msg) => write!(f, "billing provider unavailable: {msg}"),
        }
    }
}

impl std::error::Error for BillingError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    InvalidInput(String),
    PublishFailed(String),
    LockFailed(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(msg) => write!(f, "invalid transport input: {msg}"),
            Self::PublishFailed(msg) => write!(f, "publish failed: {msg}"),
            Self::LockFailed(msg) => write!(f, "lock operation failed: {msg}"),
        }
    }
}

impl std::error::Error for TransportError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeError {
    InvalidConfiguration(String),
    StorageInit(String),
    Migration(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration(msg) => write!(f, "invalid runtime configuration: {msg}"),
            Self::StorageInit(msg) => write!(f, "storage init failed: {msg}"),
            Self::Migration(msg) => write!(f, "migration failed: {msg}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// Configuration passed into an injected [`OidcVerifier`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidcVerificationConfig {
    pub issuer_url: String,
    pub client_id: String,
    pub enabled: bool,
}

/// Verified identity claims returned by an injected [`OidcVerifier`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidcVerifiedIdentity {
    pub sub: String,
    pub exp: i64,
    pub iat: i64,
    pub organization_id: Option<String>,
    pub username: String,
    pub email: String,
    pub roles: Vec<String>,
    pub jti: String,
}

/// Failure category returned by an [`OidcVerifier`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OidcVerificationError {
    InvalidToken,
    Unavailable,
}

impl fmt::Display for OidcVerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidToken => f.write_str("invalid OIDC token"),
            Self::Unavailable => f.write_str("OIDC verification unavailable"),
        }
    }
}

impl std::error::Error for OidcVerificationError {}

/// Runtime-injected verifier for OpenID Connect ID tokens.
#[async_trait::async_trait]
pub trait OidcVerifier: Send + Sync {
    async fn verify_id_token(
        &self,
        token: &str,
        config: &OidcVerificationConfig,
    ) -> Result<OidcVerifiedIdentity, OidcVerificationError>;
}

/// Fail-closed default [`OidcVerifier`] used when no external OIDC adapter is wired.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnconfiguredOidcVerifier;

#[async_trait::async_trait]
impl OidcVerifier for UnconfiguredOidcVerifier {
    async fn verify_id_token(
        &self,
        _token: &str,
        _config: &OidcVerificationConfig,
    ) -> Result<OidcVerifiedIdentity, OidcVerificationError> {
        Err(OidcVerificationError::Unavailable)
    }
}

/// Runtime-injected workload peer verifier for gRPC services (for example,
/// external mTLS client certificate authentication).
pub trait PeerIdentityVerifier: Send + Sync + fmt::Debug {
    #[allow(clippy::result_large_err)]
    fn verify_peer(&self, req: &tonic::Request<()>) -> Result<PeerIdentity, tonic::Status>;
}

/// Fail-closed default [`PeerIdentityVerifier`] when no mTLS verifier is injected.
#[derive(Clone, Debug)]
pub struct FailClosedPeerVerifier {
    reason: String,
}

impl FailClosedPeerVerifier {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl Default for FailClosedPeerVerifier {
    fn default() -> Self {
        Self::new("verified mTLS peer identity is unavailable")
    }
}

impl PeerIdentityVerifier for FailClosedPeerVerifier {
    fn verify_peer(&self, _req: &tonic::Request<()>) -> Result<PeerIdentity, tonic::Status> {
        Err(tonic::Status::unauthenticated(self.reason.clone()))
    }
}

/// 1. Tenant / Workspace Isolation Seam.
#[async_trait::async_trait]
pub trait TenantContextProvider: Send + Sync {
    /// Extracts and validates the caller's workspace scope from HTTP request extensions.
    fn resolve_http_scope(
        &self,
        req: &axum::http::Request<axum::body::Body>,
    ) -> Result<WorkspaceScope, AuthzError>;

    /// Configures a PostgreSQL transaction before business queries execute.
    async fn bind_pg_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        scope: &WorkspaceScope,
    ) -> Result<(), StorageError>;

    /// Configures a PostgreSQL transaction for background or cross-item maintenance
    /// jobs (no-op in Community Edition; elevates transaction role in Cloud).
    async fn bind_pg_system_transaction(
        &self,
        _tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    /// Wraps protected HTTP routes with an optional edition-specific guard layer.
    fn apply_http_guard(&self, router: axum::Router) -> axum::Router;
}

/// 2. Authentication, SSO & Workload Identity Seam.
#[async_trait::async_trait]
pub trait AuthPolicyProvider: Send + Sync {
    /// Verifies workload-to-workload gRPC calls.
    #[allow(clippy::result_large_err)]
    fn verify_grpc_peer(&self, req: &tonic::Request<()>) -> Result<PeerIdentity, tonic::Status>;

    /// Checks whether a token identifier (`jti`) has been revoked.
    async fn is_jti_revoked(&self, jti: &str, scope: &WorkspaceScope) -> Result<bool, AuthError>;

    /// Optional extra authentication routes mounted by the host edition.
    fn extra_auth_routes(&self) -> Option<axum::Router>;

    /// Optional runtime OIDC ID-token verifier.
    fn oidc_verifier(&self) -> Option<Arc<dyn OidcVerifier>> {
        None
    }

    /// Capabilities advertised to clients via capability discovery.
    fn capabilities(&self) -> AuthCapabilities;
}

/// 3. Platform Billing & Quota Seam (distinct from solo-owner merchant payments).
#[async_trait::async_trait]
pub trait BillingPlatformProvider: Send + Sync {
    /// Evaluates platform-level quotas before protected actions.
    async fn check_platform_quota(
        &self,
        scope: &WorkspaceScope,
        action: &str,
    ) -> Result<QuotaDecision, BillingError>;

    /// Optional accounting sink for platform-level usage reporting.
    async fn record_platform_usage(&self, event: UsageAccountingEvent);

    /// Optional platform subscription checkout handler for editions offering hosted plans.
    /// Returns `Ok(None)` in editions where hosted plans are unavailable.
    async fn create_platform_checkout_session(
        &self,
        _scope: &WorkspaceScope,
        _request_payload: serde_json::Value,
    ) -> Result<Option<String>, BillingError> {
        Ok(None)
    }

    /// Optional platform Stripe webhook reconciliation hook for non-merchant SaaS billing events.
    async fn reconcile_platform_webhook_event(
        &self,
        _event_json: serde_json::Value,
    ) -> Result<(), BillingError> {
        Ok(())
    }

    /// Optional extra platform billing routes mounted by the host edition.
    fn platform_billing_routes(&self) -> Option<axum::Router>;
}

/// 4. Realtime Fanout, Distributed Locking & Cross-Device Sync Seam.
#[async_trait::async_trait]
pub trait SyncTransportProvider: Send + Sync {
    /// Publishes an invalidation or realtime event to subscribers on `topic`.
    async fn publish_event(
        &self,
        topic: &str,
        payload: serde_json::Value,
    ) -> Result<(), TransportError>;

    /// Acquires a short-lived resource lock (for example, inventory reservation).
    async fn acquire_lock(
        &self,
        key: &str,
        ttl_secs: u64,
    ) -> Result<Option<LockGuard>, TransportError>;

    /// Optional extra sync routes mounted by the host edition.
    fn extra_sync_routes(&self) -> Option<axum::Router>;
}

/// 5. Runtime, Database Factory, Migrations & Service Mounting Seam.
#[async_trait::async_trait]
pub trait DeploymentRuntime: Send + Sync {
    fn edition_capabilities(&self) -> EditionCapabilities;
    async fn init_storage(&self) -> Result<StorageHandle, RuntimeError>;
    async fn run_migrations(&self, storage: &StorageHandle) -> Result<(), RuntimeError>;
    fn mount_extra_http_routes(&self) -> axum::Router;
    fn mount_extra_grpc_services(
        &self,
        router: tonic::transport::server::Router,
    ) -> tonic::transport::server::Router;
}
