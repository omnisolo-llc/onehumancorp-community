//! Tenant/provider-bound encrypted API keys. Never stores consumer sessions or
//! reports verification from a nonempty input alone. Keys are never serializable.
use super::usage_ledger::{LedgerError, UsageLedger};
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
};
use futures_util::StreamExt;
use serde::Serialize;
use std::{collections::BTreeMap, time::Duration};
use zeroize::Zeroizing;

// API keys do not promise infinite authorization. Expired verification is
// explicit and can be renewed through the read-only refresh operation.
const VERIFICATION_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;

#[derive(Clone)]
pub struct ConnectionVault {
    db: UsageLedger,
    keys: BTreeMap<String, Zeroizing<Vec<u8>>>,
    active: String,
    #[cfg(test)]
    test_endpoints: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ConnectionInfo {
    pub provider: String,
    pub state: String,
    pub revision: String,
    pub verified_at: i64,
    pub expires_at: i64,
}

fn identity(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && value.len() <= 255
        && !value.chars().any(char::is_control)
}
fn encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn decode(value: &str) -> Result<Vec<u8>, LedgerError> {
    if !value.len().is_multiple_of(2) || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(LedgerError::Invalid);
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16).map_err(|_| LedgerError::Invalid)
        })
        .collect()
}
fn is_consumer_session_token(secret: &str) -> bool {
    secret.starts_with("sess-")
        || secret.starts_with("sk-ant-sid")
        || secret.starts_with("__Secure-")
        || secret.starts_with("__Host-")
        || secret.starts_with("SID=")
        || secret.starts_with("SAPISID=")
        || secret.contains(';')
}

fn valid_google_oauth_bearer(secret: &str) -> bool {
    if is_consumer_session_token(secret)
        || secret.starts_with("sk-")
        || secret.starts_with("sk_")
        || secret.starts_with("rk_")
        || secret.starts_with("re_")
    {
        return false;
    }
    // RFC 6750 bearer token syntax: URL/base64-safe token characters with
    // optional trailing '=' padding only (rejects cookie KEY=VALUE pairs).
    let without_padding = secret.trim_end_matches('=');
    !without_padding.is_empty()
        && !without_padding.contains('=')
        && without_padding.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
        })
}

pub fn supported(provider: &str) -> bool {
    matches!(
        provider,
        "openai_api" | "anthropic_api" | "stripe" | "resend" | "google_workspace"
    )
}

