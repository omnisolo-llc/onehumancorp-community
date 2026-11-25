//! Per-tenant credential ownership, payer mode, and tariff resolution. User
//! payloads never select these values or create a spending allowance. This
//! ledger uses exactly the same configured SQLx pool as the receipt store, with
//! separate, conservative transactions: an ambiguous admission is retained and
//! is never redispatched.
use super::receipts::{Error, Receipt};
use super::{AdmissionError, AdmittedAnalysis, AnalysisPolicy, Authority};
use crate::persistence::AppDatabase;
use sea_orm::ConnectionTrait;
use serde::{Deserialize, Serialize};
use server_harness::middleware::connection_vault::ConnectionVault;
use server_harness::middleware::usage_ledger::{
    Admission, LedgerError, PayerMode, RateCard, TokenCounts, UsageLedger, UsageReceipt, UsageScope,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

fn default_customer_direct_inference_is_not_rebilled() -> bool {
    true
}

pub(crate) fn valid_tenant_id(tenant_id: &str) -> bool {
    !tenant_id.trim().is_empty()
        && tenant_id.trim() == tenant_id
        && tenant_id.len() <= 255
        && !tenant_id.eq_ignore_ascii_case("system")
        && !tenant_id.chars().any(char::is_control)
}

fn vault_provider_for(analysis_provider: &str) -> Option<&'static str> {
    match analysis_provider {
        "openai" | "openai-compatible" => Some("openai_api"),
        "anthropic" => Some("anthropic_api"),
        _ => None,
    }
}

fn ledger_from_database(database: &AppDatabase) -> Result<UsageLedger, AdmissionError> {
    match database.connection().get_database_backend() {
        sea_orm::DatabaseBackend::Postgres => Ok(UsageLedger::Postgres(
            database.connection().get_postgres_connection_pool().clone(),
        )),
        sea_orm::DatabaseBackend::Sqlite => Ok(UsageLedger::Sqlite(
            database.connection().get_sqlite_connection_pool().clone(),
        )),
        _ => Err(AdmissionError::Unavailable),
    }
}

