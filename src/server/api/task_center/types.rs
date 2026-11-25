//! Shared wire enums, DTOs, and canonical digest helpers for Omni Assistant & OHC Task Center.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_ARTIFACT_CONTENT_BYTES: usize = 65_536;
pub const MAX_PROMPT_CHARS: usize = 16_000;
pub const MAX_SPACE_NAME_CHARS: usize = 80;
pub const MAX_TASK_TITLE_CHARS: usize = 160;
pub const MAX_MICROS: i64 = 1_000_000_000_000;
pub const DEFAULT_DEPARTMENTS: [&str; 4] = ["finance", "marketing", "operations", "sales"];

macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }
        impl $name {
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }
            pub fn parse(value: &str) -> Result<Self, StoreError> {
                match value {
                    $($wire => Ok(Self::$variant),)+
                    _ => Err(StoreError::Database(concat!("corrupt ", stringify!($name)).into())),
                }
            }
        }
    };
}

wire_enum!(TaskCenterTaskStatus {
    Draft => "draft",
    Queued => "queued",
    Running => "running",
    AwaitingApproval => "awaiting_approval",
    WaitingForInput => "waiting_for_input",
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled",
    OutcomeUnknown => "outcome_unknown",
});

wire_enum!(ProvenanceState {
    Draft => "draft",
    AwaitingApproval => "awaiting_approval",
    Verified => "verified",
    Failed => "failed",
    Cancelled => "cancelled",
    OutcomeUnknown => "outcome_unknown",
    Unconfigured => "unconfigured",
});

wire_enum!(AttentionRing {
    None => "none",
    PermissionRequired => "permission_required",
    NeedsInput => "needs_input",
    Blocked => "blocked",
    CompletedUnread => "completed_unread",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceKind {
    Operations,
    ClientDelivery,
    CashRecovery,
    Research,
    #[default]
    Custom,
}

impl SpaceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Operations => "operations",
            Self::ClientDelivery => "client_delivery",
            Self::CashRecovery => "cash_recovery",
            Self::Research => "research",
            Self::Custom => "custom",
        }
    }
    pub fn parse(value: &str) -> Result<Self, StoreError> {
        match value {
            "operations" => Ok(Self::Operations),
            "client_delivery" => Ok(Self::ClientDelivery),
            "cash_recovery" => Ok(Self::CashRecovery),
            "research" => Ok(Self::Research),
            "custom" => Ok(Self::Custom),
            _ => Err(StoreError::Database("corrupt SpaceKind".into())),
        }
    }
}

wire_enum!(SpaceSourceHealth {
    Live => "live",
    Offline => "offline",
    Revoked => "revoked",
    Unconfigured => "unconfigured",
});

wire_enum!(ActionRiskTier {
    AutoExecute => "auto_execute",
    DraftForReview => "draft_for_review",
});

wire_enum!(PayerMode {
    ManagedApi => "managed_api",
    ByokApi => "byok_api",
    Local => "local",
    Unconfigured => "unconfigured",
});

wire_enum!(StepKind {
    Plan => "plan",
    ToolCall => "tool_call",
    LlmTurn => "llm_turn",
    ApprovalGate => "approval_gate",
    Artifact => "artifact",
    System => "system",
});

wire_enum!(ApprovalStatus {
    Pending => "pending",
    Approved => "approved",
    Rejected => "rejected",
    Expired => "expired",
});

