// SPDX-License-Identifier: Apache-2.0
// Copyright (c) OmniSolo LLC.

//! Single-tenant open-source default implementations of the core extension traits.

use crate::extensions::{
    AuthCapabilities, AuthError, AuthPolicyProvider, AuthzError, BillingError,
    BillingPlatformProvider, DeploymentRuntime, EditionCapabilities, LockGuard, PeerIdentity,
    QuotaDecision, RuntimeError, StorageError, StorageHandle, SyncTransportProvider,
    TenantContextProvider, TransportError, UsageAccountingEvent, WorkspaceScope,
};
use dashmap::DashMap;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

type HmacSha256 = Hmac<Sha256>;

/// Single-tenant open-source implementation of [`TenantContextProvider`].
#[derive(Clone, Debug)]
pub struct SingleTenantContext {
    workspace_id: String,
}

impl SingleTenantContext {
    /// Creates a single-tenant context bound to a concrete non-empty workspace ID.
    /// Rejects blank and reserved `"system"` identifiers to fail closed.
    pub fn new(workspace_id: impl Into<String>) -> Result<Self, AuthzError> {
        let workspace_id = workspace_id.into().trim().to_string();
        if workspace_id.is_empty() || workspace_id.eq_ignore_ascii_case("system") {
            return Err(AuthzError::InvalidConfiguration(
                "single-tenant workspace_id must be non-empty and not 'system'".to_string(),
            ));
        }
        Ok(Self { workspace_id })
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }
}

#[async_trait::async_trait]
impl TenantContextProvider for SingleTenantContext {
    fn resolve_http_scope(
        &self,
        req: &axum::http::Request<axum::body::Body>,
    ) -> Result<WorkspaceScope, AuthzError> {
        let scope = req
            .extensions()
            .get::<WorkspaceScope>()
            .ok_or_else(|| AuthzError::Unauthenticated("missing workspace scope".to_string()))?;
        let actor_id = scope.actor_id.trim();
        if actor_id.is_empty() {
            return Err(AuthzError::Unauthenticated("missing actor_id".to_string()));
        }
        let requested_workspace = scope.workspace_id.trim();
        if requested_workspace != self.workspace_id {
            return Err(AuthzError::Forbidden(
                "workspace scope does not match configured local workspace".to_string(),
            ));
        }
        Ok(WorkspaceScope {
            workspace_id: self.workspace_id.clone(),
            actor_id: actor_id.to_string(),
            roles: scope.roles.clone(),
        })
    }

    async fn bind_pg_transaction(
        &self,
        _tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        scope: &WorkspaceScope,
    ) -> Result<(), StorageError> {
        if scope.workspace_id.trim() != self.workspace_id {
            return Err(StorageError::InvalidScope(
                "transaction scope does not match configured local workspace".to_string(),
            ));
        }
        Ok(())
    }

    fn apply_http_guard(&self, router: axum::Router) -> axum::Router {
        router
    }
}

/// Single-tenant open-source implementation of [`AuthPolicyProvider`].
/// Uses HMAC-SHA256 bearer verification for local workload gRPC calls and an
/// in-process revocation set for token invalidation.
#[derive(Clone)]
pub struct LocalOwnerAuthPolicy {
    workspace_id: String,
    grpc_token: Option<(Vec<u8>, Vec<u8>)>,
    revoked_jtis: Arc<DashMap<String, ()>>,
}

impl fmt::Debug for LocalOwnerAuthPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalOwnerAuthPolicy")
            .field("workspace_id", &self.workspace_id)
            .field("grpc_token_configured", &self.grpc_token.is_some())
            .field("revoked_count", &self.revoked_jtis.len())
            .finish()
    }
}

impl LocalOwnerAuthPolicy {
    /// Creates a policy with no gRPC token configured (gRPC peer checks fail closed).
    pub fn new(workspace_id: impl Into<String>) -> Result<Self, AuthzError> {
        let workspace_id = workspace_id.into().trim().to_string();
        if workspace_id.is_empty() || workspace_id.eq_ignore_ascii_case("system") {
            return Err(AuthzError::InvalidConfiguration(
                "local auth policy requires a non-empty workspace_id".to_string(),
            ));
        }
        Ok(Self {
            workspace_id,
            grpc_token: None,
            revoked_jtis: Arc::new(DashMap::new()),
        })
    }

