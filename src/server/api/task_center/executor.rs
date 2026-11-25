//! Fail-closed LLM executor, UsageLedger micro-USD budget enforcement,
//! and standing-authority policy evaluator.
use super::provider::{invoke_provider, sanitize_artifact_content};
pub use super::sse::{SseEventKind, TaskCenterEventHub, TaskCenterEventStream, format_sse_frame};
use super::store::TaskCenterStore;
use super::types::{
    ActionRiskTier, ArtifactFormat, AttentionRing, MAX_MICROS, NewStep, PayerMode, ProvenanceState,
    StandingPolicyRecord, StepKind, TaskArtifactRecord, TaskCenterBudgetInfo,
    TaskCenterRuntimeInfo, TaskCenterTaskStatus, TaskTransition,
};
use crate::db::{DB, DbStore};
use chrono::Utc;
use dashmap::DashMap;
use server_harness::middleware::{
    connection_vault::ConnectionVault,
    usage_ledger::{
        Admission, LedgerError, PayerMode as LedgerPayerMode, RateCard, TokenCounts, UsageLedger,
        UsageReceipt, UsageScope,
    },
};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 60;

#[derive(Clone)]
pub struct RedactedSecret(String);
impl RedactedSecret {
    pub fn new(value: String) -> Self {
        Self(value)
    }
    pub(super) fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for RedactedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Clone, Debug)]
pub struct TaskCenterLlmConfig {
    pub provider: String,
    pub model: String,
    pub endpoint: String,
    pub api_key: Option<RedactedSecret>,
    pub payer_mode: PayerMode,
    pub rate_card: Option<RateCard>,
    pub max_output_tokens: i32,
    pub request_ceiling_micros: i64,
    pub timeout_secs: u64,
}

