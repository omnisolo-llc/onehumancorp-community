use crate::local_usage::{LocalUsageEvent, LocalUsageTracker};
use crate::scheduler::Scheduler;
use crate::tasks::TaskManager;
use ::ohc_core::defaults::{
    InProcessLocalTransport, LocalOwnerAuthPolicy, NoOpPlatformBilling, SingleTenantContext,
};
use ::ohc_core::extensions::{
    AuthPolicyProvider, BillingPlatformProvider, SyncTransportProvider, TenantContextProvider,
    WorkspaceScope,
};
use ::server_omnisolo::orchestration::{
    Agent, AgentCapabilities, MeetingRoom, MeshEvent, Message, TeammateMeshEvent,
};
use ::server_utils::cache::DistributedCacheBackend;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

pub(crate) const MAX_INBOX_MESSAGES_PER_AGENT: usize = 500;
const MAX_INBOX_AGENTS: usize = 4_096;
pub(crate) const MAX_RECENT_EVENTS: usize = 200;
const MAX_AUTO_COR_INFLIGHT: usize = 4_096;

/// Tenant-scoped cache key for a single workspace's agent roster.
pub fn tenant_agents_cache_key(tenant_id: &str) -> String {
    format!("hub:agents:tenant:{tenant_id}")
}

/// Tenant-scoped cache key for a single workspace's meetings.
pub fn tenant_meetings_cache_key(tenant_id: &str) -> String {
    format!("hub:meetings:tenant:{tenant_id}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubEvent {
    pub r#type: String,
    pub payload: String,
    pub occurred_at: DateTime<Utc>,
}

pub struct AppContext {
    pub semantic_router: crate::orchestration::router::SemanticRouter,
    agents: RwLock<HashMap<String, Agent>>,
    meetings: RwLock<HashMap<String, MeetingRoom>>,
    meeting_orgs: RwLock<HashMap<String, String>>,
    inbox: RwLock<HashMap<String, VecDeque<Message>>>,
    subs: RwLock<HashMap<String, broadcast::Sender<Message>>>,
    minimax_api_key: String,
    caps_tx: broadcast::Sender<AgentCapabilities>,
    mesh_events: RwLock<HashMap<String, broadcast::Sender<MeshEvent>>>,
    teammate_events: RwLock<HashMap<String, broadcast::Sender<TeammateMeshEvent>>>,
    task_manager: TaskManager,
    scheduler: Scheduler,
    recent_events: RwLock<VecDeque<HubEvent>>,
    auto_cor_track: std::sync::Mutex<HashSet<String>>,
    event_log_tx: mpsc::Sender<serde_json::Value>,
    pub pool: sqlx::PgPool,
    agent_cache: RwLock<Option<Arc<Vec<Agent>>>>,
    tenant_agent_cache: RwLock<HashMap<String, Arc<Vec<Agent>>>>,
    execution_measurements:
        RwLock<HashMap<String, crate::api::agent_metrics::ExecutionMeasurements>>,
    meetings_cache: RwLock<Option<Arc<Vec<MeetingRoom>>>>,
    tenant_meetings_cache: RwLock<HashMap<String, Arc<Vec<MeetingRoom>>>>,
    referral_tracker: Arc<crate::services::growth::referrals::ReferralTracker>,
    tenant_context: Arc<dyn TenantContextProvider>,
    auth_policy: Arc<dyn AuthPolicyProvider>,
    billing_platform: Arc<dyn BillingPlatformProvider>,
    sync_transport: Arc<dyn SyncTransportProvider>,
    distributed_cache: Option<Arc<dyn DistributedCacheBackend>>,
    usage_tracker: Arc<LocalUsageTracker>,
}

pub type Hub = AppContext;

impl AppContext {
    pub fn usage_ledger(&self) -> Option<::server_harness::middleware::usage_ledger::UsageLedger> {
        use ::server_harness::middleware::usage_ledger::UsageLedger;
        let db = self.task_manager.db.read().ok()?.clone()?;
        Some(match &db.store {
            crate::db::DbStore::Postgres => UsageLedger::Postgres(db.pool.clone()),
            crate::db::DbStore::Sqlite(pool) => UsageLedger::Sqlite(pool.clone()),
        })
    }

    pub fn set_db(&self, db: std::sync::Arc<crate::db::DB>) {
        *self.task_manager.db.write().unwrap() = Some(db);
    }

    pub fn new(event_log_tx: mpsc::Sender<serde_json::Value>, pool: sqlx::PgPool) -> Self {
        let workspace_id = std::env::var("OHC_DEFAULT_TENANT_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                let configured = ::server_config::get().bootstrap_org_id.trim();
                if configured.is_empty() {
                    "default".to_string()
                } else {
                    configured.to_string()
                }
            });
        let tenant_context = SingleTenantContext::new(workspace_id.clone())
            .unwrap_or_else(|_| SingleTenantContext::new("default").expect("default scope valid"));
        let auth_policy = LocalOwnerAuthPolicy::new(workspace_id)
            .unwrap_or_else(|_| LocalOwnerAuthPolicy::new("default").expect("default auth valid"));
        Self::new_with_extensions(
            event_log_tx,
            pool,
            Arc::new(tenant_context),
            Arc::new(auth_policy),
            Arc::new(NoOpPlatformBilling),
            Arc::new(InProcessLocalTransport::default()),
            None,
        )
    }

    pub fn new_with_extensions(
        event_log_tx: mpsc::Sender<serde_json::Value>,
        pool: sqlx::PgPool,
        tenant_context: Arc<dyn TenantContextProvider>,
        auth_policy: Arc<dyn AuthPolicyProvider>,
        billing_platform: Arc<dyn BillingPlatformProvider>,
        sync_transport: Arc<dyn SyncTransportProvider>,
        distributed_cache: Option<Arc<dyn DistributedCacheBackend>>,
    ) -> Self {
        let minimax_api_key = std::env::var("MINIMAX_API_KEY").unwrap_or_default();
        let (caps_tx, _) = broadcast::channel(100);

        AppContext {
            semantic_router: crate::orchestration::router::SemanticRouter::new(),
            agents: RwLock::new(HashMap::new()),
            agent_cache: RwLock::new(None),
            tenant_agent_cache: RwLock::new(HashMap::new()),
            execution_measurements: RwLock::new(HashMap::new()),
            meetings: RwLock::new(HashMap::new()),
            meeting_orgs: RwLock::new(HashMap::new()),
            meetings_cache: RwLock::new(None),
            tenant_meetings_cache: RwLock::new(HashMap::new()),
            inbox: RwLock::new(HashMap::new()),
            subs: RwLock::new(HashMap::new()),
            minimax_api_key,
            caps_tx,
            pool,
            mesh_events: RwLock::new(HashMap::new()),
            teammate_events: RwLock::new(HashMap::new()),
            task_manager: TaskManager::new(),
            scheduler: Scheduler::new(),
            recent_events: RwLock::new(VecDeque::new()),
            auto_cor_track: std::sync::Mutex::new(HashSet::new()),
            event_log_tx,
            referral_tracker: Arc::new(crate::services::growth::referrals::ReferralTracker::new()),
            tenant_context,
            auth_policy,
            billing_platform,
            sync_transport,
            distributed_cache,
            usage_tracker: Arc::new(LocalUsageTracker::new()),
        }
    }

    pub fn referral_tracker(&self) -> Arc<crate::services::growth::referrals::ReferralTracker> {
        self.referral_tracker.clone()
    }

    pub fn distributed_cache(&self) -> Option<Arc<dyn DistributedCacheBackend>> {
        self.distributed_cache.clone()
    }

    pub fn tenant_context(&self) -> Arc<dyn TenantContextProvider> {
        self.tenant_context.clone()
    }

    pub fn auth_policy(&self) -> Arc<dyn AuthPolicyProvider> {
        self.auth_policy.clone()
    }

    pub fn billing_platform(&self) -> Arc<dyn BillingPlatformProvider> {
        self.billing_platform.clone()
    }

    pub fn sync_transport(&self) -> Arc<dyn SyncTransportProvider> {
        self.sync_transport.clone()
    }

    pub fn usage_tracker(&self) -> Arc<LocalUsageTracker> {
        self.usage_tracker.clone()
    }

    pub async fn record_usage_event(&self, event: LocalUsageEvent) -> f64 {
        let total_tokens = event.input_tokens.saturating_add(event.output_tokens);
        let output_tokens = event.output_tokens;
        let (cost_usd, accounting) = self.usage_tracker.record_event(event);
        let labels = serde_json::json!({
            "agent_id": accounting.actor_id,
            "organization_id": accounting.workspace_id,
            "input_tokens": accounting.input_tokens,
            "output_tokens": accounting.output_tokens,
            "cached_input_tokens": accounting.cached_input_tokens,
            "cost_usd": cost_usd,
        });
        let _ = ::server_telemetry::buffer_metric(
            &self.pool,
            "ohc_token_usage_total",
            "counter",
            output_tokens.max(0) as f32,
            labels.clone(),
        )
        .await;
        let _ = ::server_telemetry::buffer_metric(
            &self.pool,
            "ohc_tenant_token_usage_total",
            "counter",
            total_tokens.max(0) as f32,
            labels.clone(),
        )
        .await;
        let cost_cents = (cost_usd * 100.0).round() as i64;
        let _ = ::server_telemetry::buffer_metric_i64(
            &self.pool,
            "ohc_llm_cost_total_cents",
            "counter",
            cost_cents,
            labels,
        )
        .await;
        self.billing_platform
            .record_platform_usage(accounting)
            .await;
        cost_usd
    }

    async fn invalidate_agent_cache(&self, tenant_id: Option<&str>) {
        *self.agent_cache.write().await = None;
        let keys_to_delete: Vec<String> = {
            let mut tenant_cache = self.tenant_agent_cache.write().await;
            match tenant_id.map(str::trim).filter(|t| !t.is_empty()) {
                Some(tid) => {
                    tenant_cache.remove(tid);
                    vec![tenant_agents_cache_key(tid)]
                }
                None => tenant_cache
                    .drain()
                    .map(|(tid, _)| tenant_agents_cache_key(&tid))
                    .collect(),
            }
        };
        if !keys_to_delete.is_empty()
            && let Some(cache) = &self.distributed_cache
        {
            for key in &keys_to_delete {
                cache.del_raw(key).await;
            }
        }
    }

    async fn invalidate_meetings_cache(&self, tenant_id: Option<&str>) {
        *self.meetings_cache.write().await = None;
        let keys_to_delete: Vec<String> = {
            let mut tenant_cache = self.tenant_meetings_cache.write().await;
            match tenant_id.map(str::trim).filter(|t| !t.is_empty()) {
                Some(tid) => {
                    tenant_cache.remove(tid);
                    vec![tenant_meetings_cache_key(tid)]
                }
                None => tenant_cache
                    .drain()
                    .map(|(tid, _)| tenant_meetings_cache_key(&tid))
                    .collect(),
            }
        };
        if !keys_to_delete.is_empty()
            && let Some(cache) = &self.distributed_cache
        {
            for key in &keys_to_delete {
                cache.del_raw(key).await;
            }
        }
    }

    fn push_bounded_inbox(
        inbox: &mut HashMap<String, VecDeque<Message>>,
        to_agent: &str,
        msg: Message,
    ) {
        if !inbox.contains_key(to_agent)
            && inbox.len() >= MAX_INBOX_AGENTS
            && let Some(evict_key) = inbox.keys().next().cloned()
        {
            inbox.remove(&evict_key);
        }
        let messages = inbox.entry(to_agent.to_owned()).or_default();
        while messages.len() >= MAX_INBOX_MESSAGES_PER_AGENT {
            messages.pop_front();
        }
        messages.push_back(msg);
    }

    pub async fn register_agent(&self, agent: Agent) {
        let org_id = agent.organization_id.clone();
        {
            let mut agents = self.agents.write().await;
            agents.insert(agent.id.clone(), agent);
        }
        self.invalidate_agent_cache(Some(&org_id)).await;
    }

    /// Retire only the still-existing registration owned by this tenant.
    /// Completion of old work must not resurrect an agent removed meanwhile.
    pub async fn update_agent_status(&self, id: &str, tenant_id: &str, status: &str) -> bool {
        {
            let mut agents = self.agents.write().await;
            let Some(agent) = agents
                .get_mut(id)
                .filter(|agent| agent.organization_id == tenant_id)
            else {
                return false;
            };
            agent.status = status.to_owned();
        }
        self.invalidate_agent_cache(Some(tenant_id)).await;
        true
    }

    pub async fn get_agent(&self, id: &str) -> Option<Agent> {
        let agents = self.agents.read().await;
        agents.get(id).cloned()
    }

    pub async fn get_agents_count(&self) -> usize {
        let agents = self.agents.read().await;
        agents.len()
    }

    pub async fn fire_agent(&self, id: &str) {
        // Acquire and drop each lock independently to prevent multi-lock cycles.
        let removed_org = {
            let mut agents = self.agents.write().await;
            agents.remove(id).map(|agent| agent.organization_id)
        };
        {
            let mut inbox = self.inbox.write().await;
            inbox.remove(id);
        }
        self.invalidate_agent_cache(removed_org.as_deref()).await;
    }

    pub async fn get_agents(&self) -> Arc<Vec<Agent>> {
        {
            let cache = self.agent_cache.read().await;
            if let Some(agents) = &*cache {
                return Arc::clone(agents);
            }
        }

        // Never cache all tenants' agents under a global Redis key (SEC-25);
        // keep the cross-tenant in-process snapshot strictly local.
        let mut agents_vec: Vec<Agent> = {
            let agents = self.agents.read().await;
            agents.values().cloned().collect()
        };
        agents_vec.sort_by(|a, b| a.id.cmp(&b.id));

        let arc = Arc::new(agents_vec);
        *self.agent_cache.write().await = Some(Arc::clone(&arc));
        arc
    }

    pub async fn get_agents_by_org(&self, org_id: &str) -> Vec<Agent> {
        let org_id = org_id.trim();
        if org_id.is_empty() {
            return Vec::new();
        }
        {
            let cache = self.tenant_agent_cache.read().await;
            if let Some(agents) = cache.get(org_id) {
                return agents.as_ref().clone();
            }
        }

        let cache_key = tenant_agents_cache_key(org_id);
        if let Some(cache) = &self.distributed_cache
            && let Some(data) = cache.get_raw(&cache_key).await
            && let Ok(agents) = serde_json::from_str::<Vec<Agent>>(&data)
        {
            let mut agents_vec: Vec<Agent> = agents
                .into_iter()
                .filter(|agent| agent.organization_id == org_id)
                .collect();
            agents_vec.sort_by(|a, b| a.id.cmp(&b.id));
            let arc = Arc::new(agents_vec.clone());
            self.tenant_agent_cache
                .write()
                .await
                .insert(org_id.to_string(), arc);
            return agents_vec;
        }

        let mut agents_vec: Vec<Agent> = {
            let agents = self.agents.read().await;
            agents
                .values()
                .filter(|a| a.organization_id == org_id)
                .cloned()
                .collect()
        };
        agents_vec.sort_by(|a, b| a.id.cmp(&b.id));

        let arc = Arc::new(agents_vec.clone());
        self.tenant_agent_cache
            .write()
            .await
            .insert(org_id.to_string(), Arc::clone(&arc));

        if let Some(cache) = &self.distributed_cache
            && let Ok(json) = serde_json::to_string(&agents_vec)
        {
            cache.set_raw(&cache_key, json, &[], 3600).await;
        }

        agents_vec
    }

    pub async fn meeting_belongs_to_other_org(&self, meeting_id: &str, org_id: &str) -> bool {
        let meeting_id = meeting_id.trim();
        let org_id = org_id.trim();
        if meeting_id.is_empty() || org_id.is_empty() {
            return false;
        }
        if let Some(owner) = self.meeting_orgs.read().await.get(meeting_id).cloned() {
            return owner != org_id;
        }
        let participants_opt = self
            .meetings
            .read()
            .await
            .get(meeting_id)
            .map(|meeting| meeting.participants.clone());
        let Some(participants) = participants_opt else {
            return false;
        };
        let agents = self.agents.read().await;
        participants.iter().any(|participant| {
            agents.get(participant).is_some_and(|agent| {
                !agent.organization_id.is_empty() && agent.organization_id != org_id
            })
        })
    }

    /// Atomically verify/reserve meeting ownership for `org_id` before opening or updating
    /// a meeting so one tenant can never overwrite or hijack another tenant's meeting ID.
    pub async fn try_open_meeting_for_org(
        &self,
        org_id: String,
        id: String,
        participants: Vec<String>,
        agenda: String,
    ) -> Result<MeetingRoom, String> {
        let org_trimmed = org_id.trim().to_string();
        let id_trimmed = id.trim().to_string();
        if id_trimmed.is_empty() {
            return Err("meeting_id is required".to_string());
        }
        if org_trimmed.is_empty() {
            return Err("organization_id is required".to_string());
        }
        {
            let mut meeting_orgs = self.meeting_orgs.write().await;
            if let Some(existing_owner) = meeting_orgs.get(&id_trimmed)
                && existing_owner != &org_trimmed
            {
                return Err("meeting belongs to another organization".to_string());
            }
            meeting_orgs.insert(id_trimmed.clone(), org_trimmed.clone());
        }
        Ok(self
            .open_meeting_for_org(org_trimmed, id_trimmed, participants, agenda)
            .await)
    }

    pub async fn open_meeting_for_org(
        &self,
        org_id: String,
        id: String,
        participants: Vec<String>,
        agenda: String,
    ) -> MeetingRoom {
        // Step 1: Update participant agent state under a short agents lock, then drop.
        let mut touched_orgs: HashSet<String> = HashSet::new();
        if !org_id.trim().is_empty() {
            touched_orgs.insert(org_id.trim().to_string());
        }
        {
            let mut agents = self.agents.write().await;
            for participant in &participants {
                if let Some(agent) = agents.get_mut(participant)
                    && (org_id.is_empty() || agent.organization_id == org_id)
                {
                    agent.status = "IN_MEETING".to_string();
                    if !agent.organization_id.is_empty() {
                        touched_orgs.insert(agent.organization_id.clone());
                    }
                }
            }
        }

        // Step 2: Insert meeting under a short meetings lock, then drop.
        let meeting = MeetingRoom {
            id: id.clone(),
            agenda,
            participants,
            transcript: vec![],
        };
        {
            let mut meetings = self.meetings.write().await;
            meetings.insert(id.clone(), meeting.clone());
        }

        // Step 3: Record explicit exact org ownership under its own short lock.
        let owner_org = if !org_id.trim().is_empty() {
            Some(org_id.trim().to_string())
        } else if touched_orgs.len() == 1 {
            touched_orgs.iter().next().cloned()
        } else {
            None
        };
        if let Some(ref owner) = owner_org {
            self.meeting_orgs.write().await.insert(id, owner.clone());
        }

        // Step 4: Invalidate caches with zero map locks held.
        if touched_orgs.is_empty() {
            self.invalidate_agent_cache(None).await;
        } else {
            for org in &touched_orgs {
                self.invalidate_agent_cache(Some(org)).await;
            }
        }
        self.invalidate_meetings_cache(owner_org.as_deref()).await;

        meeting
    }

    pub async fn open_meeting(
        &self,
        id: String,
        participants: Vec<String>,
        agenda: String,
    ) -> MeetingRoom {
        self.open_meeting_for_org(String::new(), id, participants, agenda)
            .await
    }

    pub async fn publish(self: std::sync::Arc<Self>, msg: Message) -> Result<(), String> {
        // Check rate limiting BEFORE acquiring locks, since the warning publish recurses into publish()
        if msg.from_agent != "system-scheduler" && msg.r#type != "warning" {
            let tenant_id = self
                .get_agent(&msg.to_agent)
                .await
                .map(|agent| agent.organization_id)
                .unwrap_or_else(|| "default".to_owned());
            let agent_id = msg.to_agent.clone();
            let meeting_id = msg.meeting_id.clone();
            let billing = self.billing_platform.clone();
            let hub_clone = self.clone();
            tokio::spawn(async move {
                let scope = WorkspaceScope {
                    workspace_id: tenant_id,
                    actor_id: agent_id.clone(),
                    roles: vec![],
                };
                if let Ok(limit_status) =
                    billing.check_platform_quota(&scope, "agents.message").await
                    && limit_status.soft_limit_reached
                {
                    tracing::warn!("Quota warning: {:?}", limit_status.warning_message);
                    if let Some(user_msg) = limit_status.warning_message {
                        let warning_msg = Message {
                            id: format!("warning-{}", chrono::Utc::now().timestamp()),
                            from_agent: "system-scheduler".to_string(),
                            to_agent: agent_id.clone(),
                            r#type: "warning".to_string(),
                            content: user_msg,
                            occurred_at_unix: chrono::Utc::now().timestamp(),
                            meeting_id,
                        };
                        let to = warning_msg.to_agent.clone();
                        {
                            let mut inbox = hub_clone.inbox.write().await;
                            Self::push_bounded_inbox(&mut inbox, &to, warning_msg.clone());
                        }
                        let tx_opt = hub_clone.subs.read().await.get(&to).cloned();
                        if let Some(tx) = tx_opt {
                            let _ = tx.send(warning_msg);
                        }
                    }
                }
            });
        }

        let to_agent = msg.to_agent.clone();

        // Step 1: Add to recipient's bounded inbox, then drop inbox lock immediately.
        {
            let mut inbox = self.inbox.write().await;
            Self::push_bounded_inbox(&mut inbox, &to_agent, msg.clone());
        }

        // Step 2: Append to meeting transcript under a short meetings lock, then drop
        // BEFORE invalidating Redis caches or reading subscribers.
        let mut meeting_updated = false;
        let mut summarization_transcript: Option<Vec<Message>> = None;
        if !msg.meeting_id.is_empty() {
            let mut meetings = self.meetings.write().await;
            if let Some(meeting) = meetings.get_mut(&msg.meeting_id) {
                meeting.transcript.push(msg.clone());
                meeting_updated = true;
                if meeting.transcript.len() > 10 && !self.minimax_api_key.is_empty() {
                    summarization_transcript = Some(meeting.transcript.clone());
                }
            }
        }

        if meeting_updated {
            let meeting_org = self.meeting_orgs.read().await.get(&msg.meeting_id).cloned();
            self.invalidate_meetings_cache(meeting_org.as_deref()).await;

            if let Some(transcript) = summarization_transcript {
                let api_key = self.minimax_api_key.clone();
                let m_id = msg.meeting_id.clone();
                let hub = self.clone();

                tokio::spawn(async move {
                    let client = crate::minimax::MinimaxClient::new(api_key);
                    let mut prompt = "Extract and summarize ONLY the exact parameters, architectural decisions, and required next steps from this transcript. Discard all conversational filler, pleasantries, and non-actionable text. Output MUST be an ultra-dense, bulleted technical brief optimized for minimal token footprint:\n".to_string();

                    for m in &transcript {
                        prompt.push_str(&format!("{}: {}\n", m.from_agent, m.content));
                    }

                    match client.reason(&prompt).await {
                        Ok(summary) => {
                            let updated = {
                                let mut meetings = hub.meetings.write().await;
                                if let Some(mtg) = meetings.get_mut(&m_id) {
                                    let mut new_transcript = vec![Message {
                                        id: format!("summary-{}", Utc::now().timestamp()),
                                        from_agent: "SYSTEM_SUMMARIZER".to_string(),
                                        to_agent: "all".to_string(),
                                        r#type: "status".to_string(),
                                        content: format!("[CONTEXT SUMMARIZED]: {}", summary),
                                        meeting_id: m_id.clone(),
                                        occurred_at_unix: Utc::now().timestamp(),
                                    }];

                                    if mtg.transcript.len() > 3 {
                                        new_transcript.extend(
                                            mtg.transcript
                                                .iter()
                                                .skip(mtg.transcript.len() - 3)
                                                .cloned(),
                                        );
                                    } else {
                                        new_transcript.extend(mtg.transcript.iter().cloned());
                                    }
                                    mtg.transcript = new_transcript;
                                    true
                                } else {
                                    false
                                }
                            };
                            if updated {
                                let org = hub.meeting_orgs.read().await.get(&m_id).cloned();
                                hub.invalidate_meetings_cache(org.as_deref()).await;
                            }
                        }
                        Err(e) => tracing::error!("Summarization failed: {}", e),
                    }
                });
            }
        }

        // Step 3: Notify subscribers with a short subs lock cloned and dropped before send.
        let tx_opt = self.subs.read().await.get(&to_agent).cloned();
        if let Some(tx) = tx_opt {
            let _ = tx.send(msg);
        }

        Ok(())
    }

    pub async fn get_meetings_by_org(&self, org_id: &str) -> Arc<Vec<MeetingRoom>> {
        let org_id = org_id.trim();
        if org_id.is_empty() {
            return Arc::new(Vec::new());
        }
        {
            let cache = self.tenant_meetings_cache.read().await;
            if let Some(meetings) = cache.get(org_id) {
                return Arc::clone(meetings);
            }
        }

        let cache_key = tenant_meetings_cache_key(org_id);
        if let Some(cache) = &self.distributed_cache
            && let Some(data) = cache.get_raw(&cache_key).await
            && let Ok(meetings) = serde_json::from_str::<Vec<MeetingRoom>>(&data)
        {
            let arc = Arc::new(meetings);
            self.tenant_meetings_cache
                .write()
                .await
                .insert(org_id.to_string(), Arc::clone(&arc));
            return arc;
        }

        // Acquire each snapshot independently (never holding two locks at once).
        let meeting_orgs_snapshot = self.meeting_orgs.read().await.clone();
        let agent_orgs_snapshot: HashMap<String, String> = {
            let agents = self.agents.read().await;
            agents
                .iter()
                .map(|(id, agent)| (id.clone(), agent.organization_id.clone()))
                .collect()
        };
        let all_meetings = self.get_meetings().await;

        let mut filtered = Vec::new();
        for m in all_meetings.iter() {
            let belongs = if let Some(owner) = meeting_orgs_snapshot.get(&m.id) {
                owner == org_id
            } else {
                let mut matched_own = false;
                let mut has_foreign = false;
                for participant in &m.participants {
                    if let Some(participant_org) = agent_orgs_snapshot.get(participant) {
                        if participant_org == org_id {
                            matched_own = true;
                        } else if !participant_org.is_empty() {
                            has_foreign = true;
                        }
                    }
                }
                matched_own && !has_foreign
            };
            if belongs {
                filtered.push(m.clone());
            }
        }

        let arc = Arc::new(filtered);
        self.tenant_meetings_cache
            .write()
            .await
            .insert(org_id.to_string(), Arc::clone(&arc));

        if let Some(cache) = &self.distributed_cache
            && let Ok(json) = serde_json::to_string(&*arc)
        {
            cache.set_raw(&cache_key, json, &[], 5).await;
        }

        arc
    }

    pub async fn get_meetings(&self) -> Arc<Vec<MeetingRoom>> {
        {
            let cache = self.meetings_cache.read().await;
            if let Some(meetings) = &*cache {
                return Arc::clone(meetings);
            }
        }

        // Never cache all tenants' meetings under a global Redis key; keep local only.
        let meetings_vec: Vec<MeetingRoom> = {
            let meetings = self.meetings.read().await;
            meetings.values().cloned().collect()
        };

        let arc = Arc::new(meetings_vec);
        *self.meetings_cache.write().await = Some(Arc::clone(&arc));
        arc
    }

    pub async fn record_agent_execution(
        &self,
        id: &str,
        duration: std::time::Duration,
        failed: bool,
    ) {
        self.execution_measurements
            .write()
            .await
            .entry(id.to_owned())
            .or_default()
            .record(duration, failed);
    }

    pub async fn agent_measurements(
        &self,
        id: &str,
    ) -> (crate::api::agent_metrics::ExecutionMeasurements, usize) {
        let sample = self
            .execution_measurements
            .read()
            .await
            .get(id)
            .cloned()
            .unwrap_or_default();
        let connections = self
            .subs
            .read()
            .await
            .get(id)
            .map_or(0, broadcast::Sender::receiver_count);
        (sample, connections)
    }

    pub async fn remove_transient_agent(&self, id: &str) {
        let removed_org = self
            .agents
            .write()
            .await
            .remove(id)
            .map(|agent| agent.organization_id);
        self.inbox.write().await.remove(id);
        self.subs.write().await.remove(id);
        self.execution_measurements.write().await.remove(id);
        self.invalidate_agent_cache(removed_org.as_deref()).await;
    }

    pub async fn get_inbox(&self, agent_id: &str) -> Vec<Message> {
        let mut inbox = self.inbox.write().await;
        inbox.remove(agent_id).map(Vec::from).unwrap_or_default()
    }

    pub async fn subscribe(&self, agent_id: String) -> broadcast::Receiver<Message> {
        let mut subs = self.subs.write().await;
        let tx = subs.entry(agent_id).or_insert_with(|| {
            let (tx, _) = broadcast::channel(100); // Buffer of 100
            tx
        });
        tx.subscribe()
    }

    pub async fn delegate_task(
        self: std::sync::Arc<Self>,
        from_agent_id: String,
        to_agent_id: String,
        mut task: Message,
    ) -> Result<(), String> {
        check_documentation_gate(&task.content)?;

        {
            let agents = self.agents.read().await;
            let sender = agents
                .get(&from_agent_id)
                .ok_or("sender agent is not registered")?;
            let recipient = agents
                .get(&to_agent_id)
                .ok_or("recipient agent is not registered")?;
            if sender.organization_id.is_empty()
                || sender.organization_id != recipient.organization_id
            {
                return Err("delegation crosses organization boundary".into());
            }
        }

        task.from_agent = from_agent_id;
        task.to_agent = to_agent_id;

        self.publish(task).await
    }

    pub async fn delegate_sub_task(
        self: std::sync::Arc<Self>,
        from_agent_id: &str,
        target_role: &str,
        instruction: &str,
        parent_thread_id: &str,
    ) -> Result<String, String> {
        check_documentation_gate(instruction)?;

        let target_id = {
            let agents = self.agents.read().await;
            let sender = agents
                .get(from_agent_id)
                .ok_or("sender agent is not registered")?;
            if sender.organization_id.is_empty() {
                return Err("sender organization is missing".into());
            }
            agents
                .values()
                .filter(|agent| {
                    agent.id != from_agent_id
                        && agent.organization_id == sender.organization_id
                        && agent.role == target_role
                })
                .map(|agent| agent.id.clone())
                .min()
                .ok_or("no registered agent is available for the requested role")?
        };
        let task = Message {
            id: uuid::Uuid::new_v4().to_string(),
            from_agent: from_agent_id.to_owned(),
            to_agent: target_id.clone(),
            r#type: "TaskDelegation".into(),
            content: serde_json::json!({"task": instruction, "parent_thread_id": parent_thread_id})
                .to_string(),
            occurred_at_unix: Utc::now().timestamp(),
            meeting_id: parent_thread_id.to_owned(),
        };
        self.delegate_task(from_agent_id.to_owned(), target_id.clone(), task)
            .await?;
        Ok(target_id)
    }

    pub fn minimax_api_key(&self) -> &str {
        &self.minimax_api_key
    }

    pub fn advertise_capabilities(&self, caps: AgentCapabilities) -> Result<(), String> {
        let _ = self.caps_tx.send(caps);
        Ok(())
    }

    pub fn subscribe_capabilities(&self) -> broadcast::Receiver<AgentCapabilities> {
        self.caps_tx.subscribe()
    }

    pub async fn publish_mesh_event(&self, event: MeshEvent) -> Result<(), String> {
        let mut mesh_events = self.mesh_events.write().await;
        let tx = mesh_events.entry(event.topic.clone()).or_insert_with(|| {
            let (tx, _) = broadcast::channel(100);
            tx
        });
        let _ = tx.send(event);
        Ok(())
    }

    pub async fn subscribe_mesh_events(&self, topic: String) -> broadcast::Receiver<MeshEvent> {
        let mut mesh_events = self.mesh_events.write().await;
        let tx = mesh_events.entry(topic).or_insert_with(|| {
            let (tx, _) = broadcast::channel(100);
            tx
        });
        tx.subscribe()
    }

    pub async fn publish_teammate_event(
        &self,
        channel: String,
        event: TeammateMeshEvent,
    ) -> Result<(), String> {
        let mut teammate_events = self.teammate_events.write().await;
        let tx = teammate_events.entry(channel).or_insert_with(|| {
            let (tx, _) = broadcast::channel(100);
            tx
        });
        let _ = tx.send(event);
        Ok(())
    }

    pub async fn subscribe_teammate_mesh(
        &self,
        channel: String,
    ) -> broadcast::Receiver<TeammateMeshEvent> {
        let mut teammate_events = self.teammate_events.write().await;
        let tx = teammate_events.entry(channel).or_insert_with(|| {
            let (tx, _) = broadcast::channel(100);
            tx
        });
        tx.subscribe()
    }

    pub fn task_manager(&self) -> &TaskManager {
        &self.task_manager
    }

    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    pub fn log_event(&self, event: serde_json::Value) {
        let _ = self.event_log_tx.try_send(event);
    }

    pub async fn append_recent_event(&self, event: HubEvent) {
        let mut recent = self.recent_events.write().await;
        while recent.len() >= MAX_RECENT_EVENTS {
            recent.pop_front();
        }
        recent.push_back(event);
    }

    pub async fn recent_events(&self, limit: usize) -> Vec<HubEvent> {
        let recent = self.recent_events.read().await;
        let count = recent.len().min(limit);
        recent.iter().rev().take(count).cloned().collect()
    }

    pub fn sanitize_hub_event(&self, raw: serde_json::Value) -> HubEvent {
        let event_type = raw["type"].as_str().unwrap_or("unknown").to_string();
        let redacted_raw = ::server_telemetry::redact_interface_pii(raw);
        HubEvent {
            r#type: event_type,
            payload: redacted_raw.to_string(),
            occurred_at: Utc::now(),
        }
    }

    pub fn tool_parameter_auto_correction(
        &self,
        event_id: &str,
        agent_id: &str,
        payload: &[u8],
    ) -> Result<(), String> {
        {
            let mut auto_cor_track = self
                .auto_cor_track
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if auto_cor_track.contains(event_id) {
                return Err("event already being processed".to_string());
            }
            if auto_cor_track.len() >= MAX_AUTO_COR_INFLIGHT {
                return Err("auto-correction queue at capacity".to_string());
            }
            auto_cor_track.insert(event_id.to_string());
        }

        struct Guard<'a>(&'a std::sync::Mutex<HashSet<String>>, &'a str);
        impl<'a> Drop for Guard<'a> {
            fn drop(&mut self) {
                let mut track = self.0.lock().unwrap_or_else(|error| error.into_inner());
                track.remove(self.1);
            }
        }
        let _guard = Guard(&self.auto_cor_track, event_id);

        let mut temp: HashMap<String, serde_json::Value> =
            serde_json::from_slice(payload).map_err(|e| e.to_string())?;

        let mut corrected = false;
        for (_k, v) in temp.iter_mut() {
            if let Some(s) = v.as_str()
                && let Ok(n) = s.parse::<i64>()
                && n.to_string() == s
            {
                *v = serde_json::Value::Number(n.into());
                corrected = true;
            }
        }

        self.log_event(serde_json::json!({
            "event_id": event_id,
            "agent_id": agent_id,
            "type": "ToolParameterAutoCorrection",
            "payload": temp,
            "corrected": corrected,
        }));

        Ok(())
    }

    pub async fn fork_agent(
        self: std::sync::Arc<Self>,
        parent_id: &str,
        directive: &str,
    ) -> Result<String, String> {
        let mut agents = self.agents.write().await;

        let parent = agents
            .get(parent_id)
            .ok_or_else(|| format!("parent agent not found: {}", parent_id))?
            .clone();

        let child_id = format!("{}-fork-{}", parent_id, uuid::Uuid::new_v4());
        let child = Agent {
            id: child_id.clone(),
            name: format!("{} (Fork)", parent.name),
            role: parent.role.clone(),
            organization_id: parent.organization_id.clone(),
            status: "IDLE".to_string(),
            provider_type: parent.provider_type.clone(),
        };

        let org_id = parent.organization_id.clone();
        agents.insert(child_id.clone(), child);
        drop(agents);
        self.invalidate_agent_cache(Some(&org_id)).await;

        // Copy history
        let history = {
            let inbox = self.inbox.read().await;
            inbox.get(parent_id).cloned().unwrap_or_default()
        };

        for msg in history {
            let mut child_msg = msg.clone();
            child_msg.id = format!("msg-{}", uuid::Uuid::new_v4());
            child_msg.to_agent = child_id.clone();
            self.clone().publish(child_msg).await?;
        }

        // Send directive
        let directive_msg = Message {
            id: format!("msg-{}", uuid::Uuid::new_v4()),
            from_agent: "SYSTEM".to_string(),
            to_agent: child_id.clone(),
            r#type: "TaskAssignment".to_string(),
            content: format!(
                "<task-notification>\nDirective: {}\n</task-notification>",
                directive
            ),
            occurred_at_unix: Utc::now().timestamp(),
            meeting_id: String::new(),
        };

        self.clone().publish(directive_msg).await?;

        Ok(child_id)
    }

    pub async fn check_health(&self) -> Result<serde_json::Value, String> {
        let start = std::time::Instant::now();

        let configured_db = self
            .task_manager
            .db
            .read()
            .ok()
            .and_then(|guard| guard.clone());
        let sqlite_pool = match configured_db.as_ref().map(|db| &db.store) {
            Some(crate::db::DbStore::Sqlite(pool)) => Some(pool.clone()),
            Some(crate::db::DbStore::Postgres) => None,
            None => crate::db::get_sqlite_pool_if_exists(),
        };
        let pg_pool = configured_db
            .as_ref()
            .map(|db| db.pool.clone())
            .unwrap_or_else(|| self.pool.clone());

        let (db_ping_opt, local_to_cloud_sync_queue, sync_error_count, stuck_missions) =
            if let Some(sqlite) = sqlite_pool {
                let db_ping_opt = match sqlx::query("SELECT 1").execute(&sqlite).await {
                    Ok(_) => Some(start.elapsed().as_millis() as u64),
                    Err(_) => None,
                };
                let has_agent_missions: bool = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'agent_missions'",
                )
                .fetch_one(&sqlite)
                .await
                .unwrap_or(0)
                    > 0;
                let has_sync_columns: bool = if has_agent_missions {
                    sqlx::query_scalar::<_, i64>(
                        "SELECT COUNT(*) FROM pragma_table_info('agent_missions') \
                         WHERE name IN ('synced_to_cloud', 'sync_error', 'last_synced_at')",
                    )
                    .fetch_one(&sqlite)
                    .await
                    .unwrap_or(0)
                        == 3
                } else {
                    false
                };
                let (sync_queue, sync_errors) = if has_sync_columns {
                    let queue = sqlx::query_scalar::<_, i64>(
                        "SELECT count(*) FROM agent_missions \
                         WHERE synced_to_cloud = 0 \
                           AND (status = 'CLOUD_ESCALATION' OR status = 'BURSTING') \
                           AND (sync_error IS NULL OR last_synced_at < datetime('now', '-5 minutes'))",
                    )
                    .fetch_one(&sqlite)
                    .await
                    .unwrap_or(0);
                    let errors = sqlx::query_scalar::<_, i64>(
                        "SELECT count(*) FROM agent_missions \
                         WHERE sync_error IS NOT NULL AND synced_to_cloud = 0",
                    )
                    .fetch_one(&sqlite)
                    .await
                    .unwrap_or(0);
                    (queue, errors)
                } else {
                    (0, 0)
                };
                let stuck = if has_agent_missions {
                    sqlx::query_scalar::<_, i64>(
                        "SELECT count(*) FROM agent_missions WHERE status = 'STUCK'",
                    )
                    .fetch_one(&sqlite)
                    .await
                    .unwrap_or(0)
                } else {
                    0
                };
                (db_ping_opt, sync_queue, sync_errors, stuck)
            } else {
                let ping_pool = pg_pool.clone();
                let ping_future = tokio::task::spawn(async move {
                    match sqlx::query("SELECT 1").execute(&ping_pool).await {
                        Ok(_) => Some(start.elapsed().as_millis() as u64),
                        Err(_) => None,
                    }
                });

                let schema_pool = pg_pool.clone();
                let missions_future = tokio::task::spawn(async move {
                    let has_agent_missions: bool =
                        sqlx::query_scalar("SELECT to_regclass('agent_missions') IS NOT NULL")
                            .fetch_one(&schema_pool)
                            .await
                            .unwrap_or(false);
                    if !has_agent_missions {
                        return (0_i64, 0_i64, 0_i64);
                    }

                    let has_sync_columns: bool = sqlx::query_scalar(
                        "SELECT COUNT(*) = 3 \
                         FROM pg_attribute \
                         WHERE attrelid = to_regclass('agent_missions') \
                           AND attname IN ('synced_to_cloud', 'sync_error', 'last_synced_at') \
                           AND NOT attisdropped",
                    )
                    .fetch_one(&schema_pool)
                    .await
                    .unwrap_or(false);

                    let stuck_pool = schema_pool.clone();
                    let stuck_future = tokio::task::spawn(async move {
                        sqlx::query_scalar::<_, i64>(
                            "SELECT count(*) FROM agent_missions WHERE status = 'STUCK'",
                        )
                        .fetch_one(&stuck_pool)
                        .await
                        .unwrap_or(0)
                    });

                    if !has_sync_columns {
                        let stuck = stuck_future.await.unwrap_or(0);
                        return (0_i64, 0_i64, stuck);
                    }

                    let queue_pool = schema_pool.clone();
                    let sync_queue_future = tokio::task::spawn(async move {
                        sqlx::query_scalar::<_, i64>(
                            "SELECT count(*) FROM agent_missions \
                             WHERE synced_to_cloud = $1 \
                               AND (status = 'CLOUD_ESCALATION' OR status = 'BURSTING') \
                               AND (sync_error IS NULL OR last_synced_at < NOW() - INTERVAL '5 minutes')",
                        )
                        .bind(false)
                        .fetch_one(&queue_pool)
                        .await
                        .unwrap_or(0)
                    });

                    let errors_pool = schema_pool.clone();
                    let sync_errors_future = tokio::task::spawn(async move {
                        sqlx::query_scalar::<_, i64>(
                            "SELECT count(*) FROM agent_missions \
                             WHERE sync_error IS NOT NULL AND synced_to_cloud = $1",
                        )
                        .bind(false)
                        .fetch_one(&errors_pool)
                        .await
                        .unwrap_or(0)
                    });

                    let (queue_res, errors_res, stuck_res) =
                        tokio::join!(sync_queue_future, sync_errors_future, stuck_future);
                    (
                        queue_res.unwrap_or(0),
                        errors_res.unwrap_or(0),
                        stuck_res.unwrap_or(0),
                    )
                });

                let (ping_res, missions_res) = tokio::join!(ping_future, missions_future);
                let db_ping_opt = ping_res.ok().flatten();
                let (sync_queue, sync_errors, stuck) = missions_res.unwrap_or((0, 0, 0));
                (db_ping_opt, sync_queue, sync_errors, stuck)
            };

        let db_ok = db_ping_opt.is_some();
        let db_ping = db_ping_opt.unwrap_or(0);

        let mode = if crate::is_standalone_runtime() {
            "standalone"
        } else {
            "cloud"
        };

        let status = if db_ok { "healthy" } else { "degraded" };
        let mesh_active = db_ok;
        let cloud_connected = mode != "standalone";

        let configured_db_url = std::env::var("OMNISOLO_DATABASE_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                std::env::var("DATABASE_URL")
                    .ok()
                    .filter(|v| !v.trim().is_empty())
            });

        let hybrid_mode_ready = if mode == "standalone" {
            configured_db_url.is_some() && db_ok
        } else {
            db_ok
        };

        let mut checklist = Vec::new();
        if configured_db_url
            .as_deref()
            .is_some_and(|url| url.starts_with("postgres"))
        {
            checklist.push("PostgreSQL Connected");
        }
        if std::env::var("REDIS_URL").is_ok() {
            checklist.push("Redis Available");
        }
        if mode == "standalone"
            && configured_db_url
                .as_deref()
                .is_none_or(|url| url.starts_with("sqlite"))
        {
            checklist.push("SQLite Standalone Enabled");
        }

        Ok(serde_json::json!({
            "mode": mode,
            "status": status,
            "db_ping_ms": db_ping,
            "mesh_active": mesh_active,
            "cloud_connected": cloud_connected,
            "hybrid_mode_ready": hybrid_mode_ready,
            "local_to_cloud_sync_queue": local_to_cloud_sync_queue,
            "sync_error_count": sync_error_count,
            "checklist": checklist,
            "stuck_missions": stuck_missions,
        }))
    }
}