    /// Configures a local HMAC-SHA256 bearer token for loopback gRPC peer verification.
    pub fn with_hmac_token(
        mut self,
        expected_token: &str,
        signing_key: &[u8],
    ) -> Result<Self, AuthzError> {
        if expected_token.trim().is_empty() || signing_key.len() < 32 {
            return Err(AuthzError::InvalidConfiguration(
                "local gRPC token must be non-empty and signing key must be at least 32 bytes"
                    .to_string(),
            ));
        }
        let mut mac = HmacSha256::new_from_slice(signing_key).map_err(|_| {
            AuthzError::InvalidConfiguration("invalid HMAC verification key".to_string())
        })?;
        mac.update(expected_token.as_bytes());
        let expected_hash = mac.finalize().into_bytes().to_vec();
        self.grpc_token = Some((expected_hash, signing_key.to_vec()));
        Ok(self)
    }

    /// Marks a token identifier as revoked in this process.
    pub fn revoke_jti(&self, jti: impl Into<String>) {
        let jti = jti.into();
        if !jti.trim().is_empty() {
            self.revoked_jtis.insert(jti, ());
        }
    }
}

#[async_trait::async_trait]
impl AuthPolicyProvider for LocalOwnerAuthPolicy {
    fn verify_grpc_peer(&self, req: &tonic::Request<()>) -> Result<PeerIdentity, tonic::Status> {
        let Some((expected_hash, key)) = &self.grpc_token else {
            return Err(tonic::Status::unauthenticated(
                "local gRPC peer authentication token is not configured",
            ));
        };
        let auth_header = req
            .metadata()
            .get("authorization")
            .ok_or_else(|| tonic::Status::unauthenticated("missing authorization header"))?
            .to_str()
            .map_err(|_| tonic::Status::unauthenticated("invalid authorization header"))?;
        let token = auth_header
            .strip_prefix("Bearer ")
            .ok_or_else(|| tonic::Status::unauthenticated("authorization must use Bearer token"))?;
        let mut mac = HmacSha256::new_from_slice(key)
            .map_err(|_| tonic::Status::internal("invalid HMAC key"))?;
        mac.update(token.as_bytes());
        mac.verify_slice(expected_hash)
            .map_err(|_| tonic::Status::unauthenticated("invalid bearer token"))?;
        Ok(PeerIdentity {
            principal: "local-owner-agent".to_string(),
            workspace_id: Some(self.workspace_id.clone()),
            auth_scheme: "hmac-bearer".to_string(),
        })
    }

    async fn is_jti_revoked(&self, jti: &str, scope: &WorkspaceScope) -> Result<bool, AuthError> {
        if jti.trim().is_empty() || scope.workspace_id.trim().is_empty() {
            return Err(AuthError::InvalidInput(
                "jti and workspace_id are required for revocation checks".to_string(),
            ));
        }
        if scope.workspace_id.trim() != self.workspace_id {
            return Err(AuthError::InvalidInput(
                "workspace_id does not match configured local workspace".to_string(),
            ));
        }
        Ok(self.revoked_jtis.contains_key(jti))
    }

    fn extra_auth_routes(&self) -> Option<axum::Router> {
        None
    }

    fn capabilities(&self) -> AuthCapabilities {
        AuthCapabilities {
            local_password: true,
            api_keys: true,
            oidc_sso: false,
            mtls_peer_auth: false,
        }
    }
}

/// Single-tenant open-source implementation of [`BillingPlatformProvider`].
/// Never enforces hosted platform tiers; personal BYOK hard budgets continue to
/// run independently inside the local usage ledger.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoOpPlatformBilling;

#[async_trait::async_trait]
impl BillingPlatformProvider for NoOpPlatformBilling {
    async fn check_platform_quota(
        &self,
        scope: &WorkspaceScope,
        action: &str,
    ) -> Result<QuotaDecision, BillingError> {
        if scope.workspace_id.trim().is_empty() || action.trim().is_empty() {
            return Err(BillingError::InvalidInput(
                "workspace_id and action must be non-empty".to_string(),
            ));
        }
        Ok(QuotaDecision::allow())
    }

    async fn record_platform_usage(&self, _event: UsageAccountingEvent) {}

    fn platform_billing_routes(&self) -> Option<axum::Router> {
        None
    }
}

/// Single-tenant open-source implementation of [`SyncTransportProvider`].
/// Uses in-process `tokio::sync::broadcast` channels and TTL-guarded locks.
#[derive(Clone, Debug)]
pub struct InProcessLocalTransport {
    channels: Arc<DashMap<String, broadcast::Sender<serde_json::Value>>>,
    locks: Arc<DashMap<String, Instant>>,
    channel_capacity: usize,
}

impl Default for InProcessLocalTransport {
    fn default() -> Self {
        Self::new(128)
    }
}

impl InProcessLocalTransport {
    pub fn new(channel_capacity: usize) -> Self {
        Self {
            channels: Arc::new(DashMap::new()),
            locks: Arc::new(DashMap::new()),
            channel_capacity: channel_capacity.max(1),
        }
    }