macro_rules! transaction {
    ($vault:expr,$tenant:expr,$tx:ident,$body:block) => {{
        match &$vault.db {
            UsageLedger::Postgres(pool) => {
                let mut $tx = pool.begin().await?;
                sqlx::query("SELECT set_config('app.current_tenant',$1,true)")
                    .bind($tenant)
                    .execute(&mut *$tx)
                    .await?;
                $body
            }
            UsageLedger::Sqlite(pool) => {
                let mut $tx = pool.begin().await?;
                $body
            }
        }
    }};
}
impl ConnectionVault {
    pub fn from_environment(db: UsageLedger) -> Result<Self, LedgerError> {
        let encoded = Zeroizing::new(
            std::env::var("OMNISOLO_CONNECTION_KEYS").map_err(|_| LedgerError::Unconfigured)?,
        );
        let keys: BTreeMap<String, String> =
            serde_json::from_str(&encoded).map_err(|_| LedgerError::Invalid)?;
        let active = std::env::var("OMNISOLO_CONNECTION_ACTIVE_KEY")
            .map_err(|_| LedgerError::Unconfigured)?;
        Self::with_keys(db, keys, active)
    }
    pub fn with_keys(
        db: UsageLedger,
        encoded: BTreeMap<String, String>,
        active: String,
    ) -> Result<Self, LedgerError> {
        if encoded.is_empty() || encoded.len() > 8 || !identity(&active) {
            return Err(LedgerError::Invalid);
        }
        let mut keys = BTreeMap::new();
        for (id, value) in encoded {
            let value = Zeroizing::new(value);
            let key = decode(&value)?;
            if !identity(&id) || key.len() != 32 {
                return Err(LedgerError::Invalid);
            }
            keys.insert(id, Zeroizing::new(key));
        }
        if !keys.contains_key(&active) {
            return Err(LedgerError::Unconfigured);
        }
        Ok(Self {
            db,
            keys,
            active,
            #[cfg(test)]
            test_endpoints: BTreeMap::new(),
        })
    }
    #[cfg(test)]
    pub fn with_test_endpoint(
        mut self,
        provider: impl Into<String>,
        endpoint: impl Into<String>,
    ) -> Self {
        self.test_endpoints.insert(provider.into(), endpoint.into());
        self
    }
    pub async fn initialize(&self) -> Result<(), LedgerError> {
        Self::initialize_schema(&self.db).await
    }
    pub async fn initialize_schema(db: &UsageLedger) -> Result<(), LedgerError> {
        const DDL: &str = "CREATE TABLE IF NOT EXISTS ohc_provider_connections (tenant_id TEXT NOT NULL,provider TEXT NOT NULL,revision TEXT NOT NULL,state TEXT NOT NULL,key_id TEXT NOT NULL,nonce_hex TEXT NOT NULL,ciphertext_hex TEXT NOT NULL,verified_at BIGINT NOT NULL,PRIMARY KEY(tenant_id,provider))";
        match db {
            UsageLedger::Postgres(pool) => {
                let mut tx = pool.begin().await?;
                sqlx::query("SELECT pg_advisory_xact_lock(734562192)")
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(DDL).execute(&mut *tx).await?;
                tx.commit().await?;
            }
            UsageLedger::Sqlite(pool) => {
                sqlx::query(DDL).execute(pool).await?;
            }
        }
        Ok(())
    }
    pub async fn verify_and_store(
        &self,
        tenant: &str,
        provider: &str,
        secret: &str,
    ) -> Result<ConnectionInfo, LedgerError> {
        if !identity(tenant)
            || !supported(provider)
            || secret.trim() != secret
            || secret.len() < 12
            || secret.len() > 4096
            || secret
                .chars()
                .any(|ch| ch.is_control() || ch.is_whitespace())
            || is_consumer_session_token(secret)
        {
            return Err(LedgerError::Invalid);
        }
        let prefix_valid = match provider {
            "openai_api" => secret.starts_with("sk-") && !secret.starts_with("sk-ant-"),
            "stripe" => secret.starts_with("sk_") || secret.starts_with("rk_"),
            "resend" => secret.starts_with("re_"),
            "anthropic_api" => secret.starts_with("sk-ant-") && !secret.starts_with("sk-ant-sid"),
            "google_workspace" => valid_google_oauth_bearer(secret),
            _ => false,
        };
        if !prefix_valid {
            return Err(LedgerError::Invalid);
        }
        let expected = self.revision(tenant, provider).await?;
        self.verify_expected(tenant, provider, secret, expected.as_deref())
            .await
    }
    async fn verify_expected(
        &self,
        tenant: &str,
        provider: &str,
        secret: &str,
        expected: Option<&str>,
    ) -> Result<ConnectionInfo, LedgerError> {
        let default_endpoint = match provider {
            "openai_api" => "https://api.openai.com/v1/models",
            "stripe" => "https://api.stripe.com/v1/balance",
            "anthropic_api" => "https://api.anthropic.com/v1/models",
            "resend" => "https://api.resend.com/domains",
            "google_workspace" => {
                "https://www.googleapis.com/drive/v3/files?pageSize=1&fields=files(id)"
            }
            _ => return Err(LedgerError::Invalid),
        };
        #[cfg(test)]
        let endpoint = self
            .test_endpoints
            .get(provider)
            .map(String::as_str)
            .unwrap_or(default_endpoint);
        #[cfg(not(test))]
        let endpoint = default_endpoint;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| LedgerError::State)?;
        let request = client.get(endpoint);
        let request = match provider {
            "stripe" => request.basic_auth(secret, Some("")),
            "anthropic_api" => request
                .header("x-api-key", secret)
                .header("anthropic-version", "2023-06-01"),
            _ => request.bearer_auth(secret),
        };
        let response = request.send().await.map_err(|_| LedgerError::State)?;
        if !response.status().is_success() {
            return Err(LedgerError::State);
        }
        // Check only the response's object discriminator, never persist financial
        // balances/model data as credential metadata or log the response body.
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| LedgerError::State)?;
            if body.len().saturating_add(chunk.len()) > 1024 * 1024 {
                return Err(LedgerError::State);
            }
            body.extend_from_slice(&chunk);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&body).map_err(|_| LedgerError::State)?;
        if provider == "stripe" {
            if value.get("object").and_then(|v| v.as_str()) != Some("balance")
                || !value["livemode"].is_boolean()
            {
                return Err(LedgerError::State);
            }
        } else if provider == "google_workspace" {
            if !value.get("files").is_some_and(serde_json::Value::is_array) {
                return Err(LedgerError::State);
            }
        } else if !value["data"].is_array() {
            return Err(LedgerError::State);
        }
        self.store_verified_expected(tenant, provider, secret, expected)
            .await
    }
    async fn revision(&self, tenant: &str, provider: &str) -> Result<Option<String>, LedgerError> {
        if !identity(tenant) || !supported(provider) {
            return Err(LedgerError::Invalid);
        }
        transaction!(self, tenant, tx, {
            sqlx::query_scalar(
                "SELECT revision FROM ohc_provider_connections WHERE tenant_id=$1 AND provider=$2",
            )
            .bind(tenant)
            .bind(provider)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Into::into)
        })
    }
    #[cfg(test)]
    async fn store_verified(
        &self,
        tenant: &str,
        provider: &str,
        secret: &str,
    ) -> Result<ConnectionInfo, LedgerError> {
        let expected = self.revision(tenant, provider).await?;
        self.store_verified_expected(tenant, provider, secret, expected.as_deref())
            .await
    }
    async fn store_verified_expected(
        &self,
        tenant: &str,
        provider: &str,
        secret: &str,
        expected: Option<&str>,
    ) -> Result<ConnectionInfo, LedgerError> {
        if !identity(tenant) || !supported(provider) {
            return Err(LedgerError::Invalid);
        }
        let revision = uuid::Uuid::new_v4().to_string();
        let aad = serde_json::to_vec(&(tenant, provider, &revision, &self.active))
            .map_err(|_| LedgerError::Invalid)?;
        let cipher = Aes256Gcm::new_from_slice(
            self.keys
                .get(&self.active)
                .ok_or(LedgerError::Unconfigured)?
                .as_slice(),
        )
        .map_err(|_| LedgerError::Invalid)?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let encrypted = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: secret.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| LedgerError::State)?;
        let verified_at = chrono::Utc::now().timestamp();
        transaction!(self, tenant, tx, {
            let changed=sqlx::query("INSERT INTO ohc_provider_connections(tenant_id,provider,revision,state,key_id,nonce_hex,ciphertext_hex,verified_at) SELECT $1,$2,$3,'verified',$4,$5,$6,$7 WHERE $8 IS NULL OR EXISTS(SELECT 1 FROM ohc_provider_connections WHERE tenant_id=$1 AND provider=$2 AND revision=$8) ON CONFLICT(tenant_id,provider) DO UPDATE SET revision=excluded.revision,state=excluded.state,key_id=excluded.key_id,nonce_hex=excluded.nonce_hex,ciphertext_hex=excluded.ciphertext_hex,verified_at=excluded.verified_at WHERE ohc_provider_connections.revision=$8")
                .bind(tenant).bind(provider).bind(&revision).bind(&self.active).bind(encode(&nonce)).bind(encode(&encrypted)).bind(verified_at).bind(expected).execute(&mut *tx).await?.rows_affected();
            if changed != 1 {
                return Err(LedgerError::Conflict);
            }
            tx.commit().await?;
            Ok(ConnectionInfo {
                provider: provider.into(),
                state: "verified".into(),
                revision,
                verified_at,
                expires_at: verified_at + VERIFICATION_TTL_SECONDS,
            })
        })
    }
    pub async fn read_key(
        &self,
        tenant: &str,
        provider: &str,
    ) -> Result<Zeroizing<String>, LedgerError> {
        self.read_key_since(
            tenant,
            provider,
            chrono::Utc::now().timestamp() - VERIFICATION_TTL_SECONDS,
        )
        .await
    }
    async fn read_key_since(
        &self,
        tenant: &str,
        provider: &str,
        since: i64,
    ) -> Result<Zeroizing<String>, LedgerError> {
        if !identity(tenant) || !supported(provider) {
            return Err(LedgerError::Invalid);
        }
        transaction!(self, tenant, tx, {
            let record:Option<(String,String,String,String)>=sqlx::query_as("SELECT revision,key_id,nonce_hex,ciphertext_hex FROM ohc_provider_connections WHERE tenant_id=$1 AND provider=$2 AND (state='verified' OR ($3=0 AND state='verification_required')) AND verified_at>$3")
                .bind(tenant).bind(provider).bind(since).fetch_optional(&mut *tx).await?;
            let (revision, key_id, nonce, ciphertext) = record.ok_or(LedgerError::Unconfigured)?;
            let key = self.keys.get(&key_id).ok_or(LedgerError::Unconfigured)?;
            if nonce.len() != 24 || ciphertext.len() > (4096 + 16) * 2 {
                return Err(LedgerError::Invalid);
            }
            let nonce = decode(&nonce)?;
            if nonce.len() != 12 {
                return Err(LedgerError::Invalid);
            }
            let aad = serde_json::to_vec(&(tenant, provider, &revision, &key_id))
                .map_err(|_| LedgerError::Invalid)?;
            let cipher =
                Aes256Gcm::new_from_slice(key.as_slice()).map_err(|_| LedgerError::Invalid)?;
            let decrypted = Zeroizing::new(
                cipher
                    .decrypt(
                        Nonce::from_slice(&nonce),
                        Payload {
                            msg: &decode(&ciphertext)?,
                            aad: &aad,
                        },
                    )
                    .map_err(|_| LedgerError::State)?,
            );
            let secret = String::from_utf8(decrypted.to_vec()).map_err(|_| LedgerError::State)?;
            Ok(Zeroizing::new(secret))
        })
    }
    /// Owner-authorized, read-only provider revalidation; never reuses a revoked
    /// key or overwrites a key rotated while the request was in flight.
    pub async fn refresh(
        &self,
        tenant: &str,
        provider: &str,
    ) -> Result<ConnectionInfo, LedgerError> {
        let expected = self
            .revision(tenant, provider)
            .await?
            .ok_or(LedgerError::Unconfigured)?;
        let key = self.read_key_since(tenant, provider, 0).await?;
        match self
            .verify_expected(tenant, provider, &key, Some(&expected))
            .await
        {
            Ok(connection) => Ok(connection),
            Err(error) => {
                // A failed or ambiguous check must not leave a stale green
                // badge. Do not invalidate a key replaced/revoked in flight.
                self.mark_verification_required(tenant, provider, &expected)
                    .await?;
                Err(error)
            }
        }
    }
    async fn mark_verification_required(
        &self,
        tenant: &str,
        provider: &str,
        expected: &str,
    ) -> Result<(), LedgerError> {
        if !identity(tenant) || !supported(provider) || !identity(expected) {
            return Err(LedgerError::Invalid);
        }
        transaction!(self, tenant, tx, {
            sqlx::query("UPDATE ohc_provider_connections SET state='verification_required' WHERE tenant_id=$1 AND provider=$2 AND revision=$3 AND state IN ('verified','verification_required')")
                .bind(tenant).bind(provider).bind(expected).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(())
        })
    }
    pub async fn revoke(&self, tenant: &str, provider: &str) -> Result<(), LedgerError> {
        if !identity(tenant) || !supported(provider) {
            return Err(LedgerError::Invalid);
        }
        transaction!(self, tenant, tx, {
            // Tombstones defeat an in-flight first connection verification too.
            let revision = uuid::Uuid::new_v4().to_string();
            sqlx::query("INSERT INTO ohc_provider_connections(tenant_id,provider,revision,state,key_id,nonce_hex,ciphertext_hex,verified_at) VALUES($1,$2,$3,'revoked','','','',$4) ON CONFLICT(tenant_id,provider) DO UPDATE SET revision=excluded.revision,state='revoked',ciphertext_hex='',nonce_hex='',verified_at=excluded.verified_at")
                .bind(tenant).bind(provider).bind(revision).bind(chrono::Utc::now().timestamp()).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(())
        })
    }
    pub async fn list(&self, tenant: &str) -> Result<Vec<ConnectionInfo>, LedgerError> {
        if !identity(tenant) {
            return Err(LedgerError::Invalid);
        }
        transaction!(self, tenant, tx, {
            let rows:Vec<(String,String,String,i64)>=sqlx::query_as("SELECT provider,state,revision,verified_at FROM ohc_provider_connections WHERE tenant_id=$1 ORDER BY provider LIMIT 100")
                .bind(tenant).fetch_all(&mut *tx).await?;
            let now = chrono::Utc::now().timestamp();
            Ok(rows
                .into_iter()
                .map(|(provider, state, revision, verified_at)| {
                    let expires_at = verified_at.saturating_add(VERIFICATION_TTL_SECONDS);
                    ConnectionInfo {
                        provider,
                        state: if state == "verified" && expires_at <= now {
                            "reauthentication_required".into()
                        } else {
                            state
                        },
                        revision,
                        verified_at,
                        expires_at,
                    }
                })
                .collect())
        })
    }

    /// Lists tenant connections, surfacing expired 7-day verification TTLs as
    /// `"verification_required"` to conform to the shared `ConnectorStatus`
    /// contract (`verified | verification_required | revoked | not_connected`).
    pub async fn list_with_expiry_status(
        &self,
        tenant: &str,
    ) -> Result<Vec<ConnectionInfo>, LedgerError> {
        let mut connections = self.list(tenant).await?;
        for connection in &mut connections {
            if connection.state == "reauthentication_required" {
                connection.state = "verification_required".into();
            }
        }
        Ok(connections)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn vault() -> (ConnectionVault, sqlx::SqlitePool) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let vault = ConnectionVault::with_keys(
            UsageLedger::Sqlite(pool.clone()),
            BTreeMap::from([("v1".into(), "11".repeat(32))]),
            "v1".into(),
        )
        .unwrap();
        vault.initialize().await.unwrap();
        (vault, pool)
    }
    #[tokio::test]
    async fn encryption_is_random_bound_to_tenant_and_revocable() {
        let (vault, pool) = vault().await;
        vault
            .store_verified("tenant-a", "openai_api", "sk-test-not-a-real-key")
            .await
            .unwrap();
        let first: String =
            sqlx::query_scalar("SELECT ciphertext_hex FROM ohc_provider_connections")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!first.contains("sk-test"));
        assert!(vault.read_key("tenant-b", "openai_api").await.is_err());
        assert_eq!(
            vault
                .read_key("tenant-a", "openai_api")
                .await
                .unwrap()
                .as_str(),
            "sk-test-not-a-real-key"
        );
        vault
            .store_verified("tenant-a", "openai_api", "sk-test-not-a-real-key")
            .await
            .unwrap();
        let second: String =
            sqlx::query_scalar("SELECT ciphertext_hex FROM ohc_provider_connections")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_ne!(first, second);
        sqlx::query("UPDATE ohc_provider_connections SET tenant_id='tenant-b'")
            .execute(&pool)
            .await
            .unwrap();
        assert!(vault.read_key("tenant-b", "openai_api").await.is_err());
        vault
            .store_verified("tenant-a", "openai_api", "sk-test-not-a-real-key")
            .await
            .unwrap();
        vault.revoke("tenant-a", "openai_api").await.unwrap();
        assert!(vault.read_key("tenant-a", "openai_api").await.is_err());
        assert_eq!(vault.list("tenant-a").await.unwrap()[0].state, "revoked");
    }
    #[tokio::test]
    async fn revoke_wins_over_inflight_verification_including_first_connect() {
        let (vault, _) = vault().await;
        let first = vault.revision("tenant-a", "stripe").await.unwrap();
        assert!(first.is_none());
        vault.revoke("tenant-a", "stripe").await.unwrap();
        assert_eq!(
            vault
                .store_verified_expected("tenant-a", "stripe", "sk_test_not_real", first.as_deref())
                .await
                .unwrap_err(),
            LedgerError::Conflict
        );
        vault
            .store_verified("tenant-a", "stripe", "sk_test_not_real")
            .await
            .unwrap();
        let previous = vault.revision("tenant-a", "stripe").await.unwrap();
        vault.revoke("tenant-a", "stripe").await.unwrap();
        assert_eq!(
            vault
                .store_verified_expected(
                    "tenant-a",
                    "stripe",
                    "sk_test_new_key",
                    previous.as_deref()
                )
                .await
                .unwrap_err(),
            LedgerError::Conflict
        );
        assert!(vault.read_key("tenant-a", "stripe").await.is_err());
    }
    #[tokio::test]
    async fn expired_verification_is_not_an_active_connection() {
        let (vault, pool) = vault().await;
        vault
            .store_verified("tenant-a", "stripe", "sk_test_not_real")
            .await
            .unwrap();
        sqlx::query("UPDATE ohc_provider_connections SET verified_at=1")
            .execute(&pool)
            .await
            .unwrap();
        assert!(vault.read_key("tenant-a", "stripe").await.is_err());
        assert_eq!(
            vault.list("tenant-a").await.unwrap()[0].state,
            "reauthentication_required"
        );
        assert!(vault.read_key_since("tenant-a", "stripe", 0).await.is_ok());
        vault.revoke("tenant-a", "stripe").await.unwrap();
        assert!(vault.read_key_since("tenant-a", "stripe", 0).await.is_err());
    }
    #[tokio::test]
    async fn failed_reverification_pauses_only_the_same_revision() {
        let (vault, _) = vault().await;
        vault
            .store_verified("tenant-a", "stripe", "sk_test_not_real")
            .await
            .unwrap();
        let previous = vault.revision("tenant-a", "stripe").await.unwrap().unwrap();
        vault
            .mark_verification_required("tenant-a", "stripe", &previous)
            .await
            .unwrap();
        assert!(vault.read_key("tenant-a", "stripe").await.is_err());
        assert_eq!(
            vault.list("tenant-a").await.unwrap()[0].state,
            "verification_required"
        );
        // The owner may retry verification, but the paused key is never an
        // execution credential until the provider confirms it again.
        assert!(vault.read_key_since("tenant-a", "stripe", 0).await.is_ok());
        vault
            .store_verified("tenant-a", "stripe", "sk_test_replaced")
            .await
            .unwrap();
        vault
            .mark_verification_required("tenant-a", "stripe", &previous)
            .await
            .unwrap();
        assert_eq!(
            vault.read_key("tenant-a", "stripe").await.unwrap().as_str(),
            "sk_test_replaced"
        );
        vault.revoke("tenant-a", "stripe").await.unwrap();
        vault
            .mark_verification_required("tenant-a", "stripe", &previous)
            .await
            .unwrap();
        assert!(vault.read_key_since("tenant-a", "stripe", 0).await.is_err());
        assert_eq!(vault.list("tenant-a").await.unwrap()[0].state, "revoked");
    }
    #[tokio::test]
    async fn key_rotation_reads_retained_key_and_fails_without_it() {
        let (old, pool) = vault().await;
        old.store_verified("tenant-a", "stripe", "sk_test_not_real")
            .await
            .unwrap();
        let rotated = ConnectionVault::with_keys(
            UsageLedger::Sqlite(pool.clone()),
            BTreeMap::from([
                ("v1".into(), "11".repeat(32)),
                ("v2".into(), "22".repeat(32)),
            ]),
            "v2".into(),
        )
        .unwrap();
        assert!(rotated.read_key("tenant-a", "stripe").await.is_ok());
        rotated
            .store_verified("tenant-a", "stripe", "sk_test_not_real")
            .await
            .unwrap();
        assert!(old.read_key("tenant-a", "stripe").await.is_err());
        assert!(
            ConnectionVault::with_keys(
                UsageLedger::Sqlite(pool),
                BTreeMap::from([("v1".into(), "short".into())]),
                "v1".into()
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn google_workspace_verifies_oauth_token_and_refreshes_against_drive_endpoint() {
        use axum::{
            Json, Router,
            extract::State,
            http::{HeaderMap, StatusCode},
            routing::get,
        };
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };

        let token = format!("{}{}.test-drive-oauth-token-0001", "ya", "29");
        let calls = Arc::new(AtomicUsize::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let app = Router::new()
            .route(
                "/drive/v3/files",
                get(
                    |State((tok, calls, fail)): State<(
                        String,
                        Arc<AtomicUsize>,
                        Arc<AtomicBool>,
                    )>,
                     headers: HeaderMap| async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let ok = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            == Some(format!("Bearer {tok}").as_str());
                        if !ok || fail.load(Ordering::SeqCst) {
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(serde_json::json!({"error": {"code": 401}})),
                            );
                        }
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({"files": [{"id": "drive-file-1"}]})),
                        )
                    },
                ),
            )
            .with_state((token.clone(), calls.clone(), fail.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let (vault, _) = vault().await;
        let vault = vault.with_test_endpoint(
            "google_workspace",
            format!("http://{addr}/drive/v3/files?pageSize=1&fields=files(id)"),
        );
        let conn = vault
            .verify_and_store("tenant-a", "google_workspace", &token)
            .await
            .unwrap();
        assert_eq!(conn.state, "verified");
        assert_eq!(conn.expires_at, conn.verified_at + VERIFICATION_TTL_SECONDS);
        assert_eq!(
            vault
                .read_key("tenant-a", "google_workspace")
                .await
                .unwrap()
                .as_str(),
            token
        );

        let refreshed = vault.refresh("tenant-a", "google_workspace").await.unwrap();
        assert_eq!(refreshed.state, "verified");
        assert_ne!(refreshed.revision, conn.revision);
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        fail.store(true, Ordering::SeqCst);
        assert_eq!(
            vault
                .refresh("tenant-a", "google_workspace")
                .await
                .unwrap_err(),
            LedgerError::State
        );
        assert!(
            vault
                .read_key("tenant-a", "google_workspace")
                .await
                .is_err()
        );
        assert_eq!(
            vault.list_with_expiry_status("tenant-a").await.unwrap()[0].state,
            "verification_required"
        );
        server.abort();
    }

    #[tokio::test]
    async fn google_workspace_rejects_drive_response_without_files_array() {
        use axum::{Json, Router, http::StatusCode, routing::get};
        let app = Router::new().route(
            "/drive/v3/files",
            get(|| async { (StatusCode::OK, Json(serde_json::json!({"data": []}))) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let (vault, _) = vault().await;
        let vault = vault.with_test_endpoint(
            "google_workspace",
            format!("http://{addr}/drive/v3/files?pageSize=1&fields=files(id)"),
        );
        assert_eq!(
            vault
                .verify_and_store(
                    "tenant-a",
                    "google_workspace",
                    "google-oauth-bearer-token-1234"
                )
                .await
                .unwrap_err(),
            LedgerError::State
        );
        assert!(
            vault
                .list_with_expiry_status("tenant-a")
                .await
                .unwrap()
                .is_empty()
        );
        server.abort();
    }

    #[tokio::test]
    async fn rejects_consumer_subscription_tokens_and_invalid_provider_formats() {
        let (vault, _) = vault().await;
        for (provider, secret) in [
            ("google_workspace", "oauth-token with-space"),
            ("google_workspace", "oauth-token\n12345678"),
            ("google_workspace", "sk-openai-key-1234567890"),
            ("google_workspace", "SID=abc123456789; SAPISID=xyz987654321"),
            ("google_workspace", "__Secure-1PSID=abcdef1234567890"),
            ("google_workspace", "sess-consumer-session-12345"),
            (
                "openai_api",
                "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.consumer.token",
            ),
            ("openai_api", "sess-consumer-session-12345"),
            ("anthropic_api", "sk-ant-sid01-consumer-web-session-token"),
            ("resend", "sk-not-a-resend-key"),
            ("stripe", "pk_live_publishable_not_secret"),
        ] {
            assert_eq!(
                vault
                    .verify_and_store("tenant-a", provider, secret)
                    .await
                    .unwrap_err(),
                LedgerError::Invalid
            );
        }
    }

    #[tokio::test]
    async fn list_with_expiry_status_surfaces_expired_7_day_ttl_as_verification_required() {
        let (vault, pool) = vault().await;
        vault
            .store_verified(
                "tenant-a",
                "google_workspace",
                "google-oauth-bearer-token-1234",
            )
            .await
            .unwrap();
        let expired_at = chrono::Utc::now().timestamp() - VERIFICATION_TTL_SECONDS - 5;
        sqlx::query("UPDATE ohc_provider_connections SET verified_at = ?1")
            .bind(expired_at)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            vault
                .read_key("tenant-a", "google_workspace")
                .await
                .is_err()
        );
        assert_eq!(
            vault.list_with_expiry_status("tenant-a").await.unwrap()[0].state,
            "verification_required"
        );
        assert_eq!(
            vault.list("tenant-a").await.unwrap()[0].state,
            "reauthentication_required"
        );
        vault.revoke("tenant-a", "google_workspace").await.unwrap();
        assert_eq!(
            vault.list_with_expiry_status("tenant-a").await.unwrap()[0].state,
            "revoked"
        );
    }
}