fn get_feature_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"\[Feature:\s*([^\]]+)\]").unwrap())
}

pub fn check_documentation_gate(content: &str) -> Result<(), String> {
    let regex = get_feature_regex();

    if let Some(caps) = regex.captures(content) {
        let feature_name: &str = caps.get(1).unwrap().as_str();
        let base_dir = format!("docs/features/{}", feature_name);

        let required_files = ["design-doc.md", "cuj.md", "test-plan.md"];
        for file in &required_files {
            let path = std::path::Path::new(&base_dir).join(file);
            if !path.exists() {
                return Err(format!(
                    "missing required documentation: {}",
                    path.display()
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_publish_mesh_event() {
        if std::env::var("OMNISOLO_DATABASE_URL").is_err() {
            return;
        }
        let db_url = std::env::var("OMNISOLO_DATABASE_URL").unwrap();
        let pool = crate::db::secure_pg_pool_options()
            .connect_lazy(&db_url)
            .unwrap();
        let (tx, _) = mpsc::channel(100);
        let hub = std::sync::Arc::new(Hub::new(tx, pool));

        let mut rx = hub.subscribe_mesh_events("test_topic".to_string()).await;

        let event = MeshEvent {
            event_id: "test_id".to_string(),
            topic: "test_topic".to_string(),
            payload: b"test_payload".to_vec(),
            timestamp: 0,
        };

        hub.publish_mesh_event(event.clone()).await.unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event_id, event.event_id);
        assert_eq!(received.topic, event.topic);
        assert_eq!(received.payload, event.payload);
    }

    #[tokio::test]
    async fn test_sanitize_hub_event_redaction() {
        if std::env::var("OMNISOLO_DATABASE_URL").is_err() {
            return;
        }
        let db_url = std::env::var("OMNISOLO_DATABASE_URL").unwrap();
        let pool = crate::db::secure_pg_pool_options()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy(&db_url)
            .unwrap();
        let (tx, _) = mpsc::channel(100);
        let hub = std::sync::Arc::new(Hub::new(tx, pool));

        let raw = serde_json::json!({
            "type": "TestEvent",
            "password": "secret-password",
            "email": "test@example.com",
            "S S N": "123-45-6789",
            "tax_ID_number": "123456789",
            "mixed_array": [
                { "nested_ip_address": "127.0.0.1" },
                { "S_S_N": "000-00-0000" }
            ],
            "nested": {
                "auth_token": "token123",
                "nested_again": {
                    "credit_card": "1234"
                }
            }
        });

        let sanitized = hub.sanitize_hub_event(raw);
        let payload: serde_json::Value = serde_json::from_str(&sanitized.payload).unwrap();

        assert_eq!(payload["password"], "[REDACTED]");
        assert_eq!(payload["email"], "[REDACTED]");
        assert_eq!(payload["S S N"], "[REDACTED]");
        assert_eq!(payload["tax_ID_number"], "[REDACTED]");
        assert_eq!(payload["mixed_array"][0]["nested_ip_address"], "[REDACTED]");
        assert_eq!(payload["mixed_array"][1]["S_S_N"], "[REDACTED]");
        assert_eq!(payload["nested"]["auth_token"], "[REDACTED]");
        assert_eq!(
            payload["nested"]["nested_again"]["credit_card"],
            "[REDACTED]"
        );
    }

    #[tokio::test]
    async fn test_cache_invalidation() {
        if std::env::var("OMNISOLO_DATABASE_URL").is_err() {
            return;
        }
        let db_url = std::env::var("OMNISOLO_DATABASE_URL").unwrap();
        let pool = crate::db::secure_pg_pool_options()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy(&db_url)
            .unwrap();
        let (tx, _) = mpsc::channel(100);
        let hub = std::sync::Arc::new(Hub::new(tx, pool));

        // 1. Initial get caches empty state
        let agents = hub.get_agents().await;
        assert_eq!(agents.len(), 0);

        // Cache should be populated
        assert!(hub.agent_cache.read().await.is_some());

        // 2. Registering an agent invalidates the cache
        hub.register_agent(Agent {
            id: "agent1".to_string(),
            name: "Agent 1".to_string(),
            role: "test".to_string(),
            organization_id: "org1".to_string(),
            status: "IDLE".to_string(),
            provider_type: "test".to_string(),
        })
        .await;
        assert!(hub.agent_cache.read().await.is_none());

        // 3. Get agents caches again
        let agents = hub.get_agents().await;
        assert_eq!(agents.len(), 1);
        assert!(hub.agent_cache.read().await.is_some());

        // 4. Fire agent invalidates
        hub.fire_agent("agent1").await;
        assert!(hub.agent_cache.read().await.is_none());

        // 5. Open meeting invalidates both caches
        let meetings = hub.get_meetings();
        assert_eq!(meetings.await.len(), 0);
        assert!(hub.meetings_cache.read().await.is_some());

        hub.open_meeting("meeting1".to_string(), vec![], "agenda".to_string())
            .await;
        assert!(hub.meetings_cache.read().await.is_none());
        assert!(hub.agent_cache.read().await.is_none());

        // 6. Publish invalidates meeting cache
        let meetings = hub.get_meetings();
        assert_eq!(meetings.await.len(), 1);
        assert!(hub.meetings_cache.read().await.is_some());

        let _ = hub
            .clone()
            .publish(Message {
                id: "msg1".to_string(),
                from_agent: "sys".to_string(),
                to_agent: "all".to_string(),
                r#type: "test".to_string(),
                content: "test".to_string(),
                occurred_at_unix: 0,
                meeting_id: "meeting1".to_string(),
            })
            .await;
        assert!(hub.meetings_cache.read().await.is_none());
    }
    #[tokio::test]
    async fn test_delegate_sub_task_invalid_sender() {
        if std::env::var("OMNISOLO_DATABASE_URL").is_err() {
            return;
        }

        let db_url = std::env::var("OMNISOLO_DATABASE_URL").unwrap();
        let pool = crate::db::secure_pg_pool_options()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy(&db_url)
            .unwrap();
        let (tx, _) = mpsc::channel(100);
        let hub = std::sync::Arc::new(Hub::new(tx, pool));

        let res = hub
            .delegate_sub_task(
                "non_existent_agent",
                "developer",
                "fix the bug",
                "thread123",
            )
            .await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err(), "sender agent is not registered");
    }

    #[tokio::test]
    async fn test_delegate_sub_task_valid_hierarchy() {
        let db_url = "postgres://localhost/test";
        let pool = crate::db::secure_pg_pool_options()
            .after_release(|conn, _meta| {
                Box::pin(async move {
                    use sqlx::Executor;
                    conn.execute("DISCARD ALL").await?;
                    Ok(true)
                })
            })
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy(db_url)
            .unwrap();
        let (tx, _) = mpsc::channel(100);
        let hub = std::sync::Arc::new(Hub::new(tx, pool));

        hub.register_agent(Agent {
            id: "manager_agent".to_string(),
            name: "Manager".to_string(),
            role: "Manager".to_string(),
            organization_id: "org1".to_string(),
            status: "IDLE".to_string(),
            provider_type: "builtin".to_string(),
        })
        .await;

        hub.register_agent(Agent {
            id: "developer-worker".into(),
            role: "developer".into(),
            organization_id: "org1".into(),
            ..Default::default()
        })
        .await;
        let res = hub
            .clone()
            .delegate_sub_task("manager_agent", "developer", "fix the bug", "thread123")
            .await;

        assert!(res.is_ok());
        let spawned_id = res.unwrap();
        assert_eq!(spawned_id, "developer-worker");
        assert_eq!(
            hub.get_inbox("developer-worker").await[0].r#type,
            "TaskDelegation"
        );
    }

    #[tokio::test]
    async fn test_fork_agent() {
        if std::env::var("OMNISOLO_DATABASE_URL").is_err() {
            return;
        }

        let db_url = std::env::var("OMNISOLO_DATABASE_URL").unwrap();
        let pool = crate::db::secure_pg_pool_options()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy(&db_url)
            .unwrap();

        let (tx, _) = mpsc::channel(100);
        let hub = std::sync::Arc::new(Hub::new(tx, pool));

        let parent_id = "parent_agent_id".to_string();

        hub.register_agent(Agent {
            id: parent_id.clone(),
            name: "Parent Agent".to_string(),
            role: "Developer".to_string(),
            organization_id: "org1".to_string(),
            status: "IDLE".to_string(),
            provider_type: "builtin".to_string(),
        })
        .await;

        let msg1 = Message {
            id: "msg1".to_string(),
            from_agent: "user".to_string(),
            to_agent: parent_id.clone(),
            r#type: "text".to_string(),
            content: "Hello parent".to_string(),
            occurred_at_unix: 0,
            meeting_id: "".to_string(),
        };

        let msg2 = Message {
            id: "msg2".to_string(),
            from_agent: "user".to_string(),
            to_agent: parent_id.clone(),
            r#type: "text".to_string(),
            content: "How are you".to_string(),
            occurred_at_unix: 1,
            meeting_id: "".to_string(),
        };

        hub.clone().publish(msg1.clone()).await.unwrap();
        hub.clone().publish(msg2.clone()).await.unwrap();

        let directive = "Do something specific in the fork";
        let child_id_res = hub.clone().fork_agent(&parent_id, directive).await;

        assert!(child_id_res.is_ok());
        let child_id = child_id_res.unwrap();

        assert!(child_id.starts_with("parent_agent_id-fork-"));

        let child_agent_opt = hub.get_agent(&child_id).await;
        assert!(child_agent_opt.is_some());
        let child_agent = child_agent_opt.unwrap();
        assert_eq!(child_agent.name, "Parent Agent (Fork)");
        assert_eq!(child_agent.role, "Developer");

        let child_inbox = hub.get_inbox(&child_id).await;

        // Should have 3 messages: msg1, msg2, and the directive
        assert_eq!(child_inbox.len(), 3);

        assert_eq!(child_inbox[0].content, "Hello parent");
        assert_eq!(child_inbox[0].to_agent, child_id);
        assert_ne!(child_inbox[0].id, "msg1"); // ID should be new

        assert_eq!(child_inbox[1].content, "How are you");
        assert_eq!(child_inbox[1].to_agent, child_id);
        assert_ne!(child_inbox[1].id, "msg2"); // ID should be new

        assert_eq!(child_inbox[2].from_agent, "SYSTEM");
        assert_eq!(child_inbox[2].to_agent, child_id);
        assert_eq!(child_inbox[2].r#type, "TaskAssignment");
        assert!(child_inbox[2].content.contains(directive));
        assert!(child_inbox[2].content.contains("<task-notification>"));
    }

    #[tokio::test]
    async fn test_check_health() {
        // Skip test if no database is available
        if std::env::var("OMNISOLO_DATABASE_URL").is_err() {
            return;
        }

        let db_url = std::env::var("OMNISOLO_DATABASE_URL").unwrap();
        // Since test db is likely unmigrated/empty, we connect lazily
        let pool = crate::db::secure_pg_pool_options()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy(&db_url)
            .unwrap();
        let (tx, _) = mpsc::channel(100);
        let hub = std::sync::Arc::new(Hub::new(tx, pool));

        let health = hub.check_health().await.unwrap();

        // When lazily connected, if DB doesn't exist, status might be degraded,
        // or we might get an error depending on how check_health handles failure.
        // In our check_health, failure to query SELECT 1 results in db_ping = 0.
        // We just ensure the response contains the fields we expect.
        assert!(health.get("status").is_some());
        assert!(health.get("db_ping_ms").is_some());
        assert!(health.get("hybrid_mode_ready").is_some());
        assert!(health.get("local_to_cloud_sync_queue").is_some());
    }

    #[tokio::test]
    async fn test_concurrent_publish_does_not_block_runtime() {
        let pool = crate::db::secure_pg_pool_options()
            .connect_lazy("postgres://localhost/test")
            .unwrap();
        let (tx, _) = mpsc::channel(100);
        let hub = std::sync::Arc::new(Hub::new(tx, pool));

        let mut rx1 = hub.subscribe("agent-a".to_string()).await;
        let mut rx2 = hub.subscribe("agent-b".to_string()).await;

        let mut handles = Vec::new();
        for i in 0..20 {
            let h = hub.clone();
            handles.push(tokio::spawn(async move {
                h.publish(Message {
                    id: format!("msg-{}", i),
                    from_agent: "system-scheduler".to_string(),
                    to_agent: "agent-a".to_string(),
                    r#type: "chat".to_string(),
                    content: format!("concurrent message {}", i),
                    occurred_at_unix: i,
                    meeting_id: String::new(),
                })
                .await
                .unwrap();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let mut count = 0;
        while rx1.try_recv().is_ok() {
            count += 1;
        }
        assert_eq!(count, 20, "all 20 concurrent publishes should be received");

        assert!(
            rx2.try_recv().is_err(),
            "agent-b should not receive agent-a messages"
        );
    }

    #[tokio::test]
    async fn test_hub_concurrent_lock_cycle_no_deadlock() {
        let pool = crate::db::secure_pg_pool_options()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://localhost/test")
            .unwrap();
        let (tx, _) = mpsc::channel(100);
        let hub = std::sync::Arc::new(Hub::new(tx, pool));

        let stress = async {
            let h1 = hub.clone();
            let t1 = tokio::spawn(async move {
                for i in 0..80 {
                    let id = format!("agent-{}", i % 8);
                    h1.register_agent(Agent {
                        id: id.clone(),
                        organization_id: "org-1".to_string(),
                        ..Default::default()
                    })
                    .await;
                    h1.fire_agent(&id).await;
                }
            });

            let h2 = hub.clone();
            let t2 = tokio::spawn(async move {
                for i in 0..80 {
                    h2.open_meeting_for_org(
                        "org-1".to_string(),
                        format!("room-{}", i % 4),
                        vec![format!("agent-{}", i % 8)],
                        "sync".to_string(),
                    )
                    .await;
                }
            });

            let h3 = hub.clone();
            let t3 = tokio::spawn(async move {
                for i in 0..80 {
                    let _ = h3
                        .clone()
                        .publish(Message {
                            id: format!("m-{i}"),
                            from_agent: "system-scheduler".to_string(),
                            to_agent: format!("agent-{}", i % 8),
                            r#type: "chat".to_string(),
                            content: "ping".to_string(),
                            occurred_at_unix: i,
                            meeting_id: format!("room-{}", i % 4),
                        })
                        .await;
                }
            });

            let (r1, r2, r3) = tokio::join!(t1, t2, t3);
            r1.unwrap();
            r2.unwrap();
            r3.unwrap();
        };

        tokio::time::timeout(std::time::Duration::from_secs(5), stress)
            .await
            .expect("concurrent fire_agent + open_meeting + publish must never deadlock");
    }

    #[tokio::test]
    async fn test_get_meetings_by_org_exact_equality_prevents_substring_leak() {
        let pool = crate::db::secure_pg_pool_options()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://localhost/test")
            .unwrap();
        let (tx, _) = mpsc::channel(100);
        let hub = std::sync::Arc::new(Hub::new(tx, pool));

        hub.register_agent(Agent {
            id: "a-org".to_string(),
            organization_id: "org".to_string(),
            ..Default::default()
        })
        .await;
        hub.register_agent(Agent {
            id: "a-org-2".to_string(),
            organization_id: "org-2".to_string(),
            ..Default::default()
        })
        .await;

        hub.open_meeting_for_org(
            "org".to_string(),
            "room-primary".to_string(),
            vec!["a-org".to_string()],
            "org agenda".to_string(),
        )
        .await;
        hub.open_meeting_for_org(
            "org-2".to_string(),
            "org-2-secret-room".to_string(),
            vec!["a-org-2".to_string()],
            "org-2 agenda".to_string(),
        )
        .await;

        let org_meetings = hub.get_meetings_by_org("org").await;
        assert_eq!(org_meetings.len(), 1);
        assert_eq!(org_meetings[0].id, "room-primary");

        let org2_meetings = hub.get_meetings_by_org("org-2").await;
        assert_eq!(org2_meetings.len(), 1);
        assert_eq!(org2_meetings[0].id, "org-2-secret-room");

        assert!(hub.get_meetings_by_org("").await.is_empty());
    }

    #[test]
    fn test_hub_cache_keys_are_tenant_scoped() {
        let k_a = tenant_agents_cache_key("org-a");
        let k_b = tenant_agents_cache_key("org-b");
        assert_eq!(k_a, "hub:agents:tenant:org-a");
        assert_ne!(k_a, k_b);
        assert_ne!(k_a, "hub:agents");
        assert_eq!(
            tenant_meetings_cache_key("org-a"),
            "hub:meetings:tenant:org-a"
        );
    }

    #[tokio::test]
    async fn test_hub_bounds_inbox_and_recent_events() {
        let pool = crate::db::secure_pg_pool_options()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://localhost/test")
            .unwrap();
        let (tx, _) = mpsc::channel(100);
        let hub = std::sync::Arc::new(Hub::new(tx, pool));

        for i in 0..(MAX_INBOX_MESSAGES_PER_AGENT + 25) {
            hub.clone()
                .publish(Message {
                    id: format!("msg-{i}"),
                    from_agent: "system-scheduler".to_string(),
                    to_agent: "agent-bounded".to_string(),
                    r#type: "chat".to_string(),
                    content: format!("c-{i}"),
                    occurred_at_unix: i as i64,
                    meeting_id: String::new(),
                })
                .await
                .unwrap();
        }
        let inbox = hub.get_inbox("agent-bounded").await;
        assert_eq!(inbox.len(), MAX_INBOX_MESSAGES_PER_AGENT);
        assert_eq!(inbox[0].id, "msg-25");

        for i in 0..(MAX_RECENT_EVENTS + 30) {
            hub.append_recent_event(HubEvent {
                r#type: "evt".to_string(),
                payload: format!("{i}"),
                occurred_at: Utc::now(),
            })
            .await;
        }
        let events = hub.recent_events(MAX_RECENT_EVENTS + 100).await;
        assert_eq!(events.len(), MAX_RECENT_EVENTS);
    }

    #[test]
    fn test_local_usage_tracker_records_positive_llm_cost() {
        let tracker = LocalUsageTracker::new();
        let (cost, accounting) = tracker.record_event(LocalUsageEvent {
            workspace_id: "tenant-1".to_string(),
            actor_id: "agent-1".to_string(),
            model: "default".to_string(),
            input_tokens: 100_000,
            output_tokens: 50_000,
            cached_input_tokens: 0,
            local_embedding_tokens: 0,
        });
        assert!(
            cost > 0.0,
            "LocalUsageTracker must record positive LLM cost"
        );
        assert_eq!(accounting.workspace_id, "tenant-1");
        assert_eq!(accounting.actor_id, "agent-1");
    }
}