    /// Subscribes to in-process events published on `topic`.
    pub fn subscribe(&self, topic: &str) -> broadcast::Receiver<serde_json::Value> {
        let sender = self
            .channels
            .entry(topic.to_string())
            .or_insert_with(|| broadcast::channel(self.channel_capacity).0)
            .clone();
        sender.subscribe()
    }
}

#[async_trait::async_trait]
impl SyncTransportProvider for InProcessLocalTransport {
    async fn publish_event(
        &self,
        topic: &str,
        payload: serde_json::Value,
    ) -> Result<(), TransportError> {
        let topic = topic.trim();
        if topic.is_empty() {
            return Err(TransportError::InvalidInput(
                "topic must not be empty".to_string(),
            ));
        }
        let sender = self
            .channels
            .entry(topic.to_string())
            .or_insert_with(|| broadcast::channel(self.channel_capacity).0)
            .clone();
        // Zero active receivers is valid for best-effort local event fanout.
        let _ = sender.send(payload);
        Ok(())
    }

    async fn acquire_lock(
        &self,
        key: &str,
        ttl_secs: u64,
    ) -> Result<Option<LockGuard>, TransportError> {
        let key = key.trim();
        if key.is_empty() || ttl_secs == 0 {
            return Err(TransportError::InvalidInput(
                "lock key must be non-empty and ttl_secs must be positive".to_string(),
            ));
        }
        let now = Instant::now();
        let expires_at = now + Duration::from_secs(ttl_secs);
        match self.locks.entry(key.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(mut occupied) => {
                if *occupied.get() > now {
                    return Ok(None);
                }
                occupied.insert(expires_at);
            }
            dashmap::mapref::entry::Entry::Vacant(vacant) => {
                vacant.insert(expires_at);
            }
        }
        let locks = Arc::clone(&self.locks);
        let owned_key = key.to_string();
        Ok(Some(LockGuard::new(key, move || {
            let _ = locks.remove_if(&owned_key, |_, exp| *exp == expires_at);
        })))
    }

    fn extra_sync_routes(&self) -> Option<axum::Router> {
        None
    }
}

/// Community (single-tenant open-source) edition capability descriptor.
#[derive(Clone, Copy, Debug, Default)]
pub struct CommunityEditionCapabilities;

impl CommunityEditionCapabilities {
    pub fn capabilities() -> EditionCapabilities {
        EditionCapabilities {
            edition: "community".to_string(),
            multi_tenant: false,
            sso_oidc: false,
            platform_billing: false,
            cloud_sync: false,
            collectives: false,
        }
    }
}

impl From<CommunityEditionCapabilities> for EditionCapabilities {
    fn from(_: CommunityEditionCapabilities) -> Self {
        CommunityEditionCapabilities::capabilities()
    }
}

/// Single-tenant open-source implementation of [`DeploymentRuntime`] backed by SQLite.
#[derive(Clone, Debug)]
pub struct SingleTenantRuntime {
    sqlite_url: String,
}

impl SingleTenantRuntime {
    pub fn new(sqlite_url: impl Into<String>) -> Result<Self, RuntimeError> {
        let sqlite_url = sqlite_url.into().trim().to_string();
        if sqlite_url.is_empty() {
            return Err(RuntimeError::InvalidConfiguration(
                "sqlite_url must not be empty".to_string(),
            ));
        }
        Ok(Self { sqlite_url })
    }
}

#[async_trait::async_trait]
impl DeploymentRuntime for SingleTenantRuntime {
    fn edition_capabilities(&self) -> EditionCapabilities {
        CommunityEditionCapabilities::capabilities()
    }

    async fn init_storage(&self) -> Result<StorageHandle, RuntimeError> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(4)
            .connect(&self.sqlite_url)
            .await
            .map_err(|err| RuntimeError::StorageInit(err.to_string()))?;
        Ok(StorageHandle::Sqlite(pool))
    }

    async fn run_migrations(&self, storage: &StorageHandle) -> Result<(), RuntimeError> {
        match storage {
            StorageHandle::Sqlite(pool) => {
                sqlx::query("SELECT 1")
                    .execute(pool)
                    .await
                    .map_err(|err| RuntimeError::Migration(err.to_string()))?;
                Ok(())
            }
            StorageHandle::Postgres(pool) => {
                sqlx::query("SELECT 1")
                    .execute(pool)
                    .await
                    .map_err(|err| RuntimeError::Migration(err.to_string()))?;
                Ok(())
            }
        }
    }

    fn mount_extra_http_routes(&self) -> axum::Router {
        axum::Router::new()
    }

    fn mount_extra_grpc_services(
        &self,
        router: tonic::transport::server::Router,
    ) -> tonic::transport::server::Router {
        router
    }
}
