use ::ohc_core::extensions::UsageAccountingEvent;
use dashmap::DashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalUsageEvent {
    pub workspace_id: String,
    pub actor_id: String,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub local_embedding_tokens: i64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ActorUsageSnapshot {
    pub cost_usd: f64,
    pub tokens: i64,
    pub cost_micro_usd: i64,
}

/// Solo-owner local token & micro-USD cost accumulator backing task/agent metrics
/// without proprietary plan tiers or rebilling logic.
#[derive(Debug, Default)]
pub struct LocalUsageTracker {
    entries: DashMap<(String, String), ActorUsageSnapshot>,
}

impl LocalUsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_event(&self, event: LocalUsageEvent) -> (f64, UsageAccountingEvent) {
        let model_name = if event.model.trim().is_empty() {
            "default"
        } else {
            event.model.trim()
        };
        let cost_usd = ::ohc_core::prompt::model_cost::calculate_cost(
            model_name,
            event.input_tokens.max(0),
            event.output_tokens.max(0),
            event.cached_input_tokens.max(0),
        );
        let micro_usd = (cost_usd * 1_000_000.0).round() as i64;
        let tokens = event
            .input_tokens
            .max(0)
            .saturating_add(event.output_tokens.max(0));

        let mut entry = self
            .entries
            .entry((event.workspace_id.clone(), event.actor_id.clone()))
            .or_default();
        entry.cost_micro_usd = entry.cost_micro_usd.saturating_add(micro_usd);
        entry.cost_usd = entry.cost_micro_usd as f64 / 1_000_000.0;
        entry.tokens = entry.tokens.saturating_add(tokens);

        let accounting = UsageAccountingEvent {
            workspace_id: event.workspace_id,
            actor_id: event.actor_id,
            model: model_name.to_string(),
            input_tokens: event.input_tokens.max(0) as u64,
            output_tokens: event.output_tokens.max(0) as u64,
            cached_input_tokens: event.cached_input_tokens.max(0) as u64,
            estimated_cost_micro_usd: micro_usd,
        };
        (cost_usd, accounting)
    }

    pub fn record_manual_cost(&self, actor_id: &str, workspace_id: &str, cents: i64) {
        let micro_usd = cents.saturating_mul(10_000);
        let mut entry = self
            .entries
            .entry((workspace_id.to_string(), actor_id.to_string()))
            .or_default();
        entry.cost_micro_usd = entry.cost_micro_usd.saturating_add(micro_usd);
        entry.cost_usd = entry.cost_micro_usd as f64 / 1_000_000.0;
    }

    pub fn tenant_agent_snapshot(&self, workspace_id: &str) -> Vec<(String, ActorUsageSnapshot)> {
        self.entries
            .iter()
            .filter(|item| item.key().0 == workspace_id)
            .map(|item| (item.key().1.clone(), *item.value()))
            .collect()
    }

    pub fn get_tenant_cost(&self, workspace_id: &str) -> f64 {
        let total_micro: i64 = self
            .entries
            .iter()
            .filter(|item| item.key().0 == workspace_id)
            .map(|item| item.value().cost_micro_usd)
            .fold(0_i64, i64::saturating_add);
        total_micro as f64 / 1_000_000.0
    }

    pub fn get_total_cost(&self) -> f64 {
        let total_micro: i64 = self
            .entries
            .iter()
            .map(|item| item.value().cost_micro_usd)
            .fold(0_i64, i64::saturating_add);
        total_micro as f64 / 1_000_000.0
    }

    pub fn total_recorded_tokens(&self, workspace_id: &str) -> i64 {
        self.entries
            .iter()
            .filter(|item| item.key().0 == workspace_id)
            .map(|item| item.value().tokens)
            .fold(0_i64, i64::saturating_add)
    }

    pub fn calculate_efficiency(&self, cost_usd: f64, tokens: i64) -> f64 {
        if cost_usd <= 0.0 {
            if tokens > 0 { 1.0 } else { 0.0 }
        } else {
            (tokens as f64 / cost_usd).min(1_000_000.0)
        }
    }
}