wire_enum!(ArtifactFormat {
    Markdown => "markdown",
    Table => "table",
    Checklist => "checklist",
    SpaceCard => "space_card",
    Json => "json",
});

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpaceRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub kind: SpaceKind,
    pub prompt_template: String,
    pub department: String,
    pub reads: Vec<String>,
    pub shows: Vec<String>,
    pub cannot: Vec<String>,
    pub source_health: SpaceSourceHealth,
    pub pinned_artifact_ids: Vec<String>,
    pub archived: bool,
    pub created_at_unix: i64,
    pub updated_at_unix: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskStepRecord {
    pub id: String,
    pub task_id: String,
    pub seq: i32,
    pub kind: StepKind,
    pub title: String,
    pub detail: String,
    pub status: TaskCenterTaskStatus,
    pub provenance: ProvenanceState,
    pub tool_name: Option<String>,
    pub cost_micros: i64,
    pub created_at_unix: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskApprovalPayload {
    pub approval_id: String,
    pub action_kind: String,
    pub department: String,
    pub risk: ActionRiskTier,
    pub summary: String,
    pub payload_preview: serde_json::Map<String, serde_json::Value>,
    pub payload_sha256: String,
    pub estimated_cost_micros: i64,
    pub standing_limit_micros: i64,
    pub status: ApprovalStatus,
    pub decided_by: Option<String>,
    pub decided_at_unix: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskArtifactRecord {
    pub id: String,
    pub task_id: String,
    pub space_id: Option<String>,
    pub title: String,
    pub format: ArtifactFormat,
    pub content: String,
    pub provenance: ProvenanceState,
    pub pinned: bool,
    pub created_at_unix: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCenterTask {
    pub id: String,
    pub space_id: Option<String>,
    pub title: String,
    pub prompt: String,
    pub department: String,
    pub status: TaskCenterTaskStatus,
    pub provenance: ProvenanceState,
    pub attention: AttentionRing,
    pub risk: ActionRiskTier,
    pub payer_mode: PayerMode,
    pub estimated_cost_micros: i64,
    pub actual_cost_micros: Option<i64>,
    pub idempotency_key: String,
    pub receipt_id: Option<String>,
    pub output_summary: Option<String>,
    pub error_message: Option<String>,
    pub pending_approval: Option<TaskApprovalPayload>,
    pub steps: Vec<TaskStepRecord>,
    pub artifacts: Vec<TaskArtifactRecord>,
    pub archived: bool,
    pub created_at_unix: i64,
    pub updated_at_unix: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StandingPolicyRecord {
    pub department: String,
    pub auto_approve_max_micros: i64,
    pub allow_external_send: bool,
    pub allow_financial_commit: bool,
    pub updated_at_unix: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCenterRuntimeInfo {
    pub llm_configured: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub payer_mode: PayerMode,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCenterBudgetInfo {
    pub configured: bool,
    pub limit_micros: i64,
    pub spent_micros: i64,
    pub reserved_micros: i64,
    pub currency: String,
    pub unit: String,
    pub customer_direct_inference_is_not_rebilled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCenterSummaryResponse {
    pub spaces: Vec<SpaceRecord>,
    pub tasks: Vec<TaskCenterTask>,
    pub standing_policies: Vec<StandingPolicyRecord>,
    pub runtime: TaskCenterRuntimeInfo,
    pub budget: TaskCenterBudgetInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCenterSseEvent {
    pub event_id: String,
    pub kind: String,
    pub tenant_id: String,
    pub occurred_at_unix: i64,
    pub payload: serde_json::Value,
}

#[derive(Debug)]
pub enum StoreError {
    Invalid(&'static str),
    NotFound,
    Conflict(&'static str),
    Database(String),
}

impl From<sqlx::Error> for StoreError {
    fn from(e: sqlx::Error) -> Self {
        Self::Database(e.to_string())
    }
}

#[derive(Debug, Clone)]
pub enum SpacePatch {
    Rename(String),
    Archive,
    Unarchive,
    PinArtifact(String),
    UnpinArtifact(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    CancelledPreDispatch,
    MarkedOutcomeUnknownInFlight,
    PreservedOutcomeUnknown,
    AlreadyCancelled,
}

pub struct StoredTaskMeta {
    pub task: TaskCenterTask,
    pub request_digest: String,
    pub usage_event_id: Option<String>,
}

pub struct ApprovalDecisionOutcome {
    pub task: TaskCenterTask,
    pub usage_event_id: Option<String>,
    pub request_digest: String,
}

pub struct TaskTransition<'a> {
    pub expected: TaskCenterTaskStatus,
    pub status: TaskCenterTaskStatus,
    pub provenance: ProvenanceState,
    pub attention: AttentionRing,
    pub actual_cost_micros: Option<i64>,
    pub receipt_id: Option<&'a str>,
    pub output_summary: Option<&'a str>,
    pub error_message: Option<&'a str>,
    pub now: i64,
}

pub struct NewStep<'a> {
    pub kind: StepKind,
    pub title: &'a str,
    pub detail: &'a str,
    pub status: TaskCenterTaskStatus,
    pub provenance: ProvenanceState,
    pub tool_name: Option<&'a str>,
    pub cost_micros: i64,
    pub now: i64,
}

pub struct NewTaskBundle<'a> {
    pub actor_id: &'a str,
    pub requested_action: &'a str,
    pub request_digest: &'a str,
    pub usage_event_id: Option<&'a str>,
    pub task: &'a TaskCenterTask,
    pub initial_steps: &'a [TaskStepRecord],
    pub pending_approval: Option<&'a TaskApprovalPayload>,
}

pub fn canonical_sha256_json(value: &serde_json::Value) -> Result<String, StoreError> {
    let encoded = serde_jcs::to_vec(value).map_err(|_| StoreError::Invalid("invalid json"))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

pub fn contains_unsafe_html(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    ["<script", "javascript:", "<iframe", "onerror=", "onload="]
        .iter()
        .any(|needle| lower.contains(needle))
}