fn is_standalone_local_mode() -> bool {
    crate::is_standalone_runtime()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FundingPolicy {
    pub operator_tenant: String,
    pub payer: PayerMode,
    pub rate_card: Option<RateCard>,
    pub request_ceiling_micros: i64,
    #[serde(default = "default_customer_direct_inference_is_not_rebilled")]
    pub customer_direct_inference_is_not_rebilled: bool,
}

impl FundingPolicy {
    pub(crate) fn tenant_id(&self) -> &str {
        &self.operator_tenant
    }

    /// Explicit standalone single-tenant local Ollama fallback. Fails closed in
    /// multi-tenant / hosted runtimes so global operator env vars can never
    /// authorize cross-tenant inference or shared billing.
    pub(crate) fn from_environment(policy: &AnalysisPolicy) -> Result<Self, AdmissionError> {
        Self::from_values(policy, is_standalone_local_mode(), |key| {
            std::env::var(key).ok()
        })
    }

    #[cfg(test)]
    pub(crate) fn from_values_for_test(
        policy: &AnalysisPolicy,
        standalone_local_mode: bool,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, AdmissionError> {
        Self::from_values(policy, standalone_local_mode, lookup)
    }

    fn from_values(
        policy: &AnalysisPolicy,
        standalone_local_mode: bool,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, AdmissionError> {
        let payer = lookup("OMNISOLO_USAGE_PAYER");
        let value = match payer.as_deref() {
            Some("local") => {
                if !standalone_local_mode {
                    return Err(AdmissionError::Unavailable);
                }
                let tenant = lookup("OMNISOLO_LLM_TENANT_ID")
                    .or_else(|| lookup("OMNISOLO_BUILDER_TENANT_ID"))
                    .ok_or(AdmissionError::Unavailable)?;
                Self {
                    operator_tenant: tenant,
                    payer: PayerMode::Local,
                    rate_card: None,
                    request_ceiling_micros: 0,
                    customer_direct_inference_is_not_rebilled: true,
                }
            }
            Some("managed_api") => {
                let tenant = lookup("OMNISOLO_LLM_TENANT_ID")
                    .or_else(|| lookup("OMNISOLO_BUILDER_TENANT_ID"))
                    .ok_or(AdmissionError::Unavailable)?;
                let managed = ManagedFundingConfig::from_values(&lookup)?;
                let rate_card = managed
                    .rate_card_for(policy)
                    .ok_or(AdmissionError::Unavailable)?;
                Self {
                    operator_tenant: tenant,
                    payer: PayerMode::ManagedApi,
                    rate_card: Some(rate_card),
                    request_ceiling_micros: managed.request_ceiling_micros,
                    customer_direct_inference_is_not_rebilled: true,
                }
            }
            _ => return Err(AdmissionError::Unavailable),
        };
        value
            .validate(policy)
            .map_err(|_| AdmissionError::Unavailable)?;
        Ok(value)
    }

    pub(crate) fn validate(&self, policy: &AnalysisPolicy) -> Result<(), Error> {
        if !valid_tenant_id(&self.operator_tenant)
            || self.payer == PayerMode::NativeSubscription
            || (self.payer == PayerMode::Local) != (policy.provider == "ollama")
            || (self.payer == PayerMode::ByokApi && !self.customer_direct_inference_is_not_rebilled)
            || (self.payer == PayerMode::ManagedApi
                && (self.rate_card.is_none()
                    || !(1..=1_000_000_000_000).contains(&self.request_ceiling_micros)))
            || (self.payer != PayerMode::ManagedApi && self.request_ceiling_micros != 0)
        {
            return Err(Error::Invalid);
        }
        if let Some(rate) = &self.rate_card {
            rate.cost(&TokenCounts {
                input: 0,
                output: 0,
                cached_input: 0,
            })
            .map_err(|_| Error::Invalid)?;
        }
        Ok(())
    }
}

/// Platform tariff configuration for tenant-scoped `ManagedApi` funding.
/// Never carries a tenant identity or global API key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ManagedFundingConfig {
    pub rate_cards: BTreeMap<String, RateCard>,
    pub request_ceiling_micros: i64,
}

impl ManagedFundingConfig {
    #[allow(dead_code)]
    pub(crate) fn from_environment() -> Result<Self, AdmissionError> {
        Self::from_values(|key| std::env::var(key).ok())
    }

    pub(crate) fn from_values(
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, AdmissionError> {
        let rate_cards: BTreeMap<String, RateCard> = lookup("OMNISOLO_USAGE_RATE_CARDS")
            .map(|v| serde_json::from_str(&v))
            .transpose()
            .map_err(|_| AdmissionError::Unavailable)?
            .unwrap_or_default();
        let request_ceiling_micros = lookup("OMNISOLO_USAGE_MAX_REQUEST_MICROS")
            .map(|v| v.parse::<i64>())
            .transpose()
            .map_err(|_| AdmissionError::Unavailable)?
            .unwrap_or(0);
        Ok(Self {
            rate_cards,
            request_ceiling_micros,
        })
    }

    pub(crate) fn rate_card_for(&self, policy: &AnalysisPolicy) -> Option<RateCard> {
        self.rate_cards
            .get(&format!("{}/{}", policy.provider, policy.model))
            .cloned()
    }
}

/// Redacted, non-serializable tenant BYOK API secret.
pub(crate) struct TenantApiKey(String);

impl TenantApiKey {
    fn new(secret: String) -> Self {
        Self(secret)
    }

    #[allow(dead_code)]
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Drop for TenantApiKey {
    fn drop(&mut self) {
        self.0.clear();
    }
}

/// Per-tenant funding decision backed strictly by the authenticated tenant's
/// `ConnectionVault` credentials or `UsageLedger` micro-USD authorization.
#[allow(dead_code)]
pub(crate) struct ResolvedTenantFunding {
    pub policy: FundingPolicy,
    pub context: FundingContext,
    byok_api_key: Option<TenantApiKey>,
}

#[allow(dead_code)]
impl ResolvedTenantFunding {
    pub(crate) fn tenant_id(&self) -> &str {
        self.policy.tenant_id()
    }

    pub(crate) fn payer(&self) -> PayerMode {
        self.policy.payer
    }

    pub(crate) fn customer_direct_inference_is_not_rebilled(&self) -> bool {
        self.policy.customer_direct_inference_is_not_rebilled
    }

    pub(crate) fn byok_api_key(&self) -> Option<&str> {
        self.byok_api_key.as_ref().map(TenantApiKey::as_str)
    }
}

/// Resolve tenant-scoped funding strictly from the authenticated `tenant_id`,
/// `ConnectionVault`, and `UsageLedger`.
///
/// Security invariants:
/// - Empty, missing, untrimmed, control-character, or `"system"` tenants fail closed.
/// - Tenant-bound BYOK credentials (`PayerMode::ByokApi`) are decrypted solely for
///   `tenant_id` with `customer_direct_inference_is_not_rebilled: true` and `0`
///   micro-USD platform reservation; revoked/unverified BYOK connections fail
///   closed and never fall through to another payer or tenant.
/// - `PayerMode::ManagedApi` requires an explicit, positive-headroom `UsageLedger`
///   account for `tenant_id` plus a valid rate card and per-request ceiling.
/// - `PayerMode::Local` (`ollama`) is permitted ONLY when `standalone_local_mode`
///   is explicitly true, never silently in multi-tenant mode.
#[allow(dead_code)]
pub(crate) async fn resolve_tenant_funding(
    tenant_id: &str,
    policy: &AnalysisPolicy,
    database: &AppDatabase,
    vault: Option<&ConnectionVault>,
    managed: Option<&ManagedFundingConfig>,
    standalone_local_mode: bool,
) -> Result<ResolvedTenantFunding, AdmissionError> {
    if !valid_tenant_id(tenant_id) {
        return Err(AdmissionError::Forbidden);
    }
    // Re-validate analysis policy invariants fail-closed.
    AnalysisPolicy::new(
        policy.provider.clone(),
        policy.model.clone(),
        policy.max_output_tokens,
    )?;

    if policy.provider == "ollama" {
        if !standalone_local_mode {
            return Err(AdmissionError::Unavailable);
        }
        let funding_policy = FundingPolicy {
            operator_tenant: tenant_id.to_owned(),
            payer: PayerMode::Local,
            rate_card: None,
            request_ceiling_micros: 0,
            customer_direct_inference_is_not_rebilled: true,
        };
        funding_policy
            .validate(policy)
            .map_err(|_| AdmissionError::Unavailable)?;
        let context = FundingContext::new(funding_policy.clone(), database)?;
        return Ok(ResolvedTenantFunding {
            policy: funding_policy,
            context,
            byok_api_key: None,
        });
    }

    if let (Some(vault), Some(vault_provider)) = (vault, vault_provider_for(&policy.provider)) {
        match vault.read_key(tenant_id, vault_provider).await {
            Ok(secret) => {
                let funding_policy = FundingPolicy {
                    operator_tenant: tenant_id.to_owned(),
                    payer: PayerMode::ByokApi,
                    rate_card: managed.and_then(|cfg| cfg.rate_card_for(policy)),
                    request_ceiling_micros: 0,
                    customer_direct_inference_is_not_rebilled: true,
                };
                funding_policy
                    .validate(policy)
                    .map_err(|_| AdmissionError::Unavailable)?;
                let context = FundingContext::new(funding_policy.clone(), database)?;
                return Ok(ResolvedTenantFunding {
                    policy: funding_policy,
                    context,
                    byok_api_key: Some(TenantApiKey::new(secret.to_string())),
                });
            }
            Err(LedgerError::Unconfigured) => {
                let connections = vault
                    .list(tenant_id)
                    .await
                    .map_err(|_| AdmissionError::Unavailable)?;
                if connections
                    .iter()
                    .any(|conn| conn.provider == vault_provider)
                {
                    // A revoked, expired, or verification-required BYOK key
                    // must fail closed and never silently switch payers.
                    return Err(AdmissionError::Forbidden);
                }
            }
            Err(LedgerError::Invalid) => return Err(AdmissionError::Forbidden),
            Err(_) => return Err(AdmissionError::Unavailable),
        }
    }

    let managed_cfg = managed.ok_or(AdmissionError::Unavailable)?;
    let rate_card = managed_cfg
        .rate_card_for(policy)
        .ok_or(AdmissionError::Unavailable)?;
    let ledger = ledger_from_database(database)?;
    let summary = ledger
        .summary(tenant_id)
        .await
        .map_err(|_| AdmissionError::Unavailable)?;
    let outstanding = summary.spent_micros.saturating_add(summary.reserved_micros);
    if summary.limit_micros <= 0 || outstanding >= summary.limit_micros {
        return Err(AdmissionError::Unavailable);
    }

    let funding_policy = FundingPolicy {
        operator_tenant: tenant_id.to_owned(),
        payer: PayerMode::ManagedApi,
        rate_card: Some(rate_card),
        request_ceiling_micros: managed_cfg.request_ceiling_micros,
        customer_direct_inference_is_not_rebilled: true,
    };
    funding_policy
        .validate(policy)
        .map_err(|_| AdmissionError::Unavailable)?;
    let context = FundingContext::new(funding_policy.clone(), database)?;
    Ok(ResolvedTenantFunding {
        policy: funding_policy,
        context,
        byok_api_key: None,
    })
}

#[derive(Clone)]
pub(crate) struct UsageTicket {
    pub event_id: String,
    scope: UsageScope,
    digest: String,
    maximum: i64,
}
#[derive(Clone)]
pub(crate) struct FundingContext {
    pub policy: FundingPolicy,
    pub ledger: UsageLedger,
}
impl FundingContext {
    pub(crate) fn new(
        policy: FundingPolicy,
        database: &AppDatabase,
    ) -> Result<Self, AdmissionError> {
        let ledger = ledger_from_database(database)?;
        Ok(Self { policy, ledger })
    }
    fn ticket(
        &self,
        identity: (&str, &str),
        task: &str,
        policy: &AnalysisPolicy,
        event: String,
        request: &str,
        prepared: (i64, &str),
    ) -> Result<UsageTicket, Error> {
        let (tenant, actor) = identity;
        let (input, prepared_digest) = prepared;
        self.policy.validate(policy)?;
        if tenant != self.policy.operator_tenant {
            return Err(Error::Forbidden);
        }
        // The adapter derives this bound from its exact normalized request,
        // including all system/user text plus fixed protocol framing. Reserve
        // every allowed output token and the higher permitted input tariff.
        // Cached input is a subset, but the rate card need not discount it.
        // Hidden request sources are rejected before a receipt is admitted.
        let maximum = if self.policy.payer == PayerMode::ManagedApi {
            if !(4096..=1_000_000).contains(&input) {
                return Err(Error::Invalid);
            }
            let rate = self.policy.rate_card.as_ref().ok_or(Error::Unavailable)?;
            let cost = |cached_input| {
                rate.cost(&TokenCounts {
                    input,
                    output: i64::from(policy.max_output_tokens),
                    cached_input,
                })
                .map_err(|_| Error::Invalid)
            };
            let amount = cost(0)?.max(cost(input)?);
            if amount > self.policy.request_ceiling_micros {
                return Err(Error::Budget);
            }
            amount
        } else {
            0
        };
        let scope = UsageScope {
            tenant_id: tenant.into(),
            task_id: event.clone(),
            attempt_id: "1".into(),
            provider: policy.provider.clone(),
            model: policy.model.clone(),
            payer: self.policy.payer,
            rate_card: self.policy.rate_card.clone(),
        };
        let digest = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    "ohc-workflow-usage-v1",
                    tenant,
                    actor,
                    request,
                    task,
                    &scope,
                    &self.policy,
                    input,
                    prepared_digest
                ))
                .map_err(|_| Error::Invalid)?
            )
        );
        Ok(UsageTicket {
            event_id: event,
            scope,
            digest,
            maximum,
        })
    }
    async fn reserve_ticket(&self, ticket: UsageTicket) -> Result<UsageTicket, Error> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.ledger.reserve(
                &ticket.scope,
                &ticket.event_id,
                &ticket.digest,
                ticket.maximum,
            ),
        )
        .await
        .map_err(|_| Error::Unavailable)?
        .map_err(ledger_error)?
        {
            Admission::Admitted => Ok(ticket),
            // Only a receipt replay may reuse an identity; it never dispatches.
            Admission::Duplicate => Err(Error::Conflict),
        }
    }
    pub(crate) async fn reserve_receipt(&self, receipt: &Receipt) -> Result<UsageTicket, Error> {
        if receipt.funding.as_ref() != Some(&self.policy) {
            return Err(Error::Conflict);
        }
        let policy = AnalysisPolicy::new(
            receipt.provider.clone(),
            receipt.model.clone(),
            receipt.max_output_tokens,
        )?;
        let ticket = self.ticket(
            (&receipt.tenant_id, &receipt.actor_id),
            &receipt.task,
            &policy,
            receipt.id.clone(),
            &receipt.request_id,
            (
                receipt.input_token_bound.ok_or(Error::Unavailable)?,
                receipt
                    .prepared_request_digest
                    .as_deref()
                    .ok_or(Error::Unavailable)?,
            ),
        )?;
        self.reserve_ticket(ticket).await
    }
    pub(crate) async fn reserve_direct(
        &self,
        input: &AdmittedAnalysis,
    ) -> Result<UsageTicket, Error> {
        let Authority {
            tenant_id,
            actor_id,
            ..
        } = &input.authority;
        self.reserve_ticket(self.ticket(
            (tenant_id, actor_id),
            &input.task,
            &input.policy,
            input.execution_id.clone(),
            &input.execution_id,
            (
                input.input_token_bound.ok_or(Error::Unavailable)?,
                input.prepared.as_ref().ok_or(Error::Unavailable)?.digest(),
            ),
        )?)
        .await
    }
    pub(crate) async fn cancel_reserved(&self, ticket: &UsageTicket) {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.ledger
                .cancel_before_dispatch(&ticket.scope.tenant_id, &ticket.event_id),
        )
        .await;
        observe_write(&ticket.event_id, "cancel_before_dispatch", result);
    }
    pub(crate) async fn dispatched(&self, ticket: &UsageTicket) -> Result<(), Error> {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.ledger
                .dispatched(&ticket.scope.tenant_id, &ticket.event_id),
        )
        .await
        .map_err(|_| Error::Unavailable)?
        .map_err(ledger_error)
    }
    pub(crate) async fn settle(&self, ticket: &UsageTicket, receipt: &UsageReceipt) -> bool {
        // Unknown quantities, over-reservation responses or ambiguous commits
        // retain exposure in the ledger. No estimate is substituted as a bill.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.ledger
                .settle(&ticket.scope.tenant_id, &ticket.event_id, receipt),
        )
        .await;
        let acknowledged = matches!(&result, Ok(Ok(())));
        observe_write(&ticket.event_id, "settle", result);
        acknowledged
    }
    pub(crate) async fn unknown(&self, ticket: &UsageTicket) {
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.ledger
                .require_reconciliation(&ticket.scope.tenant_id, &ticket.event_id),
        )
        .await;
        observe_write(&ticket.event_id, "require_reconciliation", result);
    }
}
/// Log only stable internal IDs and redacted error categories. A failed or
/// ambiguous write leaves the persisted hold available for reconciliation.
pub(super) fn observe_write<T>(
    event_id: &str,
    operation: &'static str,
    result: Result<Result<T, LedgerError>, tokio::time::error::Elapsed>,
) {
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(LedgerError::State)) => {
            tracing::debug!(event_id, operation, "usage state requires reconciliation")
        }
        Ok(Err(error)) => {
            tracing::warn!(event_id, operation, error=%error, "usage write unconfirmed; retained exposure requires reconciliation")
        }
        Err(_) => tracing::warn!(
            event_id,
            operation,
            "usage write timed out; retained exposure requires reconciliation"
        ),
    }
}
fn ledger_error(error: LedgerError) -> Error {
    match error {
        LedgerError::Limit => Error::Budget,
        LedgerError::Invalid => Error::Invalid,
        LedgerError::Conflict | LedgerError::State => Error::Conflict,
        _ => Error::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::{
        Aes256Gcm, Nonce,
        aead::{Aead, KeyInit, Payload},
    };
    use rand::RngCore;
    use sea_orm::SqlxSqliteConnector;
    use std::sync::Mutex;

    static VAULT_ENV_LOCK: Mutex<()> = Mutex::new(());

    async fn sqlite_funding_harness()
    -> (sqlx::SqlitePool, AppDatabase, UsageLedger, ConnectionVault) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let ledger = UsageLedger::Sqlite(pool.clone());
        ledger.initialize().await.unwrap();
        ConnectionVault::initialize_schema(&ledger).await.unwrap();

        let key_hex = "11".repeat(32);
        let keys_json = serde_json::json!({ "v1": key_hex }).to_string();
        let vault = {
            let _guard = VAULT_ENV_LOCK.lock().unwrap();
            temp_env::with_vars(
                [
                    ("OMNISOLO_CONNECTION_KEYS", Some(keys_json.as_str())),
                    ("OMNISOLO_CONNECTION_ACTIVE_KEY", Some("v1")),
                ],
                || ConnectionVault::from_environment(ledger.clone()).unwrap(),
            )
        };
        let database =
            AppDatabase::from_connection(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool.clone()));
        (pool, database, ledger, vault)
    }

    async fn insert_verified_vault_connection(
        pool: &sqlx::SqlitePool,
        tenant_id: &str,
        provider: &str,
        secret: &str,
    ) {
        let key_id = "v1";
        let key_bytes = hex::decode("11".repeat(32)).unwrap();
        let revision = uuid::Uuid::new_v4().to_string();
        let aad = serde_json::to_vec(&(tenant_id, provider, &revision, key_id)).unwrap();
        let cipher = Aes256Gcm::new_from_slice(&key_bytes).unwrap();
        let mut nonce_bytes = [0u8; 12];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let ciphertext = cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: secret.as_bytes(),
                    aad: &aad,
                },
            )
            .unwrap();
        let verified_at = chrono::Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO ohc_provider_connections(tenant_id,provider,revision,state,key_id,nonce_hex,ciphertext_hex,verified_at) VALUES(?1,?2,?3,'verified',?4,?5,?6,?7)",
        )
        .bind(tenant_id)
        .bind(provider)
        .bind(&revision)
        .bind(key_id)
        .bind(hex::encode(nonce_bytes))
        .bind(hex::encode(ciphertext))
        .bind(verified_at)
        .execute(pool)
        .await
        .unwrap();
    }

    fn test_managed_config(policy: &AnalysisPolicy, ceiling: i64) -> ManagedFundingConfig {
        ManagedFundingConfig {
            rate_cards: BTreeMap::from([(
                format!("{}/{}", policy.provider, policy.model),
                RateCard {
                    revision: "explicit-test-tariff".into(),
                    input_micros_per_million: 1_000_000,
                    output_micros_per_million: 2_000_000,
                    cached_input_micros_per_million: 250_000,
                },
            )]),
            request_ceiling_micros: ceiling,
        }
    }

    #[tokio::test]
    async fn admission_reserves_the_worst_permitted_cached_or_uncached_input_tariff() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let database =
            AppDatabase::from_connection(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool));
        let policy =
            AnalysisPolicy::new("openai-compatible".into(), "explicit-model".into(), 256).unwrap();
        for (ordinary, cached) in [(1_000_000, 2_000_000), (2_000_000, 1_000_000)] {
            let mut funding = FundingContext::new(
                FundingPolicy {
                    operator_tenant: "operator".into(),
                    payer: PayerMode::ManagedApi,
                    rate_card: Some(RateCard {
                        revision: "explicit-test-tariff".into(),
                        input_micros_per_million: ordinary,
                        output_micros_per_million: 3_000_000,
                        cached_input_micros_per_million: cached,
                    }),
                    request_ceiling_micros: 20_000,
                    customer_direct_inference_is_not_rebilled: true,
                },
                &database,
            )
            .unwrap();
            let ticket = funding
                .ticket(
                    ("operator", "owner"),
                    "Submitted text",
                    &policy,
                    "event".into(),
                    "request",
                    (4096, &"a".repeat(64)),
                )
                .unwrap();
            assert_eq!(
                ticket.maximum,
                4096 * 2 + 256 * 3,
                "cache is a subset of input, but no discounted tariff may be assumed"
            );
            funding.policy.request_ceiling_micros = 8000;
            assert!(matches!(
                funding.ticket(
                    ("operator", "owner"),
                    "Submitted text",
                    &policy,
                    "event".into(),
                    "request",
                    (4096, &"a".repeat(64))
                ),
                Err(Error::Budget)
            ));
            funding.policy.request_ceiling_micros = 20_000;
            funding
                .policy
                .rate_card
                .as_mut()
                .unwrap()
                .cached_input_micros_per_million = i64::MAX;
            assert!(
                funding
                    .ticket(
                        ("operator", "owner"),
                        "Submitted text",
                        &policy,
                        "event".into(),
                        "request",
                        (4096, &"a".repeat(64))
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn operator_env_fallback_is_restricted_to_explicit_standalone_local_mode() {
        let remote =
            AnalysisPolicy::new("openai-compatible".into(), "explicit-model".into(), 128).unwrap();
        let local = AnalysisPolicy::new("ollama".into(), "explicit-model".into(), 128).unwrap();
        let from = |policy: &AnalysisPolicy, standalone_local: bool, pairs: &[(&str, &str)]| {
            FundingPolicy::from_values(policy, standalone_local, |key| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.to_string())
            })
        };

        // Multi-tenant / non-standalone mode must NEVER accept global operator env vars.
        assert!(
            from(
                &local,
                false,
                &[
                    ("OMNISOLO_LLM_TENANT_ID", "operator"),
                    ("OMNISOLO_USAGE_PAYER", "local"),
                ]
            )
            .is_err()
        );
        assert!(
            from(
                &remote,
                false,
                &[
                    ("OMNISOLO_LLM_TENANT_ID", "operator"),
                    ("OMNISOLO_USAGE_PAYER", "byok_api"),
                ]
            )
            .is_err()
        );

        // Standalone local mode rejects missing tenant, system tenant, non-local payer, or remote provider.
        assert!(from(&local, true, &[]).is_err());
        assert!(from(&local, true, &[("OMNISOLO_LLM_TENANT_ID", "operator")]).is_err());
        assert!(
            from(
                &local,
                true,
                &[
                    ("OMNISOLO_LLM_TENANT_ID", "system"),
                    ("OMNISOLO_USAGE_PAYER", "local"),
                ]
            )
            .is_err()
        );
        assert!(
            from(
                &local,
                true,
                &[
                    ("OMNISOLO_LLM_TENANT_ID", "operator"),
                    ("OMNISOLO_USAGE_PAYER", "byok_api"),
                ]
            )
            .is_err()
        );
        assert!(
            from(
                &local,
                true,
                &[
                    ("OMNISOLO_LLM_TENANT_ID", "operator"),
                    ("OMNISOLO_USAGE_PAYER", "native_subscription"),
                ]
            )
            .is_err()
        );
        assert!(
            from(
                &remote,
                true,
                &[
                    ("OMNISOLO_LLM_TENANT_ID", "operator"),
                    ("OMNISOLO_USAGE_PAYER", "local"),
                ]
            )
            .is_err()
        );
        assert!(
            from(
                &remote,
                true,
                &[
                    ("OMNISOLO_BUILDER_TENANT_ID", "operator"),
                    ("OMNISOLO_USAGE_PAYER", "byok_api"),
                ]
            )
            .is_err()
        );

        // Explicit standalone local Ollama with valid tenant succeeds.
        let resolved = from(
            &local,
            true,
            &[
                ("OMNISOLO_LLM_TENANT_ID", "operator"),
                ("OMNISOLO_USAGE_PAYER", "local"),
            ],
        )
        .unwrap();
        assert_eq!(resolved.tenant_id(), "operator");
        assert_eq!(resolved.payer, PayerMode::Local);
        assert!(resolved.customer_direct_inference_is_not_rebilled);

        // Explicit tenant-bound managed_api with rate card & ceiling succeeds even when
        // OMNISOLO_STANDALONE_MODE=false (strictly isolated to operator_tenant + UsageLedger).
        let rate_cards = serde_json::json!({
            "openai-compatible/explicit-model": {
                "revision": "owned-browser-v1",
                "input_micros_per_million": 1_000_000,
                "output_micros_per_million": 2_000_000,
                "cached_input_micros_per_million": 1_000_000
            }
        })
        .to_string();
        let managed = from(
            &remote,
            false,
            &[
                ("OMNISOLO_LLM_TENANT_ID", "e2e-owned-tenant"),
                ("OMNISOLO_USAGE_PAYER", "managed_api"),
                ("OMNISOLO_USAGE_MAX_REQUEST_MICROS", "20000"),
                ("OMNISOLO_USAGE_RATE_CARDS", rate_cards.as_str()),
            ],
        )
        .unwrap();
        assert_eq!(managed.tenant_id(), "e2e-owned-tenant");
        assert_eq!(managed.payer, PayerMode::ManagedApi);
        assert_eq!(managed.request_ceiling_micros, 20_000);
        assert_eq!(
            managed.rate_card.as_ref().map(|c| c.revision.as_str()),
            Some("owned-browser-v1")
        );

        // ManagedApi fails closed without matching rate card, ceiling, or valid tenant.
        assert!(
            from(
                &remote,
                false,
                &[
                    ("OMNISOLO_LLM_TENANT_ID", "system"),
                    ("OMNISOLO_USAGE_PAYER", "managed_api"),
                    ("OMNISOLO_USAGE_MAX_REQUEST_MICROS", "20000"),
                    ("OMNISOLO_USAGE_RATE_CARDS", rate_cards.as_str()),
                ]
            )
            .is_err()
        );
        assert!(
            from(
                &remote,
                false,
                &[
                    ("OMNISOLO_LLM_TENANT_ID", "e2e-owned-tenant"),
                    ("OMNISOLO_USAGE_PAYER", "managed_api"),
                    ("OMNISOLO_USAGE_MAX_REQUEST_MICROS", "0"),
                    ("OMNISOLO_USAGE_RATE_CARDS", rate_cards.as_str()),
                ]
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn resolve_tenant_funding_fails_closed_for_missing_or_invalid_tenant() {
        let (pool, database, ledger, vault) = sqlite_funding_harness().await;
        let policy = AnalysisPolicy::new("openai".into(), "gpt-4o-mini".into(), 256).unwrap();
        let managed = test_managed_config(&policy, 50_000);
        ledger.set_limit("tenant-a", 100_000).await.unwrap();
        insert_verified_vault_connection(&pool, "tenant-a", "openai_api", "sk-tenant-a-secret-key")
            .await;

        for invalid_tenant in [
            "",
            "   ",
            " tenant-a",
            "tenant-a ",
            "system",
            "SYSTEM",
            "bad\ntenant",
        ] {
            let err = resolve_tenant_funding(
                invalid_tenant,
                &policy,
                &database,
                Some(&vault),
                Some(&managed),
                false,
            )
            .await
            .err()
            .expect("invalid tenant must fail closed");
            assert_eq!(err, AdmissionError::Forbidden);
        }
    }

    #[tokio::test]
    async fn resolve_tenant_funding_isolates_byok_credentials_and_never_rebills() {
        let (pool, database, ledger, vault) = sqlite_funding_harness().await;
        let policy = AnalysisPolicy::new("openai".into(), "gpt-4o-mini".into(), 256).unwrap();
        let managed = test_managed_config(&policy, 50_000);

        ledger.set_limit("tenant-a", 100_000).await.unwrap();
        ledger.set_limit("tenant-b", 100_000).await.unwrap();
        insert_verified_vault_connection(
            &pool,
            "tenant-a",
            "openai_api",
            "sk-tenant-a-isolated-key",
        )
        .await;
        insert_verified_vault_connection(
            &pool,
            "tenant-b",
            "openai_api",
            "sk-tenant-b-isolated-key",
        )
        .await;

        let resolved_a = resolve_tenant_funding(
            "tenant-a",
            &policy,
            &database,
            Some(&vault),
            Some(&managed),
            false,
        )
        .await
        .unwrap();
        assert_eq!(resolved_a.tenant_id(), "tenant-a");
        assert_eq!(resolved_a.payer(), PayerMode::ByokApi);
        assert!(resolved_a.customer_direct_inference_is_not_rebilled());
        assert_eq!(resolved_a.policy.request_ceiling_micros, 0);
        assert_eq!(resolved_a.byok_api_key(), Some("sk-tenant-a-isolated-key"));

        let resolved_b = resolve_tenant_funding(
            "tenant-b",
            &policy,
            &database,
            Some(&vault),
            Some(&managed),
            false,
        )
        .await
        .unwrap();
        assert_eq!(resolved_b.tenant_id(), "tenant-b");
        assert_eq!(resolved_b.payer(), PayerMode::ByokApi);
        assert_eq!(resolved_b.byok_api_key(), Some("sk-tenant-b-isolated-key"));

        // Tenant C has no BYOK key and no UsageLedger budget -> fails closed.
        let resolved_c = resolve_tenant_funding(
            "tenant-c",
            &policy,
            &database,
            Some(&vault),
            Some(&managed),
            false,
        )
        .await;
        assert!(matches!(resolved_c, Err(AdmissionError::Unavailable)));

        // Cross-tenant ticket minting using Tenant A's context for Tenant B must fail closed.
        let cross_tenant_ticket = resolved_a.context.ticket(
            ("tenant-b", "actor-b"),
            "analyze revenue",
            &policy,
            "evt-cross".into(),
            "req-cross",
            (4096, &"a".repeat(64)),
        );
        assert!(matches!(cross_tenant_ticket, Err(Error::Forbidden)));

        // Settle real BYOK usage for Tenant A -> charged_micros MUST be 0 (never rebilled).
        let ticket_a = resolved_a
            .context
            .ticket(
                ("tenant-a", "actor-a"),
                "analyze revenue",
                &policy,
                "evt-byok-a".into(),
                "req-byok-a",
                (4096, &"a".repeat(64)),
            )
            .unwrap();
        assert_eq!(ticket_a.maximum, 0);
        let reserved = resolved_a.context.reserve_ticket(ticket_a).await.unwrap();
        resolved_a.context.dispatched(&reserved).await.unwrap();
        let settled = resolved_a
            .context
            .settle(
                &reserved,
                &UsageReceipt {
                    provider_request_id: "resp-byok-1".into(),
                    counts: Some(TokenCounts {
                        input: 500,
                        output: 100,
                        cached_input: 100,
                    }),
                },
            )
            .await;
        assert!(settled);
        let summary_a = ledger.summary("tenant-a").await.unwrap();
        assert_eq!(summary_a.spent_micros, 0);
        assert_eq!(summary_a.reserved_micros, 0);
        let records_a = ledger.records("tenant-a", "").await.unwrap();
        assert_eq!(records_a.len(), 1);
        assert_eq!(records_a[0].charged_micros, Some(0));
        assert!(records_a[0].provider_cost_micros.is_some());
        assert!(ledger.records("tenant-b", "").await.unwrap().is_empty());

        // Revoking Tenant A's BYOK key MUST fail closed (never silently fall back to ManagedApi),
        // while Tenant B's BYOK key remains isolated and active.
        vault.revoke("tenant-a", "openai_api").await.unwrap();
        let revoked_a = resolve_tenant_funding(
            "tenant-a",
            &policy,
            &database,
            Some(&vault),
            Some(&managed),
            false,
        )
        .await;
        assert!(matches!(revoked_a, Err(AdmissionError::Forbidden)));
        let still_active_b = resolve_tenant_funding(
            "tenant-b",
            &policy,
            &database,
            Some(&vault),
            Some(&managed),
            false,
        )
        .await
        .unwrap();
        assert_eq!(
            still_active_b.byok_api_key(),
            Some("sk-tenant-b-isolated-key")
        );
    }

    #[tokio::test]
    async fn resolve_tenant_funding_isolates_managed_api_budgets_across_tenants() {
        let (_pool, database, ledger, vault) = sqlite_funding_harness().await;
        let policy = AnalysisPolicy::new("anthropic".into(), "claude-sonnet".into(), 256).unwrap();
        let managed = test_managed_config(&policy, 20_000);

        ledger.set_limit("tenant-a", 25_000).await.unwrap();
        ledger.set_limit("tenant-zero", 0).await.unwrap();

        let resolved_a = resolve_tenant_funding(
            "tenant-a",
            &policy,
            &database,
            Some(&vault),
            Some(&managed),
            false,
        )
        .await
        .unwrap();
        assert_eq!(resolved_a.tenant_id(), "tenant-a");
        assert_eq!(resolved_a.payer(), PayerMode::ManagedApi);
        assert!(resolved_a.byok_api_key().is_none());

        // Unconfigured tenant-b and zero-limit tenant-zero MUST fail closed and never draw from tenant-a.
        assert!(matches!(
            resolve_tenant_funding(
                "tenant-b",
                &policy,
                &database,
                Some(&vault),
                Some(&managed),
                false,
            )
            .await,
            Err(AdmissionError::Unavailable)
        ));
        assert!(matches!(
            resolve_tenant_funding(
                "tenant-zero",
                &policy,
                &database,
                Some(&vault),
                Some(&managed),
                false,
            )
            .await,
            Err(AdmissionError::Unavailable)
        ));

        // Reserve and settle real ManagedApi micro-USD strictly against tenant-a.
        let ticket_a = resolved_a
            .context
            .ticket(
                ("tenant-a", "actor-a"),
                "quarterly summary",
                &policy,
                "evt-managed-a".into(),
                "req-managed-a",
                (4096, &"b".repeat(64)),
            )
            .unwrap();
        assert_eq!(ticket_a.maximum, 4096 + 256 * 2);
        let reserved = resolved_a.context.reserve_ticket(ticket_a).await.unwrap();
        assert_eq!(
            ledger.summary("tenant-a").await.unwrap().reserved_micros,
            4096 + 256 * 2
        );
        resolved_a.context.dispatched(&reserved).await.unwrap();
        assert!(
            resolved_a
                .context
                .settle(
                    &reserved,
                    &UsageReceipt {
                        provider_request_id: "resp-managed-1".into(),
                        counts: Some(TokenCounts {
                            input: 1_000,
                            output: 200,
                            cached_input: 0,
                        }),
                    },
                )
                .await
        );
        let summary_a = ledger.summary("tenant-a").await.unwrap();
        assert_eq!(summary_a.reserved_micros, 0);
        assert_eq!(summary_a.spent_micros, 1_400);
        assert!(ledger.summary("tenant-b").await.is_err());
    }

    #[tokio::test]
    async fn resolve_tenant_funding_allows_local_ollama_only_in_standalone_mode() {
        let (_pool, database, _ledger, vault) = sqlite_funding_harness().await;
        let local_policy = AnalysisPolicy::new("ollama".into(), "llama3.2".into(), 256).unwrap();

        // Multi-tenant mode must fail closed for local Ollama fallback.
        assert!(matches!(
            resolve_tenant_funding(
                "tenant-local",
                &local_policy,
                &database,
                Some(&vault),
                None,
                false,
            )
            .await,
            Err(AdmissionError::Unavailable)
        ));

        // Standalone local mode permits Local Ollama scoped to the validated tenant.
        let resolved = resolve_tenant_funding(
            "tenant-local",
            &local_policy,
            &database,
            Some(&vault),
            None,
            true,
        )
        .await
        .unwrap();
        assert_eq!(resolved.tenant_id(), "tenant-local");
        assert_eq!(resolved.payer(), PayerMode::Local);
        assert_eq!(resolved.policy.request_ceiling_micros, 0);
        assert!(resolved.customer_direct_inference_is_not_rebilled());
        assert!(resolved.byok_api_key().is_none());
    }
}
