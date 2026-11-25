#![allow(clippy::all)]
pub use ::server_common as common;
pub use ::server_omnisolo as ohc;

pub mod commit_authority;
pub mod email;
pub mod http;
pub mod postgres_store;
pub mod seaorm_store;
pub mod sqlite_store;
pub mod user_repository;
pub mod validation;

pub mod orchestration {
    /// Verified session/workload identity attached by HTTP or gRPC auth policy middleware.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct AuthInfo {
        pub org_id: String,
        pub agent_id: String,
        pub spiffe_id: String,
    }

    /// Extracts verified caller workspace and actor identity from gRPC request extensions.
    pub fn extract_grpc_workspace<T>(
        request: &tonic::Request<T>,
    ) -> Result<(String, String), tonic::Status> {
        if let Some(peer) = request
            .extensions()
            .get::<::ohc_core::extensions::PeerIdentity>()
            && let Some(workspace_id) = peer
                .workspace_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
        {
            return Ok((workspace_id.to_string(), peer.principal.clone()));
        }
        if let Some(auth) = request.extensions().get::<AuthInfo>()
            && !auth.org_id.trim().is_empty()
        {
            return Ok((auth.org_id.trim().to_string(), auth.agent_id.clone()));
        }
        if let Some(scope) = request
            .extensions()
            .get::<::ohc_core::extensions::WorkspaceScope>()
            && !scope.workspace_id.trim().is_empty()
        {
            return Ok((
                scope.workspace_id.trim().to_string(),
                scope.actor_id.clone(),
            ));
        }
        Err(tonic::Status::unauthenticated(
            "missing verified workspace identity",
        ))
    }
}

pub use orchestration::AuthInfo;

pub mod db {
    use sqlx::postgres::PgPool;
    use std::sync::OnceLock;

    static GLOBAL_POOL: OnceLock<PgPool> = OnceLock::new();

    pub fn get_pool() -> PgPool {
        GLOBAL_POOL
            .get_or_init(|| {
                let raw_url = std::env::var("DATABASE_URL")
                    .ok()
                    .or_else(|| std::env::var("OMNISOLO_DATABASE_URL").ok())
                    .unwrap_or_else(|| {
                        "postgres://postgres:postgres@localhost:5432/test".to_string()
                    });
                let is_pg =
                    raw_url.starts_with("postgres://") || raw_url.starts_with("postgresql://");
                let database_url = if is_pg {
                    raw_url
                } else {
                    "postgres://postgres:postgres@localhost:5432/test".to_string()
                };
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(100)
                    .acquire_timeout(std::time::Duration::from_millis(if is_pg {
                        15000
                    } else {
                        100
                    }))
                    .after_release(|conn, _meta| {
                        Box::pin(async move {
                            use sqlx::Executor;
                            conn.execute("RESET ROLE; RESET ALL;").await?;
                            Ok(true)
                        })
                    })
                    .connect_lazy(&database_url)
                    .expect("Failed to connect to DB pool lazily")
            })
            .clone()
    }
}

use std::collections::HashMap;

pub async fn strict_bearer_auth_middleware(
    axum::extract::State(store): axum::extract::State<std::sync::Arc<Store>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let mut authorization_values = req
        .headers()
        .get_all(axum::http::header::AUTHORIZATION)
        .iter();
    let token = authorization_values
        .next()
        .filter(|_| authorization_values.next().is_none())
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| {
            !token.is_empty()
                && token.len() <= MAX_ACCESS_TOKEN_BYTES
                && !token
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_ascii_control())
        });
    let Some(token) = token else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };

    let mut claims = match store.validate_token(token).await {
        Ok(claims) => claims,
        Err(_) => return axum::http::StatusCode::UNAUTHORIZED.into_response(),
    };
    let Some(organization_id) = claims
        .organization_id
        .as_deref()
        .map(str::trim)
        .filter(|organization_id| {
            !organization_id.is_empty() && !organization_id.eq_ignore_ascii_case("system")
        })
        .map(str::to_string)
    else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };

    let user_id = claims.sub.clone();
    let Some(user) = store
        .get_user(&user_id, &organization_id)
        .await
        .filter(|user| user.active && Store::user_access_token_fits(user))
    else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let canonical_user_id = user.id.clone();
    claims.sub = user.id;
    claims.username = user.username;
    claims.email = user.email;
    claims.roles = user.roles;
    claims.organization_id = Some(organization_id.clone());
    req.extensions_mut().insert(crate::orchestration::AuthInfo {
        org_id: organization_id.clone(),
        agent_id: canonical_user_id.clone(),
        spiffe_id: "spiffe://onehumancorp.io/web-session".to_string(),
    });
    req.extensions_mut()
        .insert(::ohc_core::extensions::WorkspaceScope {
            workspace_id: organization_id,
            actor_id: canonical_user_id,
            roles: claims.roles.clone(),
        });
    req.extensions_mut().insert(claims);
    next.run(req).await
}

pub async fn api_key_auth_middleware(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    authenticate_api_key_request(None, req, next).await
}

pub async fn api_key_auth_middleware_with_context(
    axum::extract::State(tenant_ctx): axum::extract::State<
        std::sync::Arc<dyn ::ohc_core::extensions::TenantContextProvider>,
    >,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    authenticate_api_key_request(Some(tenant_ctx), req, next).await
}

async fn authenticate_api_key_request(
    tenant_ctx: Option<std::sync::Arc<dyn ::ohc_core::extensions::TenantContextProvider>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    use sha2::Digest;

    let mut authorization_values = req
        .headers()
        .get_all(axum::http::header::AUTHORIZATION)
        .iter();
    let token = authorization_values
        .next()
        .filter(|_| authorization_values.next().is_none())
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| {
            !token.is_empty()
                && !token
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_ascii_control())
        });

    let Some(token) = token else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };

    let key_hash = format!("{:x}", sha2::Sha256::digest(token.as_bytes()));

    let mut matched_member_id = None;
    let mut matched_org_id = None;

    let has_db = crate::http::has_postgres_db();
    if has_db {
        let pool = crate::db::get_pool();
        if let Ok(mut tx) = pool.begin().await {
            let system_ready = if let Some(ctx) = tenant_ctx.as_ref() {
                ctx.bind_pg_system_transaction(&mut tx).await.is_ok()
            } else {
                true
            };
            if system_ready {
                if let Ok(Some(row)) = sqlx::query(
                    "SELECT k.member_id::text, k.organization_id \
                     FROM api_keys k \
                     JOIN users u ON u.id = k.member_id AND u.tenant_id = k.organization_id \
                     WHERE k.key_hash = $1 \
                       AND u.active = TRUE \
                       AND (k.expires_at IS NULL OR k.expires_at > NOW()) \
                       AND k.revoked_at IS NULL",
                )
                .bind(&key_hash)
                .fetch_optional(&mut *tx)
                .await
                {
                    use sqlx::Row;
                    let member_id: String = row.get(0);
                    let organization_id: String = row.get(1);
                    matched_member_id = Some(member_id);
                    matched_org_id = Some(organization_id);
                }
            }
            let _ = tx.rollback().await;
        }
    } else {
        let now = Utc::now();
        let keys = crate::http::get_in_memory_keys()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(key) = keys.iter().find(|k| {
            if k.key_hash != key_hash {
                return false;
            }
            let lifecycle = crate::http::in_memory_key_lifecycle(&k.key_hash);
            !lifecycle.revoked && lifecycle.expires_at.map(|exp| exp > now).unwrap_or(true)
        }) {
            matched_member_id = Some(key.member_id.clone());
            matched_org_id = Some(key.organization_id.clone());
        }
    }

    let (Some(member_id), Some(organization_id)) = (matched_member_id, matched_org_id) else {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    };
    let trimmed_org = organization_id.trim();
    if trimmed_org.is_empty() || trimmed_org.eq_ignore_ascii_case("system") {
        return axum::http::StatusCode::UNAUTHORIZED.into_response();
    }

    req.extensions_mut().insert(crate::orchestration::AuthInfo {
        org_id: trimmed_org.to_string(),
        agent_id: member_id.clone(),
        spiffe_id: "spiffe://onehumancorp.io/api-key".to_string(),
    });
    req.extensions_mut()
        .insert(::ohc_core::extensions::WorkspaceScope {
            workspace_id: trimmed_org.to_string(),
            actor_id: member_id,
            roles: Vec::new(),
        });

    next.run(req).await
}
use std::sync::Arc;
use std::sync::RwLock;

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub const ROLE_PLATFORM_OPERATOR: &str = "PLATFORM_OPERATOR";
pub const ROLE_ADMIN: &str = "ADMIN";
pub const ROLE_OPERATOR: &str = "OPERATOR";
pub const ROLE_VIEWER: &str = "VIEWER";
const GRPC_LOGIN_RATE_LIMIT_ATTEMPTS: u32 = 20;
const GRPC_LOGIN_RATE_LIMIT_WINDOW_SECS: i64 = 300;
const GRPC_LOGIN_RATE_LIMIT_MAX_ENTRIES: usize = 4096;

static ACTIVE_STORE_JWT_SECRET: RwLock<Option<Vec<u8>>> = RwLock::new(None);

fn read_existing_standalone_jwt_secret(secret_path: &std::path::Path) -> Option<Vec<u8>> {
    if !secret_path.exists() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(target_os = "linux")]
        options.custom_flags(0x00020000); // O_NOFOLLOW
        #[cfg(target_os = "macos")]
        options.custom_flags(0x0100); // O_NOFOLLOW
        if let Ok(mut file) = options.open(secret_path) {
            let mut bytes = Vec::new();
            if file.read_to_end(&mut bytes).is_ok() && bytes.len() >= 32 {
                return Some(bytes);
            }
        }
        None
    }
    #[cfg(not(unix))]
    {
        std::fs::read(secret_path)
            .ok()
            .filter(|bytes| bytes.len() >= 32)
    }
}

/// Resolves the shared JWT signing secret fail-closed from explicit environment/file
/// sources (`JWT_SECRET`/`JWT_SECRET_FILE`, `OMNISOLO_JWT_SECRET`/`OMNISOLO_JWT_SECRET_FILE`),
/// the active `Store` secret initialized at server startup, or the persistent standalone
/// secret file. Never falls back to a hardcoded secret.
pub fn resolve_configured_jwt_secret() -> Result<Vec<u8>, String> {
    if let Some(secret) =
        ::server_common::secret_source::load_optional_secret("JWT_SECRET", "JWT_SECRET_FILE")
            .map_err(|_| "invalid authentication secret configuration".to_string())?
    {
        return Ok(secret);
    }
    if let Some(secret) = ::server_common::secret_source::load_optional_secret(
        "OMNISOLO_JWT_SECRET",
        "OMNISOLO_JWT_SECRET_FILE",
    )
    .map_err(|_| "invalid authentication secret configuration".to_string())?
    {
        return Ok(secret);
    }
    if let Ok(guard) = ACTIVE_STORE_JWT_SECRET.read()
        && let Some(secret) = guard.as_ref()
        && !secret.is_empty()
    {
        return Ok(secret.clone());
    }
    let secret_path = ::server_config::jwt_secret_path();
    if let Some(bytes) = read_existing_standalone_jwt_secret(&secret_path) {
        return Ok(bytes);
    }
    Err("JWT authentication secret is not configured".to_string())
}