impl TaskCenterLlmConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !matches!(
            self.provider.as_str(),
            "openai" | "openai-compatible" | "anthropic" | "minimax" | "ollama"
        ) || self.model.trim().is_empty()
            || self.model.len() > 200
            || self.endpoint.trim().is_empty()
            || !(1..=4096).contains(&self.max_output_tokens)
            || self.payer_mode == PayerMode::Unconfigured
        {
            return Err("invalid llm runtime configuration");
        }
        let parsed = reqwest::Url::parse(&self.endpoint).map_err(|_| "invalid llm endpoint url")?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err("llm endpoint must be http or https");
        }
        if self.payer_mode == PayerMode::ManagedApi
            && (self.rate_card.is_none()
                || !(1..=MAX_MICROS).contains(&self.request_ceiling_micros))
        {
            return Err("managed_api requires a valid rate card and positive request ceiling");
        }
        if let Some(card) = &self.rate_card {
            card.cost(&TokenCounts {
                input: 0,
                output: 0,
                cached_input: 0,
            })
            .map_err(|_| "invalid rate card")?;
        }
        Ok(())
    }

    pub fn from_environment() -> Option<Self> {
        let non_empty = |k: &str| {
            std::env::var(k)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let provider = non_empty("OMNISOLO_LLM_PROVIDER")?;
        let model = non_empty("OMNISOLO_LLM_MODEL")?;
        let max_output_tokens = non_empty("OMNISOLO_MAX_TOKENS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(2048);
        let endpoint = non_empty("OMNISOLO_LLM_ENDPOINT")
            .or_else(|| non_empty("OMNISOLO_LOCAL_LLM_ENDPOINT"))
            .or_else(|| match provider.as_str() {
                "openai" => Some("https://api.openai.com/v1/chat/completions".into()),
                "anthropic" => Some("https://api.anthropic.com/v1/messages".into()),
                _ => None,
            })?;
        let key_env = match provider.as_str() {
            "anthropic" => "ANTHROPIC_API_KEY",
            "minimax" => "MINIMAX_API_KEY",
            _ => "OPENAI_API_KEY",
        };
        let api_key = non_empty("OMNISOLO_LLM_API_KEY")
            .or_else(|| (provider != "ollama").then(|| non_empty(key_env)).flatten())
            .map(RedactedSecret::new);
        if provider != "ollama" && api_key.is_none() {
            return None;
        }
        let payer_mode = match non_empty("OMNISOLO_USAGE_PAYER").as_deref() {
            Some("managed_api") => PayerMode::ManagedApi,
            Some("byok_api") => PayerMode::ByokApi,
            Some("local") => PayerMode::Local,
            _ => return None,
        };
        if (payer_mode == PayerMode::Local) != (provider == "ollama") {
            return None;
        }
        let cards: BTreeMap<String, RateCard> = non_empty("OMNISOLO_USAGE_RATE_CARDS")
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        let rate_card = cards.get(&format!("{provider}/{model}")).cloned();
        let request_ceiling_micros = if payer_mode == PayerMode::ManagedApi {
            non_empty("OMNISOLO_USAGE_MAX_REQUEST_MICROS")?
                .parse()
                .ok()?
        } else {
            0
        };
        let cfg = Self {
            provider,
            model,
            endpoint,
            api_key,
            payer_mode,
            rate_card,
            max_output_tokens,
            request_ceiling_micros,
            timeout_secs: DEFAULT_HTTP_TIMEOUT_SECS,
        };
        cfg.validate().ok()?;
        Some(cfg)
    }

    fn ledger_payer(&self) -> LedgerPayerMode {
        match self.payer_mode {
            PayerMode::ManagedApi => LedgerPayerMode::ManagedApi,
            PayerMode::ByokApi => LedgerPayerMode::ByokApi,
            PayerMode::Local | PayerMode::Unconfigured => LedgerPayerMode::Local,
        }
    }
}

#[derive(Debug)]
pub enum ExecutorError {
    Unconfigured(&'static str),
    BudgetExhausted(&'static str),
    BudgetUnconfigured(&'static str),
    Invalid(&'static str),
    Conflict(&'static str),
    LedgerUnavailable,
}

pub fn evaluate_standing_authority(
    policy: &StandingPolicyRecord,
    requested_action: &str,
    estimated_cost_micros: i64,
) -> ActionRiskTier {
    let action = requested_action.trim().to_ascii_lowercase();
    let external = action.starts_with("send_")
        || action.starts_with("publish")
        || matches!(action.as_str(), "external_send" | "send_email" | "send_sms");
    let financial = [
        "invoice",
        "payment",
        "refund",
        "charge",
        "checkout",
        "financial_commit",
    ]
    .iter()
    .any(|k| action.contains(k));
    if (external && !policy.allow_external_send)
        || (financial && !policy.allow_financial_commit)
        || estimated_cost_micros > policy.auto_approve_max_micros
    {
        ActionRiskTier::DraftForReview
    } else {
        ActionRiskTier::AutoExecute
    }
}

pub struct ExecutionJob {
    pub store: TaskCenterStore,
    pub events: Arc<TaskCenterEventHub>,
    pub tenant_id: String,
    pub task_id: String,
    pub space_id: Option<String>,
    pub prompt: String,
    pub cfg: TaskCenterLlmConfig,
    pub usage_event_id: Option<String>,
}

struct ActiveCancelGuard(Arc<DashMap<String, watch::Sender<bool>>>, String);
impl Drop for ActiveCancelGuard {
    fn drop(&mut self) {
        self.0.remove(&self.1);
    }
}

pub struct TaskCenterExecutor {
    db: Arc<DB>,
    runtime: Option<TaskCenterLlmConfig>,
    client: reqwest::Client,
    active_cancels: Arc<DashMap<String, watch::Sender<bool>>>,
}

impl TaskCenterExecutor {
    pub fn from_environment(db: Arc<DB>) -> Self {
        Self::new(db, TaskCenterLlmConfig::from_environment())
    }
    pub fn unconfigured(db: Arc<DB>) -> Self {
        Self::new(db, None)
    }
    pub fn with_config(db: Arc<DB>, config: TaskCenterLlmConfig) -> Result<Self, &'static str> {
        config.validate()?;
        Ok(Self::new(db, Some(config)))
    }
    fn new(db: Arc<DB>, runtime: Option<TaskCenterLlmConfig>) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("valid reqwest client configuration");
        Self {
            db,
            runtime,
            client,
            active_cancels: Arc::new(DashMap::new()),
        }
    }

    pub fn usage_ledger(&self) -> UsageLedger {
        match &self.db.store {
            DbStore::Postgres => UsageLedger::Postgres(self.db.pool.clone()),
            DbStore::Sqlite(pool) => UsageLedger::Sqlite(pool.clone()),
        }
    }

    pub async fn resolve_for_tenant(&self, tenant_id: &str) -> Option<TaskCenterLlmConfig> {
        if let Some(cfg) = &self.runtime {
            return Some(cfg.clone());
        }
        std::env::var_os("OMNISOLO_CONNECTION_KEYS")?;
        let vault = ConnectionVault::from_environment(self.usage_ledger()).ok()?;
        let env_model = std::env::var("OMNISOLO_LLM_MODEL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        for (vault_id, provider, default_model, endpoint) in [
            (
                "openai_api",
                "openai",
                "gpt-4o-mini",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                "anthropic_api",
                "anthropic",
                "claude-3-5-sonnet-20241022",
                "https://api.anthropic.com/v1/messages",
            ),
        ] {
            if let Ok(key) = vault.read_key(tenant_id, vault_id).await {
                return Some(TaskCenterLlmConfig {
                    provider: provider.into(),
                    model: env_model.unwrap_or_else(|| default_model.into()),
                    endpoint: endpoint.into(),
                    api_key: Some(RedactedSecret::new(key.to_string())),
                    payer_mode: PayerMode::ByokApi,
                    rate_card: None,
                    max_output_tokens: 2048,
                    request_ceiling_micros: 0,
                    timeout_secs: DEFAULT_HTTP_TIMEOUT_SECS,
                });
            }
        }
        None
    }

    pub async fn runtime_info(&self, tenant_id: &str) -> TaskCenterRuntimeInfo {
        match self.resolve_for_tenant(tenant_id).await {
            Some(cfg) => TaskCenterRuntimeInfo {
                llm_configured: true,
                provider: Some(cfg.provider),
                model: Some(cfg.model),
                payer_mode: cfg.payer_mode,
                reason: None,
            },
            None => TaskCenterRuntimeInfo {
                llm_configured: false,
                provider: None,
                model: None,
                payer_mode: PayerMode::Unconfigured,
                reason: Some("No LLM provider is configured. Set OMNISOLO_LLM_PROVIDER/OMNISOLO_LLM_MODEL or connect a verified OpenAI/Anthropic API key in Integrations.".into()),
            },
        }
    }

    pub async fn budget_info(&self, tenant_id: &str) -> TaskCenterBudgetInfo {
        let (configured, limit_micros, spent_micros, reserved_micros) =
            match self.usage_ledger().summary(tenant_id).await {
                Ok(s) => (true, s.limit_micros, s.spent_micros, s.reserved_micros),
                Err(_) => (false, 0, 0, 0),
            };
        TaskCenterBudgetInfo {
            configured,
            limit_micros,
            spent_micros,
            reserved_micros,
            currency: "USD".into(),
            unit: "micro_usd".into(),
            customer_direct_inference_is_not_rebilled: true,
        }
    }

    pub fn estimate_cost_micros(
        cfg: &TaskCenterLlmConfig,
        prompt: &str,
        caller_estimate: Option<i64>,
    ) -> Result<i64, ExecutorError> {
        let caller = match caller_estimate {
            Some(v) if (0..=MAX_MICROS).contains(&v) => v,
            Some(_) => {
                return Err(ExecutorError::Invalid(
                    "estimated_cost_micros out of bounds",
                ));
            }
            None => 0,
        };
        if cfg.payer_mode != PayerMode::ManagedApi {
            return Ok(caller);
        }
        let rate = cfg
            .rate_card
            .as_ref()
            .ok_or(ExecutorError::Unconfigured("managed_api rate card missing"))?;
        let input = i64::try_from(prompt.len())
            .unwrap_or(1_000_000)
            .saturating_add(512)
            .min(1_000_000);
        let output = i64::from(cfg.max_output_tokens);
        let uncached = rate
            .cost(&TokenCounts {
                input,
                output,
                cached_input: 0,
            })
            .map_err(|_| ExecutorError::Invalid("cost overflow"))?;
        let cached = rate
            .cost(&TokenCounts {
                input,
                output,
                cached_input: input,
            })
            .map_err(|_| ExecutorError::Invalid("cost overflow"))?;
        let computed = uncached.max(cached).max(caller);
        if computed > cfg.request_ceiling_micros {
            return Err(ExecutorError::BudgetExhausted(
                "estimated task cost exceeds per-request micro-USD ceiling",
            ));
        }
        Ok(computed)
    }

    pub async fn reserve_usage(
        &self,
        tenant_id: &str,
        task_id: &str,
        event_id: &str,
        request_digest: &str,
        cfg: &TaskCenterLlmConfig,
        estimated_cost_micros: i64,
    ) -> Result<bool, ExecutorError> {
        let scope = UsageScope {
            tenant_id: tenant_id.into(),
            task_id: task_id.into(),
            attempt_id: event_id.into(),
            provider: cfg.provider.clone(),
            model: cfg.model.clone(),
            payer: cfg.ledger_payer(),
            rate_card: cfg.rate_card.clone(),
        };
        let reserve_micros = if cfg.payer_mode == PayerMode::ManagedApi {
            estimated_cost_micros
        } else {
            0
        };
        match self
            .usage_ledger()
            .reserve(&scope, event_id, request_digest, reserve_micros)
            .await
        {
            Ok(Admission::Admitted | Admission::Duplicate) => Ok(true),
            Err(LedgerError::Limit) => Err(ExecutorError::BudgetExhausted(
                "usage budget exhausted; no task was dispatched",
            )),
            Err(LedgerError::Unconfigured) if cfg.payer_mode == PayerMode::ManagedApi => Err(
                ExecutorError::BudgetUnconfigured("usage budget is not configured for managed_api"),
            ),
            Err(LedgerError::Unconfigured) => Ok(false),
            Err(LedgerError::Conflict | LedgerError::State) => Err(ExecutorError::Conflict(
                "usage reservation conflicts with existing execution",
            )),
            Err(LedgerError::Invalid) => Err(ExecutorError::Invalid("invalid usage reservation")),
            Err(LedgerError::Database) => Err(ExecutorError::LedgerUnavailable),
        }
    }

    pub async fn cancel_usage_before_dispatch(&self, tenant_id: &str, event_id: &str) {
        let _ = self
            .usage_ledger()
            .cancel_before_dispatch(tenant_id, event_id)
            .await;
    }

    pub async fn mark_usage_reconciliation_required(&self, tenant_id: &str, event_id: &str) {
        let _ = self
            .usage_ledger()
            .require_reconciliation(tenant_id, event_id)
            .await;
    }

    pub fn signal_cancel(&self, task_id: &str) {
        if let Some(tx) = self.active_cancels.get(task_id) {
            let _ = tx.send(true);
        }
    }

    pub fn spawn_execution(self: &Arc<Self>, job: ExecutionJob) {
        let (cancel_tx, cancel_rx) = watch::channel(false);
        self.active_cancels.insert(job.task_id.clone(), cancel_tx);
        let executor = self.clone();
        tokio::spawn(async move { executor.run_job(job, cancel_rx).await });
    }

    async fn run_job(self: Arc<Self>, job: ExecutionJob, mut cancel_rx: watch::Receiver<bool>) {
        let _guard = ActiveCancelGuard(self.active_cancels.clone(), job.task_id.clone());
        if *cancel_rx.borrow() {
            return;
        }
        let start_ok = job
            .store
            .transition_task_cas(
                &job.tenant_id,
                &job.task_id,
                &TaskTransition {
                    expected: TaskCenterTaskStatus::Queued,
                    status: TaskCenterTaskStatus::Running,
                    provenance: ProvenanceState::Draft,
                    attention: AttentionRing::None,
                    actual_cost_micros: None,
                    receipt_id: None,
                    output_summary: None,
                    error_message: None,
                    now: Utc::now().timestamp(),
                },
            )
            .await
            .unwrap_or(false);
        if !start_ok {
            return;
        }
        if let Some(eid) = job.usage_event_id.as_deref()
            && self
                .usage_ledger()
                .dispatched(&job.tenant_id, eid)
                .await
                .is_err()
        {
            self.fail_unknown(&job, "Usage ledger dispatch failed; outcome is unknown")
                .await;
            return;
        }
        publish_task(&job.store, &job.events, &job.tenant_id, &job.task_id).await;

        let call = invoke_provider(&self.client, &job.cfg, &job.prompt);
        tokio::pin!(call);
        let timeout = tokio::time::sleep(Duration::from_secs(job.cfg.timeout_secs.max(1)));
        tokio::pin!(timeout);
        let result = tokio::select! {
            biased;
            _ = cancel_rx.changed() => {
                if let Some(eid) = job.usage_event_id.as_deref() {
                    self.mark_usage_reconciliation_required(&job.tenant_id, eid).await;
                }
                return;
            }
            _ = &mut timeout => Err("Provider inference timed out; outcome is unknown"),
            res = &mut call => res,
        };
        let resp = match result {
            Ok(r) => r,
            Err(reason) => {
                self.fail_unknown(&job, reason).await;
                return;
            }
        };

        let mut actual_cost = 0_i64;
        if let Some(eid) = job.usage_event_id.as_deref() {
            let receipt = UsageReceipt {
                provider_request_id: resp.provider_request_id.clone(),
                counts: resp.counts.clone(),
            };
            match self
                .usage_ledger()
                .settle(&job.tenant_id, eid, &receipt)
                .await
            {
                Ok(()) => {
                    if job.cfg.payer_mode == PayerMode::ManagedApi
                        && let (Some(card), Some(counts)) = (&job.cfg.rate_card, &resp.counts)
                    {
                        actual_cost = card.cost(counts).unwrap_or(0);
                    }
                }
                Err(_) if job.cfg.payer_mode == PayerMode::ManagedApi => {
                    self.fail_unknown(
                        &job,
                        "Usage ledger settlement requires reconciliation; outcome retained as unknown",
                    )
                    .await;
                    return;
                }
                Err(_) => {}
            }
        }

        let now = Utc::now().timestamp();
        let safe = sanitize_artifact_content(&resp.content);
        let summary: String = safe.chars().take(500).collect();
        let finished = job
            .store
            .transition_task_cas(
                &job.tenant_id,
                &job.task_id,
                &TaskTransition {
                    expected: TaskCenterTaskStatus::Running,
                    status: TaskCenterTaskStatus::Completed,
                    provenance: ProvenanceState::Verified,
                    attention: AttentionRing::CompletedUnread,
                    actual_cost_micros: Some(actual_cost),
                    receipt_id: Some(&resp.provider_request_id),
                    output_summary: Some(&summary),
                    error_message: None,
                    now,
                },
            )
            .await
            .unwrap_or(false);
        if !finished {
            return;
        }
        if let Ok(step) = job
            .store
            .append_step(
                &job.tenant_id,
                &job.task_id,
                &NewStep {
                    kind: StepKind::LlmTurn,
                    title: &format!("Completed via {}/{}", job.cfg.provider, job.cfg.model),
                    detail: &summary,
                    status: TaskCenterTaskStatus::Completed,
                    provenance: ProvenanceState::Verified,
                    tool_name: Some(&job.cfg.provider),
                    cost_micros: actual_cost,
                    now,
                },
            )
            .await
        {
            job.events.publish(
                &job.tenant_id,
                SseEventKind::StepAppended,
                serde_json::to_value(&step).unwrap_or_default(),
            );
        }
        let art = TaskArtifactRecord {
            id: format!("art-{}", Uuid::new_v4()),
            task_id: job.task_id.clone(),
            space_id: job.space_id.clone(),
            title: format!("Result ({})", job.cfg.model),
            format: ArtifactFormat::Markdown,
            content: safe,
            provenance: ProvenanceState::Verified,
            pinned: false,
            created_at_unix: now,
        };
        if let Ok(saved) = job.store.insert_artifact(&job.tenant_id, &art).await {
            job.events.publish(
                &job.tenant_id,
                SseEventKind::ArtifactCreated,
                serde_json::to_value(&saved).unwrap_or_default(),
            );
        }
        publish_task(&job.store, &job.events, &job.tenant_id, &job.task_id).await;
    }

    async fn fail_unknown(&self, job: &ExecutionJob, reason: &str) {
        if let Some(eid) = job.usage_event_id.as_deref() {
            self.mark_usage_reconciliation_required(&job.tenant_id, eid)
                .await;
        }
        let now = Utc::now().timestamp();
        let updated = job
            .store
            .transition_task_cas(
                &job.tenant_id,
                &job.task_id,
                &TaskTransition {
                    expected: TaskCenterTaskStatus::Running,
                    status: TaskCenterTaskStatus::OutcomeUnknown,
                    provenance: ProvenanceState::OutcomeUnknown,
                    attention: AttentionRing::Blocked,
                    actual_cost_micros: None,
                    receipt_id: None,
                    output_summary: None,
                    error_message: Some(reason),
                    now,
                },
            )
            .await
            .unwrap_or(false);
        if updated {
            let _ = job
                .store
                .append_step(
                    &job.tenant_id,
                    &job.task_id,
                    &NewStep {
                        kind: StepKind::System,
                        title: "Provider outcome unknown",
                        detail: reason,
                        status: TaskCenterTaskStatus::OutcomeUnknown,
                        provenance: ProvenanceState::OutcomeUnknown,
                        tool_name: None,
                        cost_micros: 0,
                        now,
                    },
                )
                .await;
            publish_task(&job.store, &job.events, &job.tenant_id, &job.task_id).await;
        }
    }
}

async fn publish_task(
    store: &TaskCenterStore,
    events: &TaskCenterEventHub,
    tenant_id: &str,
    task_id: &str,
) {
    if let Ok(Some(meta)) = store.get_task_meta(tenant_id, task_id).await {
        events.publish(
            tenant_id,
            SseEventKind::TaskUpdated,
            serde_json::to_value(&meta.task).unwrap_or_default(),
        );
    }
}