struct GrpcLoginRateLimiter {
    entries: HashMap<String, (i64, u32)>,
    max_attempts: u32,
    window_secs: i64,
    max_entries: usize,
}

impl GrpcLoginRateLimiter {
    fn new(max_attempts: u32, window_secs: i64, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_attempts,
            window_secs,
            max_entries,
        }
    }

    fn check_and_record(&mut self, account_key: &str, now: i64) -> Result<(), i64> {
        self.entries
            .retain(|_, (started_at, _)| now.saturating_sub(*started_at) < self.window_secs);
        if let Some((started_at, attempts)) = self.entries.get_mut(account_key) {
            if *attempts >= self.max_attempts {
                let retry = self
                    .window_secs
                    .saturating_sub(now.saturating_sub(*started_at))
                    .max(1);
                return Err(retry);
            }
            *attempts = attempts.saturating_add(1);
            return Ok(());
        }
        if self.entries.len() >= self.max_entries {
            return Err(self.window_secs.max(1));
        }
        self.entries.insert(account_key.to_string(), (now, 1));
        Ok(())
    }
}
pub const DEFAULT_COST: u32 = 10;
pub const MAX_ACCESS_TOKEN_BYTES: usize = 2048;
pub const MAX_AUTH_USER_ID_BYTES: usize = 128;
pub const MAX_AUTH_USERNAME_BYTES: usize = 128;
pub const MAX_AUTH_EMAIL_BYTES: usize = 254;
pub const MAX_AUTH_ORGANIZATION_BYTES: usize = 128;
pub const MAX_AUTH_ROLES: usize = 32;
pub const MAX_AUTH_ROLE_BYTES: usize = 64;
const MAX_ACCESS_TOKEN_CLAIMS_BYTES: usize = 1400;
const MAX_PASSWORD_HASH_CONCURRENCY: usize = 16;
const DUMMY_PASSWORD_HASH: &str = "$2b$10$dVqpE7hfDSB6wqVCcDvUGuLwuaGMsiC.FznSYrakLDs.8jQXm2wMC";

fn hash(password: String, cost: u32) -> Result<String, String> {
    bcrypt::hash(password, cost).map_err(|e| e.to_string())
}

fn verify(password: &str, hash: &str) -> Result<bool, String> {
    bcrypt::verify(password, hash).map_err(|e| e.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthenticationError {
    InvalidCredentials,
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogoutError {
    InvalidToken,
    ValidationUnavailable,
    RevocationUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenValidationError {
    Invalid(String),
    Unavailable,
}

impl TokenValidationError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

impl std::fmt::Display for TokenValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::Unavailable => formatter.write_str("token validation unavailable"),
        }
    }
}

impl std::fmt::Display for AuthenticationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidCredentials => formatter.write_str("invalid credentials"),
            Self::Unavailable(_) => formatter.write_str("authentication unavailable"),
        }
    }
}

impl std::error::Error for AuthenticationError {}

#[async_trait::async_trait]
pub trait TokenRevocationCache: Send + Sync {
    async fn mark_revoked(&self, org_id: &str, jti: &str, ttl_secs: u64) -> Result<(), String>;
    async fn is_revoked(&self, org_id: &str, jti: &str) -> Result<bool, String>;
}

fn decide_credentials_with<F>(
    candidate: Option<User>,
    password: &str,
    org_id: &str,
    mut verifier: F,
) -> Result<User, AuthenticationError>
where
    F: FnMut(&str, &str) -> Result<bool, String>,
{
    let candidate = candidate.filter(|user| {
        user.active
            && match user.organization_id.as_deref() {
                Some(user_org) => user_org == org_id,
                None => org_id.is_empty(),
            }
    });
    let password_hash = candidate
        .as_ref()
        .map(|user| user.password_hash.as_str())
        .unwrap_or(DUMMY_PASSWORD_HASH);
    let verified = verifier(password, password_hash).map_err(AuthenticationError::Unavailable)?;
    match (candidate, verified) {
        (Some(user), true) => Ok(user),
        _ => Err(AuthenticationError::InvalidCredentials),
    }
}

use ::server_common::Claims;
use ::server_omnisolo::orchestration::auth_service_server::AuthService;
use ::server_omnisolo::orchestration::*;
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tonic::{Request, Response, Status};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub email: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub roles: Vec<String>,
    pub active: bool,
    pub organization_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub permissions: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct TenantKey {
    pub org_id: String,
    pub key: String,
}

pub struct Store {
    users: RwLock<HashMap<String, User>>,
    by_name: RwLock<HashMap<TenantKey, String>>,
    by_email: RwLock<HashMap<TenantKey, String>>,
    revoked: RwLock<HashMap<String, DateTime<Utc>>>,
    revocation_cache: Option<Arc<dyn TokenRevocationCache>>,
    require_workspace_id: bool,
    secret: Vec<u8>,
    password_slots: Arc<tokio::sync::Semaphore>,
    user_creation_lock: tokio::sync::Mutex<()>,
    grpc_login_limiter: std::sync::Mutex<GrpcLoginRateLimiter>,
    repo: Option<std::sync::Arc<dyn crate::user_repository::UserRepository>>,
    portable_repo: Option<std::sync::Arc<crate::seaorm_store::SeaOrmAuthRepository>>,
}

impl Store {
    pub fn jwt_secret(&self) -> &[u8] {
        &self.secret
    }

    async fn hash_password_off_executor(&self, password: String) -> Result<String, String> {
        let _slot = self
            .password_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "password hashing unavailable".to_string())?;
        let cost = if cfg!(test) { 4 } else { DEFAULT_COST };
        tokio::task::spawn_blocking(move || hash(password, cost))
            .await
            .map_err(|_| "password hashing failed".to_string())?
    }

    pub fn check_grpc_login_rate_limit(&self, org_id: &str, identifier: &str) -> Result<(), i64> {
        let key = format!(
            "{}\0{}",
            org_id.trim().to_ascii_lowercase(),
            identifier.trim().to_ascii_lowercase()
        );
        let now = Utc::now().timestamp();
        let mut limiter = self
            .grpc_login_limiter
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        limiter.check_and_record(&key, now)
    }

    pub async fn check_self_registration_allowed(&self) -> Result<(), String> {
        if let Some(repository) = self.portable_repo() {
            let mode = repository
                .registration_mode()
                .await
                .map_err(|_| "registration unavailable".to_string())?;
            return match mode {
                crate::seaorm_store::RegistrationMode::Open => Ok(()),
                crate::seaorm_store::RegistrationMode::InviteOnly => {
                    Err("invitation required".to_string())
                }
                crate::seaorm_store::RegistrationMode::Closed => {
                    Err("registration closed".to_string())
                }
            };
        }
        if self.require_workspace_id {
            return Err("registration closed".to_string());
        }
        Ok(())
    }
    pub fn registration_hash_key(&self) -> [u8; 32] {
        let mut mac = HmacSha256::new_from_slice(&self.secret)
            .expect("HMAC accepts authentication secret lengths");
        mac.update(b"onehumancorp-registration-v1");
        mac.finalize().into_bytes().into()
    }

    fn access_token_claims(user: &User, issued_at: i64, expires_at: i64, jti: String) -> Claims {
        Claims {
            sub: user.id.clone(),
            username: user.username.clone(),
            email: user.email.clone(),
            roles: user.roles.clone(),
            organization_id: user.organization_id.clone(),
            session_id: None,
            iat: issued_at,
            exp: expires_at,
            jti,
        }
    }

    fn access_token_claims_fit(claims: &Claims) -> bool {
        serde_json::to_vec(claims)
            .is_ok_and(|payload| payload.len() <= MAX_ACCESS_TOKEN_CLAIMS_BYTES)
    }

    pub fn user_access_token_fits(user: &User) -> bool {
        if user.id.is_empty()
            || user.id.len() > MAX_AUTH_USER_ID_BYTES
            || user.username.is_empty()
            || user.username.len() > MAX_AUTH_USERNAME_BYTES
            || user.email.is_empty()
            || user.email.len() > MAX_AUTH_EMAIL_BYTES
            || user
                .organization_id
                .as_deref()
                .is_some_and(|organization| organization.len() > MAX_AUTH_ORGANIZATION_BYTES)
            || user.roles.len() > MAX_AUTH_ROLES
            || user
                .roles
                .iter()
                .any(|role| role.is_empty() || role.len() > MAX_AUTH_ROLE_BYTES)
        {
            return false;
        }
        let issued_at = chrono::Utc::now().timestamp();
        Self::access_token_claims_fit(&Self::access_token_claims(
            user,
            issued_at,
            issued_at.saturating_add(24 * 60 * 60),
            "0".repeat(32),
        ))
    }

    pub fn new() -> Self {
        let configured_secret =
            ::server_common::secret_source::load_optional_secret("JWT_SECRET", "JWT_SECRET_FILE")
                .unwrap_or_else(|_| panic!("invalid authentication secret configuration"));
        let secret = configured_secret.unwrap_or_else(|| {
                let secret_path = ::server_config::jwt_secret_path();
                if secret_path.exists() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::OpenOptionsExt;
                        use std::os::unix::fs::PermissionsExt;
                        let mut options = std::fs::OpenOptions::new();
                        options.read(true);
                        #[cfg(target_os = "linux")]
                        options.custom_flags(0x00020000); // O_NOFOLLOW
                        #[cfg(target_os = "macos")]
                        options.custom_flags(0x0100); // O_NOFOLLOW

                        if let Ok(mut file) = options.open(&secret_path) {
                            if let Ok(metadata) = file.metadata() {
                                let mut perms = metadata.permissions();
                                if perms.mode() & 0o777 != 0o600 {
                                    tracing::warn!("Insecure permissions on the OmniSolo JWT secret. Fixing them to prevent TOCTOU attacks.");
                                    perms.set_mode(0o600);
                                    if file.set_permissions(perms).is_err() {
                                        tracing::error!("Failed to securely update OmniSolo JWT secret permissions");
                                        std::process::exit(1);
                                    }
                                }
                            }
                            use std::io::Read;
                            let mut bytes = Vec::new();
                            if file.read_to_end(&mut bytes).is_ok() && bytes.len() >= 32 {
                                return bytes;
                            }
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        if let Ok(bytes) = std::fs::read(&secret_path) {
                            if bytes.len() >= 32 {
                                return bytes;
                            }
                        }
                    }
                }

                let sqlite_key_opt = std::env::var("OMNISOLO_SQLITE_KEY").ok().or_else(|| {
                    let secret_path = ::server_config::sqlite_key_path();
                    if secret_path.exists() {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::OpenOptionsExt;
                            use std::os::unix::fs::PermissionsExt;
                            let mut options = std::fs::OpenOptions::new();
                            options.read(true);
                        #[cfg(target_os = "linux")]
                        options.custom_flags(0x00020000); // O_NOFOLLOW
                        #[cfg(target_os = "macos")]
                        options.custom_flags(0x0100); // O_NOFOLLOW

                            if let Ok(mut file) = options.open(&secret_path) {
                                if let Ok(metadata) = file.metadata() {
                                    let mut perms = metadata.permissions();
                                    if perms.mode() & 0o777 != 0o600 {
                                        tracing::warn!("Insecure permissions on the OmniSolo SQLite key. Fixing them to prevent TOCTOU attacks.");
                                        perms.set_mode(0o600);
                                        if file.set_permissions(perms).is_err() {
                                            tracing::error!("Failed to securely update OmniSolo SQLite key permissions");
                                            std::process::exit(1);
                                        }
                                    }
                                }
                                use std::io::Read;
                                let mut bytes = String::new();
                                if file.read_to_string(&mut bytes).is_ok() && !bytes.trim().is_empty() {
                                    return Some(bytes.trim().to_string());
                                }
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            if let Ok(bytes) = std::fs::read_to_string(&secret_path) {
                                if !bytes.trim().is_empty() {
                                    return Some(bytes.trim().to_string());
                                }
                            }
                        }
                    }
                    None
                });

                let new_secret = if let Some(sqlite_key) = sqlite_key_opt {
                    tracing::debug!("falling back to a generated JWT secret derived from OMNISOLO_SQLITE_KEY for persistent standalone authentication"); // pii-safe
                    let mut mac = HmacSha256::new_from_slice(b"ohc_jwt_derivation_salt").expect("HMAC can take key of any size");
                    mac.update(sqlite_key.as_bytes());
                    mac.finalize().into_bytes().to_vec()
                } else {
                    tracing::debug!("falling back to a generated persistent OmniSolo JWT secret"); // pii-safe
                    let mut key_bytes = [0u8; 32];
                    use rand::RngCore;
                    rand::thread_rng().fill_bytes(&mut key_bytes);
                    key_bytes.to_vec()
                };

                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    use std::io::Write;

                    let mut options = std::fs::OpenOptions::new();
                    options.read(true).write(true).create_new(true).mode(0o600);
                    #[cfg(target_os = "linux")]
                    options.custom_flags(0x00020000); // O_NOFOLLOW
                    #[cfg(target_os = "macos")]
                    options.custom_flags(0x0100); // O_NOFOLLOW

                    if let Ok(mut file) = options.open(&secret_path) {
                        let _ = file.write_all(&new_secret);
                    }

                }
                #[cfg(not(unix))]
                {
                    let _ = std::fs::write(&secret_path, &new_secret);
                }

                new_secret
            });

        Self::from_resolved_secret(secret)
    }

    fn from_resolved_secret(secret: Vec<u8>) -> Self {
        let mut roles = HashMap::new();
        let now = Utc::now();

        roles.insert(
            ROLE_ADMIN.to_string(),
            Role {
                id: ROLE_ADMIN.to_string(),
                name: ROLE_ADMIN.to_string(),
                permissions: vec!["*".to_string()],
                created_at: now,
            },
        );
        roles.insert(
            ROLE_OPERATOR.to_string(),
            Role {
                id: ROLE_OPERATOR.to_string(),
                name: ROLE_OPERATOR.to_string(),
                permissions: vec!["read".to_string(), "write".to_string()],
                created_at: now,
            },
        );
        roles.insert(
            ROLE_VIEWER.to_string(),
            Role {
                id: ROLE_VIEWER.to_string(),
                name: ROLE_VIEWER.to_string(),
                permissions: vec!["read".to_string()],
                created_at: now,
            },
        );

        let store = Store {
            users: RwLock::new(HashMap::new()),
            by_name: RwLock::new(HashMap::new()),
            by_email: RwLock::new(HashMap::new()),
            revoked: RwLock::new(HashMap::new()),
            revocation_cache: None,
            require_workspace_id: false,
            secret,
            password_slots: Arc::new(tokio::sync::Semaphore::new(MAX_PASSWORD_HASH_CONCURRENCY)),
            user_creation_lock: tokio::sync::Mutex::new(()),
            grpc_login_limiter: std::sync::Mutex::new(GrpcLoginRateLimiter::new(
                GRPC_LOGIN_RATE_LIMIT_ATTEMPTS,
                GRPC_LOGIN_RATE_LIMIT_WINDOW_SECS,
                GRPC_LOGIN_RATE_LIMIT_MAX_ENTRIES,
            )),
            repo: None,
            portable_repo: None,
        };

        store.seed_default_admin(now);

        store
    }

    pub fn with_revocation_cache(mut self, cache: Arc<dyn TokenRevocationCache>) -> Self {
        self.revocation_cache = Some(cache);
        self
    }

    pub fn with_require_workspace_id(mut self, required: bool) -> Self {
        self.require_workspace_id = required;
        self
    }

    pub fn register_active_jwt_secret(&self) {
        if !self.secret.is_empty()
            && let Ok(mut guard) = ACTIVE_STORE_JWT_SECRET.write()
        {
            *guard = Some(self.secret.clone());
        }
    }

    fn seed_default_admin(&self, now: DateTime<Utc>) {
        let admin_user = std::env::var("ADMIN_USERNAME").unwrap_or_else(|_| "admin".to_string());
        let admin_pass = std::env::var("ADMIN_PASSWORD").unwrap_or_else(|_| "admin".to_string());
        let admin_email =
            std::env::var("ADMIN_EMAIL").unwrap_or_else(|_| "admin@localhost".to_string());

        let hash = hash(admin_pass, if cfg!(test) { 4 } else { DEFAULT_COST })
            .expect("Failed to hash password");

        let id = hex::encode(random_bytes(8));

        let admin = User {
            id: id.clone(),
            username: admin_user.clone(),
            email: admin_email.clone(),
            password_hash: hash,
            roles: vec![ROLE_ADMIN.to_string()],
            active: true,
            organization_id: None,
            created_at: now,
            updated_at: now,
        };

        self.users
            .write()
            .expect("Failed to acquire lock")
            .insert(id.clone(), admin);
        self.by_name
            .write()
            .expect("Failed to acquire lock")
            .insert(
                TenantKey {
                    org_id: "".to_string(),
                    key: admin_user,
                },
                id.clone(),
            );
        self.by_email
            .write()
            .expect("Failed to acquire lock")
            .insert(
                TenantKey {
                    org_id: "".to_string(),
                    key: admin_email,
                },
                id,
            );
    }

    pub fn with_repo(repo: std::sync::Arc<dyn crate::user_repository::UserRepository>) -> Self {
        let mut store = Store::new();
        store.repo = Some(repo);
        store
    }

    pub fn with_portable_repo(
        repo: std::sync::Arc<crate::seaorm_store::SeaOrmAuthRepository>,
    ) -> Self {
        let mut store = Store::new();
        store.repo = Some(repo.clone());
        store.portable_repo = Some(repo);
        store
    }

    pub fn reopen_portable_repo(
        &self,
        repo: std::sync::Arc<crate::seaorm_store::SeaOrmAuthRepository>,
    ) -> Self {
        let mut store = Self::from_resolved_secret(self.secret.clone());
        store.repo = Some(repo.clone());
        store.portable_repo = Some(repo);
        store
    }

    pub fn portable_repo(
        &self,
    ) -> Option<std::sync::Arc<crate::seaorm_store::SeaOrmAuthRepository>> {
        self.portable_repo.clone()
    }

    pub async fn create_user(
        &self,
        username: String,
        email: String,
        password: String,
        roles: Vec<String>,
        org_id: String,
    ) -> Result<User, String> {
        self.validate_org_id(&org_id)?;
        if username.is_empty() {
            return Err("username is required".to_string());
        }
        if password.len() < 6 {
            return Err("password must be at least 6 characters".to_string());
        }

        // Compute bcrypt off the Tokio executor BEFORE acquiring `user_creation_lock`
        // so concurrent signups never serialize across CPU-bound password hashing.
        let password_hash = self.hash_password_off_executor(password).await?;

        let _creation_guard = self.user_creation_lock.lock().await;

        let name_key = TenantKey {
            org_id: org_id.clone(),
            key: username.clone(),
        };
        if self
            .by_name
            .read()
            .expect("Failed to acquire lock")
            .contains_key(&name_key)
        {
            return Err("username already taken".to_string());
        }

        let email_key = TenantKey {
            org_id: org_id.clone(),
            key: email.clone(),
        };
        if self
            .by_email
            .read()
            .expect("Failed to acquire lock")
            .contains_key(&email_key)
        {
            return Err("email already registered".to_string());
        }

        let id = hex::encode(random_bytes(8));
        let now = Utc::now();

        let user = User {
            id: id.clone(),
            username,
            email,
            password_hash,
            roles,
            active: true,
            organization_id: Some(org_id.clone()),
            created_at: now,
            updated_at: now,
        };

        if !Self::user_access_token_fits(&user) {
            return Err("user claims exceed the access token size limit".to_string());
        }

        if let Some(repo) = &self.repo {
            repo.create_user(user.clone(), &org_id).await?;
        }

        self.users
            .write()
            .expect("Failed to acquire lock")
            .insert(id.clone(), user.clone());
        self.by_name
            .write()
            .expect("Failed to acquire lock")
            .insert(name_key, id.clone());
        self.by_email
            .write()
            .expect("Failed to acquire lock")
            .insert(email_key, id);

        Ok(user)
    }

    pub async fn create_verified_user(
        &self,
        username: String,
        email: String,
        password: String,
        roles: Vec<String>,
        ticket_hash: &str,
    ) -> Result<User, String> {
        let repository = self
            .portable_repo
            .as_ref()
            .ok_or_else(|| "registration persistence unavailable".to_string())?;

        // Compute bcrypt off the Tokio executor BEFORE acquiring `user_creation_lock`.
        let password_hash = self.hash_password_off_executor(password).await?;

        let _creation_guard = self.user_creation_lock.lock().await;

        let org_id = uuid::Uuid::new_v4().to_string();
        let name_key = TenantKey {
            org_id: org_id.clone(),
            key: username.clone(),
        };
        let email_key = TenantKey {
            org_id: org_id.clone(),
            key: email.clone(),
        };
        if self
            .by_name
            .read()
            .expect("Failed to acquire lock")
            .contains_key(&name_key)
            || self
                .by_email
                .read()
                .expect("Failed to acquire lock")
                .contains_key(&email_key)
        {
            return Err("account already exists".to_string());
        }

        let id = hex::encode(random_bytes(8));
        let now = Utc::now();
        let user = User {
            id: id.clone(),
            username,
            email,
            password_hash,
            roles,
            active: true,
            organization_id: Some(org_id),
            created_at: now,
            updated_at: now,
        };
        if !Self::user_access_token_fits(&user) {
            return Err("user claims exceed the access token size limit".to_string());
        }

        let user = repository
            .consume_ticket_and_create_user(ticket_hash, now, user)
            .await?;
        let org_id = user.organization_id.clone().unwrap_or_default();
        let name_key = TenantKey {
            org_id: org_id.clone(),
            key: user.username.clone(),
        };
        let email_key = TenantKey {
            org_id,
            key: user.email.clone(),
        };
        self.users
            .write()
            .expect("Failed to acquire lock")
            .insert(id.clone(), user.clone());
        self.by_name
            .write()
            .expect("Failed to acquire lock")
            .insert(name_key, id.clone());
        self.by_email
            .write()
            .expect("Failed to acquire lock")
            .insert(email_key, id);
        Ok(user)
    }

    pub async fn authenticate(
        &self,
        identifier: &str,
        password: &str,
        org_id: &str,
    ) -> Result<User, AuthenticationError> {
        let _password_slot = self
            .password_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| AuthenticationError::Unavailable("credential verifier busy".into()))?;
        if self.validate_org_id(org_id).is_err() {
            let password = password.to_string();
            let org_id = org_id.to_string();
            return tokio::task::spawn_blocking(move || {
                decide_credentials_with(None, &password, &org_id, verify)
            })
            .await
            .map_err(|_| {
                AuthenticationError::Unavailable("credential verifier unavailable".into())
            })?;
        }

        let lookup_result = if let Some(repo) = &self.repo {
            repo.get_by_login_identifier(identifier, org_id).await
        } else {
            Ok(self.find_memory_user_with(identifier, org_id, || {}))
        };

        let (candidate, lookup_error) = match lookup_result {
            Ok(candidate) => (candidate, None),
            Err(error) => (None, Some(error)),
        };
        let password = password.to_string();
        let org_id = org_id.to_string();
        let decision = tokio::task::spawn_blocking(move || {
            decide_credentials_with(candidate, &password, &org_id, verify)
        })
        .await
        .map_err(|_| AuthenticationError::Unavailable("credential verifier unavailable".into()))?;

        if let Some(error) = lookup_error {
            return Err(AuthenticationError::Unavailable(error));
        }
        decision
    }

    pub async fn authenticate_dummy(&self, password: &str) -> Result<User, AuthenticationError> {
        let _password_slot = self
            .password_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| AuthenticationError::Unavailable("credential verifier busy".into()))?;
        let password = password.to_string();
        tokio::task::spawn_blocking(move || decide_credentials_with(None, &password, "", verify))
            .await
            .map_err(|_| {
                AuthenticationError::Unavailable("credential verifier unavailable".into())
            })?
    }

    fn find_memory_user_with<F>(
        &self,
        identifier: &str,
        org_id: &str,
        after_index_lookup: F,
    ) -> Option<User>
    where
        F: FnOnce(),
    {
        let user_id = {
            let by_name = self.by_name.read().expect("Failed to acquire lock");
            let by_email = self.by_email.read().expect("Failed to acquire lock");
            let key = TenantKey {
                org_id: org_id.to_string(),
                key: identifier.to_string(),
            };
            match (by_name.get(&key), by_email.get(&key)) {
                (Some(name_id), Some(email_id)) if name_id != email_id => None,
                (Some(id), _) | (_, Some(id)) => Some(id.clone()),
                (None, None) => None,
            }
        };

        after_index_lookup();
        let users = self.users.read().expect("Failed to acquire lock");
        user_id.and_then(|id| users.get(&id).cloned())
    }

    fn validate_org_id(&self, org_id: &str) -> Result<(), String> {
        if org_id.trim().eq_ignore_ascii_case("system") {
            return Err("tenant_id 'system' cannot be queried directly".into());
        }
        if self.require_workspace_id && org_id.trim().is_empty() {
            return Err("empty tenant_id is not allowed".into());
        }
        Ok(())
    }

    pub async fn get_user(&self, id: &str, org_id: &str) -> Option<User> {
        if self.validate_org_id(org_id).is_err() {
            return None;
        }

        if let Some(repo) = &self.repo {
            return repo.get_by_id(id, org_id).await.ok();
        }

        let users = self.users.read().expect("Failed to acquire lock");
        let u = users.get(id)?;

        if let Some(ref user_org) = u.organization_id {
            if user_org != org_id {
                return None;
            }
        } else if !org_id.is_empty() {
            return None;
        }
        Some(u.clone())
    }

    pub async fn list_users(&self, org_id: &str) -> Vec<User> {
        if self.validate_org_id(org_id).is_err() {
            return vec![];
        }

        if let Some(repo) = &self.repo {
            return repo.list_users(org_id).await.unwrap_or_default();
        }

        let users = self.users.read().expect("Failed to acquire lock");
        users
            .values()
            .filter(|u| {
                if org_id.is_empty() {
                    u.organization_id.is_none() || u.organization_id.as_deref() == Some("")
                } else {
                    u.organization_id.as_deref() == Some(org_id)
                }
            })
            .cloned()
            .collect()
    }

    pub async fn update_user(
        &self,
        id: &str,
        email_ptr: Option<String>,
        roles: Option<Vec<String>>,
        active_ptr: Option<bool>,
        org_id: &str,
    ) -> Result<User, String> {
        self.validate_org_id(org_id)?;

        if let Some(repo) = &self.repo {
            let mut u = repo.get_by_id(id, org_id).await?;
            if let Some(email) = email_ptr {
                u.email = email;
            }
            if let Some(r) = roles {
                u.roles = r;
            }
            if let Some(active) = active_ptr {
                u.active = active;
            }
            if !Self::user_access_token_fits(&u) {
                return Err("user claims exceed the access token size limit".to_string());
            }
            u.updated_at = chrono::Utc::now();
            repo.update_user(u.clone(), org_id).await?;
            return Ok(u);
        }

        let mut users = self.users.write().expect("Failed to acquire lock");
        let mut by_email = self.by_email.write().expect("Failed to acquire lock");

        let u = users
            .get_mut(id)
            .ok_or_else(|| "user not found".to_string())?;

        if let Some(ref user_org) = u.organization_id {
            if user_org != org_id {
                return Err("user not found".to_string());
            }
        } else if !org_id.is_empty() {
            return Err("user not found".to_string());
        }

        let mut candidate = u.clone();
        if let Some(email) = email_ptr.as_ref() {
            candidate.email = email.clone();
        }
        if let Some(roles) = roles.as_ref() {
            candidate.roles = roles.clone();
        }
        if let Some(active) = active_ptr {
            candidate.active = active;
        }
        if !Self::user_access_token_fits(&candidate) {
            return Err("user claims exceed the access token size limit".to_string());
        }

        if let Some(email) = email_ptr {
            if email != u.email {
                let org = u.organization_id.clone().unwrap_or_default();
                let email_key = TenantKey {
                    org_id: org,
                    key: email.clone(),
                };
                if by_email.contains_key(&email_key) {
                    return Err("email already registered".to_string());
                }
                by_email.remove(&TenantKey {
                    org_id: u.organization_id.clone().unwrap_or_default(),
                    key: u.email.clone(),
                });
                u.email = email;
                by_email.insert(email_key, id.to_string());
            }
        }

        if let Some(r) = roles {
            u.roles = r;
        }

        if let Some(active) = active_ptr {
            u.active = active;
        }

        u.updated_at = Utc::now();

        Ok(u.clone())
    }

    pub async fn delete_user(&self, id: &str, org_id: &str) -> Result<(), String> {
        self.validate_org_id(org_id)?;

        if let Some(repo) = &self.repo {
            return repo.delete_user(id, org_id).await;
        }

        let mut users = self.users.write().expect("Failed to acquire lock");
        let mut by_name = self.by_name.write().expect("Failed to acquire lock");
        let mut by_email = self.by_email.write().expect("Failed to acquire lock");

        let u = users.get(id).ok_or_else(|| "user not found".to_string())?;

        if let Some(ref user_org) = u.organization_id {
            if user_org != org_id {
                return Err("user not found".to_string());
            }
        } else if !org_id.is_empty() {
            return Err("user not found".to_string());
        }

        let org = u.organization_id.clone().unwrap_or_default();
        by_name.remove(&TenantKey {
            org_id: org.clone(),
            key: u.username.clone(),
        });
        by_email.remove(&TenantKey {
            org_id: org,
            key: u.email.clone(),
        });

        users.remove(id);

        Ok(())
    }

    pub async fn revoke_token(
        &self,
        jti: String,
        exp: DateTime<Utc>,
        org_id: &str,
    ) -> Result<(), String> {
        self.validate_org_id(org_id)?;

        if let Some(repo) = &self.repo {
            repo.revoke_token(jti.clone(), exp, org_id).await?;
        }

        {
            let mut revoked = self.revoked.write().expect("Failed to acquire lock");
            revoked.insert(format!("{}:{}", org_id, jti), exp);

            let now = Utc::now();
            revoked.retain(|_, v| *v > now);
        }
        if let Some(cache) = &self.revocation_cache {
            let ttl = (exp.timestamp() - Utc::now().timestamp()).max(1) as u64;
            cache.mark_revoked(org_id, &jti, ttl).await?;
        }
        Ok(())
    }

    pub async fn is_revoked(&self, jti: &str, org_id: &str) -> Result<bool, String> {
        self.validate_org_id(org_id)?;

        if let Some(repo) = &self.repo {
            if repo.is_revoked(jti, org_id).await? {
                return Ok(true);
            }
        }

        {
            let revoked = self.revoked.read().expect("Failed to acquire lock");
            if let Some(exp) = revoked.get(&format!("{}:{}", org_id, jti)) {
                if *exp > Utc::now() {
                    return Ok(true);
                }
            }
        }
        if let Some(cache) = &self.revocation_cache {
            if cache.is_revoked(org_id, jti).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn issue_token_with_expiry(&self, _user: &User) -> Result<(String, i64), String> {
        let now = chrono::Utc::now();
        let expires_at = (now + chrono::Duration::hours(24)).timestamp();
        let claims = Self::access_token_claims(
            _user,
            now.timestamp(),
            expires_at,
            hex::encode(random_bytes(16)),
        );
        if !Self::access_token_claims_fit(&claims) {
            return Err("access token claims exceed the configured size limit".to_string());
        }

        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        let token = jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(&self.secret),
        )
        .map_err(|e| e.to_string())?;
        if token.len() > MAX_ACCESS_TOKEN_BYTES {
            return Err("access token exceeds the configured size limit".to_string());
        }

        Ok((token, expires_at))
    }

    pub fn issue_token(&self, user: &User) -> Result<String, String> {
        self.issue_token_with_expiry(user).map(|(token, _)| token)
    }

    pub async fn validate_token(&self, _token: &str) -> Result<Claims, String> {
        self.validate_token_claims(_token, true)
            .await
            .map_err(|error| error.to_string())
    }

    /// Validate signed claims without rejecting a prior revocation, then revoke
    /// the token in the tenant named by those claims. This makes logout
    /// idempotent without trusting transport-supplied tenant data.
    pub async fn logout_token(&self, token: &str) -> Result<(), LogoutError> {
        let claims =
            self.validate_token_claims(token, false)
                .await
                .map_err(|error| match error {
                    TokenValidationError::Invalid(_) => LogoutError::InvalidToken,
                    TokenValidationError::Unavailable => LogoutError::ValidationUnavailable,
                })?;
        let exp =
            chrono::DateTime::from_timestamp(claims.exp, 0).ok_or(LogoutError::InvalidToken)?;
        let org_id = claims.organization_id.unwrap_or_default();
        self.revoke_token(claims.jti, exp, &org_id)
            .await
            .map_err(|_| LogoutError::RevocationUnavailable)
    }

    async fn validate_token_claims(
        &self,
        _token: &str,
        check_revocation: bool,
    ) -> Result<Claims, TokenValidationError> {
        let validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
        let token_data = jsonwebtoken::decode::<Claims>(
            _token,
            &jsonwebtoken::DecodingKey::from_secret(&self.secret),
            &validation,
        );

        match token_data {
            Ok(data) => {
                if data.claims.sub.trim().is_empty() || data.claims.jti.trim().is_empty() {
                    return Err(TokenValidationError::invalid("Invalid token: empty claims"));
                }
                if self.require_workspace_id
                    && data
                        .claims
                        .organization_id
                        .clone()
                        .unwrap_or_default()
                        .trim()
                        .is_empty()
                {
                    return Err(TokenValidationError::invalid(
                        "Invalid token: organization_id is required",
                    ));
                }
                if data
                    .claims
                    .organization_id
                    .as_deref()
                    .map(|s| s.eq_ignore_ascii_case("system"))
                    .unwrap_or(false)
                {
                    return Err(TokenValidationError::invalid(
                        "Invalid token: 'system' organization is forbidden",
                    ));
                }
                if check_revocation
                    && self
                        .is_revoked(
                            &data.claims.jti,
                            &data.claims.organization_id.clone().unwrap_or_default(),
                        )
                        .await
                        .map_err(|_| TokenValidationError::Unavailable)?
                {
                    return Err(TokenValidationError::invalid("token revoked"));
                }
                if data.claims.sub.trim().is_empty() || data.claims.jti.trim().is_empty() {
                    return Err(TokenValidationError::invalid("Invalid token claims"));
                }
                Ok(data.claims)
            }
            Err(_) => Err(TokenValidationError::invalid("Invalid token")),
        }
    }
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

#[derive(Clone)]
pub struct AuthServiceServerImpl {
    pub store: Arc<Store>,
    policy: Arc<dyn ::ohc_core::extensions::AuthPolicyProvider>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthRpc {
    Login,
    Register,
    Logout,
    GetMe,
    ListUsers,
    CreateUser,
    GetUser,
    UpdateUser,
    DeleteUser,
    ListRoles,
    CreateRole,
}

impl AuthRpc {
    fn is_public(self) -> bool {
        matches!(self, Self::Login | Self::Register)
    }
}

impl AuthServiceServerImpl {
    pub fn new(
        store: Arc<Store>,
        policy: Arc<dyn ::ohc_core::extensions::AuthPolicyProvider>,
    ) -> Self {
        Self { store, policy }
    }

    fn authenticate_rpc<T>(&self, rpc: AuthRpc, request: &mut Request<T>) -> Result<(), Status> {
        if rpc.is_public() {
            return Ok(());
        }

        request
            .extensions_mut()
            .remove::<::ohc_core::extensions::PeerIdentity>();
        let probe = Request::from_parts(request.metadata().clone(), tonic::Extensions::new(), ());
        let identity = self.policy.verify_grpc_peer(&probe)?;
        request.extensions_mut().insert(identity);
        Ok(())
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

fn require_peer_identity<T>(request: &Request<T>) -> Result<(String, &str), Status> {
    let identity = request
        .extensions()
        .get::<::ohc_core::extensions::PeerIdentity>()
        .ok_or_else(|| Status::unauthenticated("Missing PeerIdentity"))?;
    let org_id = identity
        .workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|org| !org.is_empty() && !org.eq_ignore_ascii_case("system"))
        .ok_or_else(|| Status::unauthenticated("Missing workspace_id on PeerIdentity"))?
        .to_string();
    Ok((org_id, identity.principal.as_str()))
}

#[tonic::async_trait]
impl AuthService for AuthServiceServerImpl {
    async fn login(
        &self,
        mut request: Request<LoginRequest>,
    ) -> Result<Response<LoginResponse>, Status> {
        self.authenticate_rpc(AuthRpc::Login, &mut request)?;
        let req = request.into_inner();

        if self
            .store
            .check_grpc_login_rate_limit(&req.organization_id, &req.username)
            .is_err()
        {
            return Err(Status::resource_exhausted("too many login attempts"));
        }

        match self
            .store
            .authenticate(&req.username, &req.password, &req.organization_id)
            .await
        {
            Ok(user) => match self.store.issue_token_with_expiry(&user) {
                Ok((token, expires_at)) => Ok(Response::new(LoginResponse { token, expires_at })),
                Err(e) => Err(Status::internal(e)),
            },
            Err(AuthenticationError::InvalidCredentials) => {
                Err(Status::unauthenticated("invalid credentials"))
            }
            Err(AuthenticationError::Unavailable(error)) => {
                let _ = error;
                tracing::error!(event = "auth.grpc.login.unavailable");
                Err(Status::unavailable("authentication unavailable"))
            }
        }
    }

    async fn register(
        &self,
        mut request: Request<CreateUserRequest>,
    ) -> Result<Response<LoginResponse>, Status> {
        self.authenticate_rpc(AuthRpc::Register, &mut request)?;
        let req = request.into_inner();

        if let Err(reason) = self.store.check_self_registration_allowed().await {
            return match reason.as_str() {
                "registration closed" | "invitation required" => {
                    Err(Status::permission_denied(reason))
                }
                _ => Err(Status::unavailable("registration unavailable")),
            };
        }

        // Never permit unauthenticated self-registration to join a caller-supplied
        // existing organization in either cloud or standalone mode.
        if !req.organization_id.trim().is_empty() {
            return Err(Status::permission_denied(
                "self-registration cannot join an existing organization",
            ));
        }

        let username = crate::validation::normalize_username(&req.username)
            .map_err(Status::invalid_argument)?;
        let email =
            crate::validation::normalize_email(&req.email).map_err(Status::invalid_argument)?;
        crate::validation::validate_password(&req.password, &username, &email)
            .map_err(Status::invalid_argument)?;

        let final_org_id = sqlx::types::Uuid::new_v4().to_string();
        let final_role = if self.policy.capabilities().mtls_peer_auth {
            ROLE_ADMIN.to_string()
        } else {
            ROLE_VIEWER.to_string()
        };

        let user = self
            .store
            .create_user(
                username,
                email,
                req.password,
                vec![final_role],
                final_org_id,
            )
            .await
            .map_err(|_| Status::invalid_argument("registration failed"))?;

        let (token, expires_at) = self
            .store
            .issue_token_with_expiry(&user)
            .map_err(|_| Status::internal("failed to issue token"))?;

        Ok(Response::new(LoginResponse { token, expires_at }))
    }

    async fn logout(
        &self,
        mut request: Request<EmptyRequest>,
    ) -> Result<Response<EmptyResponse>, Status> {
        self.authenticate_rpc(AuthRpc::Logout, &mut request)?;
        let _ = require_peer_identity(&request)?;
        let auth_header = request
            .metadata()
            .get("authorization")
            .ok_or_else(|| Status::unauthenticated("missing authorization"))?;
        let auth_str = auth_header
            .to_str()
            .map_err(|_| Status::unauthenticated("invalid authorization"))?;
        let token = auth_str
            .strip_prefix("Bearer ")
            .or_else(|| auth_str.strip_prefix("bearer "))
            .unwrap_or(auth_str);
        self.store
            .logout_token(token)
            .await
            .map_err(|error| match error {
                LogoutError::InvalidToken => Status::unauthenticated("invalid token"),
                LogoutError::ValidationUnavailable => {
                    Status::unavailable("token validation unavailable")
                }
                LogoutError::RevocationUnavailable => {
                    Status::unavailable("token revocation unavailable")
                }
            })?;
        Ok(Response::new(EmptyResponse {}))
    }

    async fn get_me(
        &self,
        mut request: Request<EmptyRequest>,
    ) -> Result<Response<UserProto>, Status> {
        self.authenticate_rpc(AuthRpc::GetMe, &mut request)?;
        let (org_id, principal) = require_peer_identity(&request)?;

        let user = self
            .store
            .get_user(principal, &org_id)
            .await
            .ok_or_else(|| Status::not_found("User not found"))?;

        Ok(Response::new(UserProto {
            id: user.id,
            username: user.username,
            email: user.email,
            roles: user.roles,
            active: user.active,
            organization_id: user.organization_id.unwrap_or_default(),
            created_at_unix: user.created_at.timestamp(),
            updated_at_unix: user.updated_at.timestamp(),
        }))
    }

    async fn list_users(
        &self,
        mut request: Request<ListUsersRequest>,
    ) -> Result<Response<ListUsersResponse>, Status> {
        self.authenticate_rpc(AuthRpc::ListUsers, &mut request)?;
        let (org_id, _) = require_peer_identity(&request)?;

        let users = self.store.list_users(&org_id).await;
        let proto_users = users
            .into_iter()
            .map(|u| UserProto {
                id: u.id,
                username: u.username,
                email: u.email,
                roles: u.roles,
                active: u.active,
                organization_id: u.organization_id.unwrap_or_default(),
                created_at_unix: u.created_at.timestamp(),
                updated_at_unix: u.updated_at.timestamp(),
            })
            .collect();
        Ok(Response::new(ListUsersResponse { users: proto_users }))
    }

    async fn create_user(
        &self,
        mut request: Request<CreateUserRequest>,
    ) -> Result<Response<UserProto>, Status> {
        self.authenticate_rpc(AuthRpc::CreateUser, &mut request)?;
        let (org_id, principal) = require_peer_identity(&request)?;

        let caller = self
            .store
            .get_user(principal, &org_id)
            .await
            .ok_or_else(|| Status::not_found("Caller not found"))?;

        if !caller.roles.contains(&"ADMIN".to_string()) {
            return Err(Status::permission_denied("Only ADMIN can create users"));
        }

        let req = request.into_inner();
        let user = self
            .store
            .create_user(
                req.username.clone(),
                req.email.clone(),
                req.password,
                vec![],
                org_id,
            )
            .await
            .map_err(|e| Status::internal(e))?;
        Ok(Response::new(UserProto {
            id: user.id,
            username: user.username,
            email: user.email,
            roles: user.roles,
            active: user.active,
            organization_id: user.organization_id.unwrap_or_default(),
            created_at_unix: user.created_at.timestamp(),
            updated_at_unix: user.updated_at.timestamp(),
        }))
    }

    async fn get_user(
        &self,
        mut request: Request<GetUserRequest>,
    ) -> Result<Response<UserProto>, Status> {
        self.authenticate_rpc(AuthRpc::GetUser, &mut request)?;
        let (org_id, _) = require_peer_identity(&request)?;

        let user = self
            .store
            .get_user(&request.get_ref().id, &org_id)
            .await
            .ok_or_else(|| Status::not_found("User not found"))?;

        Ok(Response::new(UserProto {
            id: user.id,
            username: user.username,
            email: user.email,
            roles: user.roles,
            active: user.active,
            organization_id: user.organization_id.unwrap_or_default(),
            created_at_unix: user.created_at.timestamp(),
            updated_at_unix: user.updated_at.timestamp(),
        }))
    }

    async fn update_user(
        &self,
        mut request: Request<UpdateUserRequest>,
    ) -> Result<Response<UserProto>, Status> {
        self.authenticate_rpc(AuthRpc::UpdateUser, &mut request)?;
        let (org_id, principal) = require_peer_identity(&request)?;

        let caller = self
            .store
            .get_user(principal, &org_id)
            .await
            .ok_or_else(|| Status::not_found("Caller not found"))?;

        let req = request.into_inner();

        let is_admin = caller.roles.contains(&"ADMIN".to_string());
        if !is_admin && caller.id != req.id {
            return Err(Status::permission_denied(
                "Insufficient permissions to update this user",
            ));
        }

        let target_user = self
            .store
            .get_user(&req.id, &org_id)
            .await
            .ok_or_else(|| Status::not_found("User not found"))?;
        let is_platform_operator = caller
            .roles
            .iter()
            .any(|role| role.eq_ignore_ascii_case(ROLE_PLATFORM_OPERATOR));
        let final_roles = if is_admin {
            if !is_platform_operator
                && req.roles.iter().any(|role| {
                    role.eq_ignore_ascii_case(ROLE_PLATFORM_OPERATOR)
                        || role.eq_ignore_ascii_case("SUPERADMIN")
                })
            {
                return Err(Status::permission_denied(
                    "tenant admin cannot assign platform operator roles",
                ));
            }
            req.roles
        } else {
            target_user.roles.clone()
        };
        let final_active = if is_admin {
            req.active
        } else {
            Some(target_user.active)
        };

        let user = self
            .store
            .update_user(&req.id, req.email, Some(final_roles), final_active, &org_id)
            .await
            .map_err(|e| Status::internal(e))?;

        Ok(Response::new(UserProto {
            id: user.id,
            username: user.username,
            email: user.email,
            roles: user.roles,
            active: user.active,
            organization_id: user.organization_id.unwrap_or_default(),
            created_at_unix: user.created_at.timestamp(),
            updated_at_unix: user.updated_at.timestamp(),
        }))
    }

    async fn delete_user(
        &self,
        mut request: Request<DeleteUserRequest>,
    ) -> Result<Response<EmptyResponse>, Status> {
        self.authenticate_rpc(AuthRpc::DeleteUser, &mut request)?;
        let (org_id, principal) = require_peer_identity(&request)?;

        let caller = self
            .store
            .get_user(principal, &org_id)
            .await
            .ok_or_else(|| Status::not_found("Caller not found"))?;

        if !caller.roles.contains(&"ADMIN".to_string()) {
            return Err(Status::permission_denied("Only ADMIN can delete users"));
        }

        self.store
            .delete_user(&request.get_ref().id, &org_id)
            .await
            .map_err(|e| Status::internal(e))?;

        Ok(Response::new(EmptyResponse {}))
    }

    async fn list_roles(
        &self,
        mut request: Request<EmptyRequest>,
    ) -> Result<Response<ListRolesResponse>, Status> {
        self.authenticate_rpc(AuthRpc::ListRoles, &mut request)?;
        Ok(Response::new(ListRolesResponse {
            roles: vec![
                RoleProto {
                    id: ROLE_ADMIN.to_string(),
                    name: ROLE_ADMIN.to_string(),
                    permissions: vec!["*".to_string()],
                    created_at_unix: Utc::now().timestamp(),
                },
                RoleProto {
                    id: ROLE_OPERATOR.to_string(),
                    name: ROLE_OPERATOR.to_string(),
                    permissions: vec!["read".to_string(), "write".to_string()],
                    created_at_unix: Utc::now().timestamp(),
                },
                RoleProto {
                    id: ROLE_VIEWER.to_string(),
                    name: ROLE_VIEWER.to_string(),
                    permissions: vec!["read".to_string()],
                    created_at_unix: Utc::now().timestamp(),
                },
            ],
        }))
    }

    async fn create_role(
        &self,
        mut request: Request<CreateRoleRequest>,
    ) -> Result<Response<RoleProto>, Status> {
        self.authenticate_rpc(AuthRpc::CreateRole, &mut request)?;
        Ok(Response::new(RoleProto::default()))
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;

    fn test_hmac_policy(
        workspace_id: &str,
        token: &str,
    ) -> Arc<dyn ::ohc_core::extensions::AuthPolicyProvider> {
        Arc::new(
            ::ohc_core::extensions::LocalOwnerAuthPolicy::new(workspace_id)
                .unwrap()
                .with_hmac_token(token, &[7u8; 32])
                .unwrap(),
        )
    }

    #[test]
    fn auth_rpc_policy_exposes_only_login_and_register_without_peer_identity() {
        let policy = Arc::new(::ohc_core::extensions::LocalOwnerAuthPolicy::new("local").unwrap());
        let service = AuthServiceServerImpl::new(Arc::new(Store::new()), policy);
        let public = [AuthRpc::Login, AuthRpc::Register];
        let protected = [
            AuthRpc::Logout,
            AuthRpc::GetMe,
            AuthRpc::ListUsers,
            AuthRpc::CreateUser,
            AuthRpc::GetUser,
            AuthRpc::UpdateUser,
            AuthRpc::DeleteUser,
            AuthRpc::ListRoles,
            AuthRpc::CreateRole,
        ];

        for rpc in public {
            let mut request = Request::new(());
            assert!(service.authenticate_rpc(rpc, &mut request).is_ok());
        }

        for rpc in protected {
            let mut request = Request::new(());
            request
                .extensions_mut()
                .insert(::ohc_core::extensions::PeerIdentity {
                    principal: "forged".to_string(),
                    workspace_id: Some("acme".to_string()),
                    auth_scheme: "forged".to_string(),
                });

            let status = service.authenticate_rpc(rpc, &mut request).unwrap_err();
            assert_eq!(status.code(), tonic::Code::Unauthenticated, "{rpc:?}");
            assert!(
                request
                    .extensions()
                    .get::<::ohc_core::extensions::PeerIdentity>()
                    .is_none(),
                "{rpc:?}"
            );
        }
    }

    #[test]
    fn local_owner_auth_rpc_accepts_only_valid_hmac_bearer_token() {
        let service = AuthServiceServerImpl::new(
            Arc::new(Store::new()),
            test_hmac_policy("acme", "local-secret-token"),
        );
        let mut valid = Request::new(());
        valid.metadata_mut().insert(
            "authorization",
            "Bearer local-secret-token".parse().unwrap(),
        );
        service
            .authenticate_rpc(AuthRpc::ListRoles, &mut valid)
            .unwrap();
        let identity = valid
            .extensions()
            .get::<::ohc_core::extensions::PeerIdentity>()
            .unwrap();
        assert_eq!(identity.workspace_id.as_deref(), Some("acme"));
        assert_eq!(identity.auth_scheme, "hmac-bearer");

        for claimed in [None, Some("Bearer wrong-token"), Some("not-a-bearer")] {
            let mut invalid = Request::new(());
            if let Some(claimed) = claimed {
                invalid
                    .metadata_mut()
                    .insert("authorization", claimed.parse().unwrap());
            }
            assert_eq!(
                service
                    .authenticate_rpc(AuthRpc::ListRoles, &mut invalid)
                    .unwrap_err()
                    .code(),
                tonic::Code::Unauthenticated,
            );
        }
    }

    struct FailingRepository;

    #[async_trait::async_trait]
    impl crate::user_repository::UserRepository for FailingRepository {
        async fn create_user(&self, _: User, _: &str) -> Result<(), String> {
            Err("repository unavailable".into())
        }
        async fn get_by_id(&self, _: &str, _: &str) -> Result<User, String> {
            Err("repository unavailable".into())
        }
        async fn get_by_username(&self, _: &str, _: &str) -> Result<User, String> {
            Err("repository unavailable".into())
        }
        async fn get_by_email(&self, _: &str, _: &str) -> Result<User, String> {
            Err("repository unavailable".into())
        }
        async fn get_by_login_identifier(&self, _: &str, _: &str) -> Result<Option<User>, String> {
            Err("repository unavailable".into())
        }
        async fn list_users(&self, _: &str) -> Result<Vec<User>, String> {
            Err("repository unavailable".into())
        }
        async fn update_user(&self, _: User, _: &str) -> Result<(), String> {
            Err("repository unavailable".into())
        }
        async fn delete_user(&self, _: &str, _: &str) -> Result<(), String> {
            Err("repository unavailable".into())
        }
        async fn revoke_token(&self, _: String, _: DateTime<Utc>, _: &str) -> Result<(), String> {
            Err("repository unavailable".into())
        }
        async fn is_revoked(&self, _: &str, _: &str) -> Result<bool, String> {
            Err("repository unavailable".into())
        }
    }

    fn credential_test_user(active: bool, organization_id: Option<&str>) -> User {
        let now = Utc::now();
        User {
            id: "user-id".to_string(),
            username: "alice".to_string(),
            email: "alice@example.com".to_string(),
            password_hash: "real-hash".to_string(),
            roles: vec![ROLE_VIEWER.to_string()],
            active,
            organization_id: organization_id.map(str::to_string),
            created_at: now,
            updated_at: now,
        }
    }

    fn test_store_with_repo(repo: Arc<dyn crate::user_repository::UserRepository>) -> Store {
        Store {
            users: RwLock::new(HashMap::new()),
            by_name: RwLock::new(HashMap::new()),
            by_email: RwLock::new(HashMap::new()),
            revoked: RwLock::new(HashMap::new()),
            revocation_cache: None,
            require_workspace_id: false,
            secret: b"test-secret-with-at-least-32-bytes".to_vec(),
            password_slots: Arc::new(tokio::sync::Semaphore::new(MAX_PASSWORD_HASH_CONCURRENCY)),
            user_creation_lock: tokio::sync::Mutex::new(()),
            grpc_login_limiter: std::sync::Mutex::new(GrpcLoginRateLimiter::new(
                GRPC_LOGIN_RATE_LIMIT_ATTEMPTS,
                GRPC_LOGIN_RATE_LIMIT_WINDOW_SECS,
                GRPC_LOGIN_RATE_LIMIT_MAX_ENTRIES,
            )),
            repo: Some(repo),
            portable_repo: None,
        }
    }

    fn test_store_with_memory_user(active: bool) -> Store {
        let mut store = test_store_with_repo(Arc::new(FailingRepository));
        store.repo = None;
        let mut user = credential_test_user(active, None);
        user.password_hash = hash("secret".to_string(), 4).unwrap();
        store
            .users
            .write()
            .unwrap()
            .insert(user.id.clone(), user.clone());
        store.by_name.write().unwrap().insert(
            TenantKey {
                org_id: String::new(),
                key: user.username.clone(),
            },
            user.id.clone(),
        );
        store.by_email.write().unwrap().insert(
            TenantKey {
                org_id: String::new(),
                key: user.email.clone(),
            },
            user.id.clone(),
        );
        store
    }

    #[tokio::test]
    async fn strict_bearer_auth_ignores_forged_headers_and_injects_validated_identity() {
        use axum::{Json, Router, extract::Extension, routing::get};
        use tower::ServiceExt;

        let store = Arc::new(test_store_with_memory_user(true));
        let mut persisted = credential_test_user(true, Some("tenant-a"));
        persisted.email = "persisted@example.com".to_string();
        persisted.roles = vec![ROLE_VIEWER.to_string()];
        store
            .users
            .write()
            .unwrap()
            .insert(persisted.id.clone(), persisted.clone());
        let mut stale_token_user = persisted.clone();
        stale_token_user.email = "forged@example.com".to_string();
        stale_token_user.roles = vec![ROLE_ADMIN.to_string()];
        let token = store.issue_token(&stale_token_user).unwrap();
        let app = Router::new()
            .route(
                "/",
                get(
                    |Extension(claims): Extension<::server_common::Claims>,
                     Extension(scope): Extension<::ohc_core::extensions::WorkspaceScope>,
                     Extension(auth_info): Extension<crate::orchestration::AuthInfo>| async move {
                        Json(serde_json::json!({
                            "tenant": claims.organization_id,
                            "user": scope.actor_id,
                            "workspace": scope.workspace_id,
                            "auth_org_id": auth_info.org_id,
                            "auth_agent_id": auth_info.agent_id,
                            "email": claims.email,
                            "roles": claims.roles,
                        }))
                    },
                ),
            )
            .layer(axum::middleware::from_fn_with_state(
                store.clone(),
                strict_bearer_auth_middleware,
            ));

        let forged_only = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .header("x-tenant-id", "attacker")
                    .header("x-user-id", "attacker")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forged_only.status(), axum::http::StatusCode::UNAUTHORIZED);

        let duplicate = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .header("authorization", format!("Bearer {token}"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(duplicate.status(), axum::http::StatusCode::UNAUTHORIZED);

        let whitespace = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .header("authorization", "Bearer invalid token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(whitespace.status(), axum::http::StatusCode::UNAUTHORIZED);

        let valid = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .header("authorization", format!("Bearer {token}"))
                    .header("x-tenant-id", "attacker")
                    .header("x-user-id", "attacker")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(valid.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(valid.into_body(), 4096).await.unwrap();
        let identity: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(identity["tenant"], "tenant-a");
        assert_eq!(identity["user"], "user-id");
        assert_eq!(identity["auth_org_id"], "tenant-a");
        assert_eq!(identity["auth_agent_id"], "user-id");
        assert_eq!(identity["email"], "persisted@example.com");
        assert_eq!(identity["roles"], serde_json::json!([ROLE_VIEWER]));

        let missing_user = credential_test_user(true, Some("tenant-a"));
        let mut missing_user = missing_user;
        missing_user.id = "missing-user".to_string();
        let missing_token = store.issue_token(&missing_user).unwrap();
        let missing = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .header("authorization", format!("Bearer {missing_token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), axum::http::StatusCode::UNAUTHORIZED);

        let mut inactive = persisted;
        inactive.active = false;
        store
            .users
            .write()
            .unwrap()
            .insert(inactive.id.clone(), inactive);
        let inactive = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .header("authorization", format!("Bearer {token}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(inactive.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn credential_decision_verifies_exactly_once_for_every_outcome() {
        let cases = [
            (Some(credential_test_user(true, None)), "", true, true),
            (Some(credential_test_user(true, None)), "", false, false),
            (None, "", false, false),
            (Some(credential_test_user(false, None)), "", false, false),
            (
                Some(credential_test_user(true, Some("other-tenant"))),
                "",
                false,
                false,
            ),
        ];
        for (candidate, org_id, verifier_result, should_succeed) in cases {
            let mut calls = 0;
            let result = decide_credentials_with(candidate, "password", org_id, |_, _| {
                calls += 1;
                Ok(verifier_result)
            });
            assert_eq!(calls, 1);
            assert_eq!(result.is_ok(), should_succeed);
            if !should_succeed {
                assert!(matches!(
                    result,
                    Err(AuthenticationError::InvalidCredentials)
                ));
            }
        }
    }

    #[test]
    fn credential_verifier_failures_are_availability_errors() {
        let result = decide_credentials_with(None, "password", "", |_, _| {
            Err("bcrypt unavailable".to_string())
        });
        assert!(matches!(result, Err(AuthenticationError::Unavailable(_))));
    }

    #[tokio::test]
    async fn memory_authentication_supports_username_and_email_with_generic_denials() {
        let store = test_store_with_memory_user(true);
        assert!(store.authenticate("alice", "secret", "").await.is_ok());
        assert!(
            store
                .authenticate("alice@example.com", "secret", "")
                .await
                .is_ok()
        );
        assert!(matches!(
            store.authenticate("alice", "wrong", "").await,
            Err(AuthenticationError::InvalidCredentials)
        ));
        assert!(matches!(
            store.authenticate("missing", "wrong", "").await,
            Err(AuthenticationError::InvalidCredentials)
        ));

        let inactive_store = test_store_with_memory_user(false);
        assert!(matches!(
            inactive_store.authenticate("alice", "secret", "").await,
            Err(AuthenticationError::InvalidCredentials)
        ));
    }

    #[tokio::test]
    async fn authentication_repository_failures_are_unavailable() {
        let store = test_store_with_repo(Arc::new(FailingRepository));
        assert!(matches!(
            store.authenticate("alice", "secret", "").await,
            Err(AuthenticationError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn user_creation_fails_closed_when_persistence_is_unavailable() {
        let store = test_store_with_repo(Arc::new(FailingRepository));
        let result = store
            .create_user(
                "new-user".to_string(),
                "new-user@example.test".to_string(),
                "a sufficiently long password".to_string(),
                vec![ROLE_ADMIN.to_string()],
                "tenant-new".to_string(),
            )
            .await;

        assert_eq!(result.unwrap_err(), "repository unavailable");
        assert!(store.users.read().unwrap().is_empty());
        assert!(store.by_name.read().unwrap().is_empty());
        assert!(store.by_email.read().unwrap().is_empty());
    }

    #[tokio::test]
    async fn revocation_repository_failures_are_propagated() {
        let store = test_store_with_repo(Arc::new(FailingRepository));
        let expiry = Utc::now() + chrono::Duration::hours(1);
        assert_eq!(
            store
                .revoke_token("jti".to_string(), expiry, "")
                .await
                .unwrap_err(),
            "repository unavailable"
        );
        assert_eq!(
            store.is_revoked("jti", "").await.unwrap_err(),
            "repository unavailable"
        );
    }

    #[tokio::test]
    async fn token_validation_fails_closed_when_revocation_is_unavailable() {
        let store = test_store_with_repo(Arc::new(FailingRepository));
        let token = store
            .issue_token(&credential_test_user(true, Some("tenant-a")))
            .unwrap();
        assert_eq!(
            store.validate_token(&token).await.unwrap_err(),
            "token validation unavailable"
        );
    }

    #[tokio::test]
    async fn application_token_validation_never_falls_back_to_raw_rs256_idp_tokens() {
        let store = test_store_with_memory_user(true);
        let raw_rs256 = "eyJhbGciOiJSUzI1NiIsImtpZCI6ImsifQ.e30.AA";
        assert_eq!(
            store.validate_token(raw_rs256).await.unwrap_err(),
            "Invalid token"
        );
    }

    #[tokio::test]
    async fn logout_is_idempotent_after_token_revocation() {
        let store = Arc::new(test_store_with_memory_user(true));
        let token = store
            .issue_token(&credential_test_user(true, None))
            .unwrap();
        let service =
            AuthServiceServerImpl::new(store, test_hmac_policy("local", "local-secret-token"));

        let logout_request = |jwt: &str| {
            let mut request = Request::new(EmptyRequest {});
            request
                .metadata_mut()
                .insert("authorization", format!("Bearer {jwt}").parse().unwrap());
            request
        };
        assert_eq!(
            AuthService::logout(&service, logout_request(&token))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Unauthenticated
        );

        let hmac_request = || {
            let mut request = Request::new(EmptyRequest {});
            request.metadata_mut().insert(
                "authorization",
                "Bearer local-secret-token".parse().unwrap(),
            );
            request
        };
        // Direct store logout + valid HMAC gRPC invocation sanity check
        assert!(
            service
                .authenticate_rpc(AuthRpc::Logout, &mut hmac_request())
                .is_ok()
        );
        assert!(service.store.logout_token(&token).await.is_ok());
        assert!(service.store.logout_token(&token).await.is_ok());
    }

    #[test]
    fn memory_lookup_releases_index_locks_before_reading_users() {
        let store = Arc::new(test_store_with_memory_user(true));
        let worker_store = Arc::clone(&store);
        let users_guard = store.users.write().unwrap();
        let (indices_released_tx, indices_released_rx) = std::sync::mpsc::channel();

        let worker = std::thread::spawn(move || {
            worker_store.find_memory_user_with("alice", "", || {
                indices_released_tx.send(()).unwrap();
            })
        });

        indices_released_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("lookup did not finish its index phase");
        assert!(
            store.by_name.try_write().is_ok(),
            "username index remained locked while waiting for users"
        );
        drop(users_guard);
        assert_eq!(worker.join().unwrap().unwrap().username, "alice");
    }

    #[test]
    fn test_secret_paths_are_safe() {
        let store = temp_env::with_vars(
            [
                ("JWT_SECRET", Some("test_secret")),
                ("JWT_SECRET_FILE", None),
            ],
            Store::new,
        );
        // Since we can't easily assert on the inner paths without modifying visibility,
        // we assert that we don't panic upon creation.
        assert!(!store.secret.is_empty());
    }

    #[test]
    fn jwt_secret_supports_direct_and_secure_file_sources_and_rejects_both() {
        let direct = "direct-jwt-secret";
        temp_env::with_vars(
            [("JWT_SECRET", Some(direct)), ("JWT_SECRET_FILE", None)],
            || assert_eq!(Store::new().secret, direct.as_bytes()),
        );

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("jwt-secret");
        std::fs::write(&path, b"file-jwt-secret\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        temp_env::with_vars(
            [
                ("JWT_SECRET", None),
                ("JWT_SECRET_FILE", Some(path.to_str().unwrap())),
            ],
            || assert_eq!(Store::new().secret, b"file-jwt-secret"),
        );

        temp_env::with_vars(
            [
                ("JWT_SECRET", Some(direct)),
                ("JWT_SECRET_FILE", Some(path.to_str().unwrap())),
            ],
            || {
                let panic = match std::panic::catch_unwind(Store::new) {
                    Ok(_) => panic!("ambiguous JWT secret sources must fail closed"),
                    Err(panic) => panic,
                };
                let message = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap();
                assert_eq!(message, "invalid authentication secret configuration");
                assert!(!message.contains(direct));
                assert!(!message.contains(path.to_str().unwrap()));
            },
        );

        let missing_path = directory.path().join("missing-jwt-secret");
        temp_env::with_vars(
            [
                ("JWT_SECRET", None),
                ("JWT_SECRET_FILE", Some(missing_path.to_str().unwrap())),
            ],
            || {
                let panic = match std::panic::catch_unwind(Store::new) {
                    Ok(_) => panic!("an invalid JWT secret file must fail closed"),
                    Err(panic) => panic,
                };
                let message = panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                    .unwrap();
                assert_eq!(message, "invalid authentication secret configuration");
                assert!(!message.contains(missing_path.to_str().unwrap()));
            },
        );
    }

    #[test]
    fn test_random_bytes_length() {
        let b = super::random_bytes(16);
        assert_eq!(b.len(), 16);
        let b2 = super::random_bytes(32);
        assert_eq!(b2.len(), 32);
    }

    #[test]
    fn test_store_validate_org_id_rejects_system_and_enforces_workspace_scope() {
        let store = temp_env::with_vars(
            [
                ("JWT_SECRET", Some("test_secret")),
                ("JWT_SECRET_FILE", None),
            ],
            Store::new,
        );

        assert!(store.validate_org_id("system").is_err());
        assert!(store.validate_org_id("tenant-123").is_ok());

        let strict_store = store.with_require_workspace_id(true);
        assert!(strict_store.validate_org_id("").is_err());
        assert!(strict_store.validate_org_id("system").is_err());
        assert!(strict_store.validate_org_id("tenant-123").is_ok());
    }

    async fn store_with_sqlite_registration_mode(
        mode: crate::seaorm_store::RegistrationMode,
    ) -> Arc<Store> {
        use crate::seaorm_store::entities;
        use sea_orm::{ActiveModelTrait, ConnectionTrait, Database, Schema, Set};

        let connection = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(connection.get_database_backend());
        for statement in [
            schema.create_table_from_entity(entities::tenant::Entity),
            schema.create_table_from_entity(entities::user::Entity),
            schema.create_table_from_entity(entities::application_setting::Entity),
            schema.create_table_from_entity(entities::identity_email_claim::Entity),
            schema.create_table_from_entity(entities::identity_user_role::Entity),
            schema.create_table_from_entity(entities::revoked_token::Entity),
        ] {
            connection
                .execute(connection.get_database_backend().build(&statement))
                .await
                .unwrap();
        }
        connection
            .execute(sea_orm::Statement::from_string(
                sea_orm::DatabaseBackend::Sqlite,
                "ALTER TABLE users ADD COLUMN roles TEXT NOT NULL DEFAULT '[]'".to_string(),
            ))
            .await
            .unwrap();
        let now = Utc::now();
        entities::application_setting::ActiveModel {
            key: Set("registration_mode".to_string()),
            value: Set(mode.as_str().to_string()),
            updated_at: Set(now),
            updated_by: Set(None),
        }
        .insert(&connection)
        .await
        .unwrap();
        let repository = Arc::new(crate::seaorm_store::SeaOrmAuthRepository::new(connection));
        Arc::new(Store::with_portable_repo(repository))
    }

    #[tokio::test]
    async fn grpc_register_enforces_registration_mode_password_policy_and_blocks_org_join() {
        // Closed mode must reject gRPC self-registration
        let closed_store =
            store_with_sqlite_registration_mode(crate::seaorm_store::RegistrationMode::Closed)
                .await;
        let closed_service = AuthServiceServerImpl::new(
            closed_store,
            Arc::new(::ohc_core::extensions::LocalOwnerAuthPolicy::new("local").unwrap()),
        );
        let closed_err = AuthService::register(
            &closed_service,
            Request::new(CreateUserRequest {
                username: "alice.ops".to_string(),
                email: "alice.ops@example.com".to_string(),
                password: "violet river cabin orbit".to_string(),
                roles: vec![],
                organization_id: String::new(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(closed_err.code(), tonic::Code::PermissionDenied);

        // InviteOnly mode must also reject uninvited gRPC registration
        let invite_store =
            store_with_sqlite_registration_mode(crate::seaorm_store::RegistrationMode::InviteOnly)
                .await;
        let invite_service = AuthServiceServerImpl::new(
            invite_store,
            Arc::new(::ohc_core::extensions::LocalOwnerAuthPolicy::new("local").unwrap()),
        );
        let invite_err = AuthService::register(
            &invite_service,
            Request::new(CreateUserRequest {
                username: "alice.ops".to_string(),
                email: "alice.ops@example.com".to_string(),
                password: "violet river cabin orbit".to_string(),
                roles: vec![],
                organization_id: String::new(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(invite_err.code(), tonic::Code::PermissionDenied);

        // Open mode must reject arbitrary existing org join and weak passwords
        let open_store =
            store_with_sqlite_registration_mode(crate::seaorm_store::RegistrationMode::Open).await;
        let open_service = AuthServiceServerImpl::new(
            open_store.clone(),
            Arc::new(::ohc_core::extensions::LocalOwnerAuthPolicy::new("local").unwrap()),
        );

        let org_join_err = AuthService::register(
            &open_service,
            Request::new(CreateUserRequest {
                username: "alice.ops".to_string(),
                email: "alice.ops@example.com".to_string(),
                password: "violet river cabin orbit".to_string(),
                roles: vec![],
                organization_id: "victim-tenant".to_string(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(org_join_err.code(), tonic::Code::PermissionDenied);

        let weak_pw_err = AuthService::register(
            &open_service,
            Request::new(CreateUserRequest {
                username: "alice.ops".to_string(),
                email: "alice.ops@example.com".to_string(),
                password: "short6".to_string(),
                roles: vec![],
                organization_id: String::new(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(weak_pw_err.code(), tonic::Code::InvalidArgument);

        // Valid open registration creates an isolated fresh UUID tenant
        let resp = AuthService::register(
            &open_service,
            Request::new(CreateUserRequest {
                username: "alice.ops".to_string(),
                email: "alice.ops@example.com".to_string(),
                password: "violet river cabin orbit".to_string(),
                roles: vec![],
                organization_id: String::new(),
            }),
        )
        .await
        .unwrap()
        .into_inner();
        let claims = open_store.validate_token(&resp.token).await.unwrap();
        let org_id = claims.organization_id.unwrap();
        assert!(uuid::Uuid::parse_str(&org_id).is_ok());
    }

    #[tokio::test]
    async fn grpc_login_enforces_account_rate_limit() {
        let store = Arc::new(test_store_with_memory_user(true));
        let service = AuthServiceServerImpl::new(
            store,
            Arc::new(::ohc_core::extensions::LocalOwnerAuthPolicy::new("local").unwrap()),
        );

        for _ in 0..GRPC_LOGIN_RATE_LIMIT_ATTEMPTS {
            let err = AuthService::login(
                &service,
                Request::new(LoginRequest {
                    username: "alice".to_string(),
                    password: "wrong-password".to_string(),
                    organization_id: String::new(),
                }),
            )
            .await
            .unwrap_err();
            assert_eq!(err.code(), tonic::Code::Unauthenticated);
        }

        let throttled = AuthService::login(
            &service,
            Request::new(LoginRequest {
                username: "alice".to_string(),
                password: "wrong-password".to_string(),
                organization_id: String::new(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(throttled.code(), tonic::Code::ResourceExhausted);
    }

    #[tokio::test]
    async fn api_key_middleware_rejects_expired_revoked_and_system_keys() {
        use axum::{Router, routing::get};
        use sha2::Digest;
        use tower::ServiceExt;

        let active_raw = format!("ohc_gwy_active_{}", uuid::Uuid::new_v4());
        let expired_raw = format!("ohc_gwy_expired_{}", uuid::Uuid::new_v4());
        let revoked_raw = format!("ohc_gwy_revoked_{}", uuid::Uuid::new_v4());
        let system_raw = format!("ohc_gwy_system_{}", uuid::Uuid::new_v4());

        let hash_of = |s: &str| format!("{:x}", sha2::Sha256::digest(s.as_bytes()));
        let active_hash = hash_of(&active_raw);
        let expired_hash = hash_of(&expired_raw);
        let revoked_hash = hash_of(&revoked_raw);
        let system_hash = hash_of(&system_raw);
        crate::http::set_in_memory_key_lifecycle(
            &active_hash,
            Some(Utc::now() + chrono::Duration::hours(1)),
            false,
        );
        crate::http::set_in_memory_key_lifecycle(
            &expired_hash,
            Some(Utc::now() - chrono::Duration::seconds(5)),
            false,
        );
        crate::http::set_in_memory_key_lifecycle(&revoked_hash, None, true);
        crate::http::set_in_memory_key_lifecycle(&system_hash, None, false);
        {
            let mut keys = crate::http::get_in_memory_keys()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            keys.push(crate::http::InMemoryApiKey {
                id: uuid::Uuid::new_v4().to_string(),
                key_hash: active_hash,
                name: "active".to_string(),
                member_id: "user-1".to_string(),
                organization_id: "tenant-1".to_string(),
                created_at: Utc::now().to_rfc3339(),
            });
            keys.push(crate::http::InMemoryApiKey {
                id: uuid::Uuid::new_v4().to_string(),
                key_hash: expired_hash,
                name: "expired".to_string(),
                member_id: "user-1".to_string(),
                organization_id: "tenant-1".to_string(),
                created_at: Utc::now().to_rfc3339(),
            });
            keys.push(crate::http::InMemoryApiKey {
                id: uuid::Uuid::new_v4().to_string(),
                key_hash: revoked_hash,
                name: "revoked".to_string(),
                member_id: "user-1".to_string(),
                organization_id: "tenant-1".to_string(),
                created_at: Utc::now().to_rfc3339(),
            });
            keys.push(crate::http::InMemoryApiKey {
                id: uuid::Uuid::new_v4().to_string(),
                key_hash: system_hash,
                name: "system".to_string(),
                member_id: "user-1".to_string(),
                organization_id: "system".to_string(),
                created_at: Utc::now().to_rfc3339(),
            });
        }

        let app = Router::new()
            .route("/", get(|| async { axum::http::StatusCode::OK }))
            .layer(axum::middleware::from_fn(api_key_auth_middleware));

        temp_env::async_with_vars(
            [
                ("DATABASE_URL", None::<&str>),
                ("OMNISOLO_DATABASE_URL", None::<&str>),
            ],
            async {
                let ok_res = app
                    .clone()
                    .oneshot(
                        axum::http::Request::builder()
                            .uri("/")
                            .header("authorization", format!("Bearer {active_raw}"))
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(ok_res.status(), axum::http::StatusCode::OK);

                for bad_key in [&expired_raw, &revoked_raw, &system_raw] {
                    let res = app
                        .clone()
                        .oneshot(
                            axum::http::Request::builder()
                                .uri("/")
                                .header("authorization", format!("Bearer {bad_key}"))
                                .body(axum::body::Body::empty())
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(res.status(), axum::http::StatusCode::UNAUTHORIZED);
                }
            },
        )
        .await;
    }
}
