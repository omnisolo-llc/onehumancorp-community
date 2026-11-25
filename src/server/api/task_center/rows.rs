//! SQLx row mappings and typed decoders shared across Postgres and SQLite.
use super::types::{
    ActionRiskTier, ApprovalStatus, ArtifactFormat, AttentionRing, PayerMode, ProvenanceState,
    SpaceKind, SpaceRecord, SpaceSourceHealth, StepKind, StoreError, StoredTaskMeta,
    TaskApprovalPayload, TaskArtifactRecord, TaskCenterTask, TaskCenterTaskStatus, TaskStepRecord,
};
use serde::Deserialize;

pub(super) const SELECT_SPACE: &str = "SELECT id, name, description, kind, prompt_template, department, reads_json, shows_json, cannot_json, source_health, pinned_artifact_ids_json, archived, created_at_unix, updated_at_unix FROM task_center_spaces";
pub(super) const SELECT_TASK: &str = "SELECT id, space_id, title, prompt, department, status, provenance, attention, risk, payer_mode, estimated_cost_micros, actual_cost_micros, idempotency_key, request_digest, receipt_id, usage_event_id, output_summary, error_message, archived, created_at_unix, updated_at_unix FROM task_center_tasks";
pub(super) const SELECT_STEP: &str = "SELECT id, task_id, seq, kind, title, detail, status, provenance, tool_name, cost_micros, created_at_unix FROM task_center_steps";
pub(super) const SELECT_ARTIFACT: &str = "SELECT id, task_id, space_id, title, format, content, provenance, pinned, created_at_unix FROM task_center_artifacts";
pub(super) const SELECT_APPROVAL: &str = "SELECT approval_id, task_id, action_kind, department, risk, summary, payload_preview_json, payload_sha256, estimated_cost_micros, standing_limit_micros, status, decided_by, decided_at_unix FROM task_center_approvals";

#[derive(sqlx::FromRow)]
pub(super) struct SpaceRow {
    pub id: String,
    pub name: String,
    pub description: String,
    pub kind: String,
    pub prompt_template: String,
    pub department: String,
    pub reads_json: String,
    pub shows_json: String,
    pub cannot_json: String,
    pub source_health: String,
    pub pinned_artifact_ids_json: String,
    pub archived: bool,
    pub created_at_unix: i64,
    pub updated_at_unix: i64,
}

#[derive(sqlx::FromRow)]
pub(super) struct TaskRow {
    pub id: String,
    pub space_id: Option<String>,
    pub title: String,
    pub prompt: String,
    pub department: String,
    pub status: String,
    pub provenance: String,
    pub attention: String,
    pub risk: String,
    pub payer_mode: String,
    pub estimated_cost_micros: i64,
    pub actual_cost_micros: Option<i64>,
    pub idempotency_key: String,
    pub request_digest: String,
    pub receipt_id: Option<String>,
    pub usage_event_id: Option<String>,
    pub output_summary: Option<String>,
    pub error_message: Option<String>,
    pub archived: bool,
    pub created_at_unix: i64,
    pub updated_at_unix: i64,
}

#[derive(sqlx::FromRow)]
pub(super) struct StepRow {
    pub id: String,
    pub task_id: String,
    pub seq: i32,
    pub kind: String,
    pub title: String,
    pub detail: String,
    pub status: String,
    pub provenance: String,
    pub tool_name: Option<String>,
    pub cost_micros: i64,
    pub created_at_unix: i64,
}

#[derive(sqlx::FromRow)]
pub(super) struct ArtifactRow {
    pub id: String,
    pub task_id: String,
    pub space_id: Option<String>,
    pub title: String,
    pub format: String,
    pub content: String,
    pub provenance: String,
    pub pinned: bool,
    pub created_at_unix: i64,
}

#[derive(sqlx::FromRow)]
pub(super) struct ApprovalRow {
    pub approval_id: String,
    pub task_id: String,
    pub action_kind: String,
    pub department: String,
    pub risk: String,
    pub summary: String,
    pub payload_preview_json: String,
    pub payload_sha256: String,
    pub estimated_cost_micros: i64,
    pub standing_limit_micros: i64,
    pub status: String,
    pub decided_by: Option<String>,
    pub decided_at_unix: Option<i64>,
}

#[derive(sqlx::FromRow)]
pub(super) struct PolicyRow {
    pub department: String,
    pub auto_approve_max_micros: i64,
    pub allow_external_send: bool,
    pub allow_financial_commit: bool,
    pub updated_at_unix: i64,
}

fn parse_json<T: for<'de> Deserialize<'de>>(raw: &str, label: &str) -> Result<T, StoreError> {
    serde_json::from_str(raw).map_err(|_| StoreError::Database(format!("corrupt {label}")))
}

impl TryFrom<SpaceRow> for SpaceRecord {
    type Error = StoreError;
    fn try_from(r: SpaceRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: r.id,
            name: r.name,
            description: r.description,
            kind: SpaceKind::parse(&r.kind)?,
            prompt_template: r.prompt_template,
            department: r.department,
            reads: parse_json(&r.reads_json, "space reads")?,
            shows: parse_json(&r.shows_json, "space shows")?,
            cannot: parse_json(&r.cannot_json, "space cannot")?,
            source_health: SpaceSourceHealth::parse(&r.source_health)?,
            pinned_artifact_ids: parse_json(&r.pinned_artifact_ids_json, "pinned artifacts")?,
            archived: r.archived,
            created_at_unix: r.created_at_unix,
            updated_at_unix: r.updated_at_unix,
        })
    }
}

impl TryFrom<StepRow> for TaskStepRecord {
    type Error = StoreError;
    fn try_from(r: StepRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: r.id,
            task_id: r.task_id,
            seq: r.seq,
            kind: StepKind::parse(&r.kind)?,
            title: r.title,
            detail: r.detail,
            status: TaskCenterTaskStatus::parse(&r.status)?,
            provenance: ProvenanceState::parse(&r.provenance)?,
            tool_name: r.tool_name,
            cost_micros: r.cost_micros,
            created_at_unix: r.created_at_unix,
        })
    }
}

impl TryFrom<ArtifactRow> for TaskArtifactRecord {
    type Error = StoreError;
    fn try_from(r: ArtifactRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: r.id,
            task_id: r.task_id,
            space_id: r.space_id,
            title: r.title,
            format: ArtifactFormat::parse(&r.format)?,
            content: r.content,
            provenance: ProvenanceState::parse(&r.provenance)?,
            pinned: r.pinned,
            created_at_unix: r.created_at_unix,
        })
    }
}

impl TryFrom<ApprovalRow> for TaskApprovalPayload {
    type Error = StoreError;
    fn try_from(r: ApprovalRow) -> Result<Self, StoreError> {
        Ok(Self {
            approval_id: r.approval_id,
            action_kind: r.action_kind,
            department: r.department,
            risk: ActionRiskTier::parse(&r.risk)?,
            summary: r.summary,
            payload_preview: parse_json(&r.payload_preview_json, "approval payload")?,
            payload_sha256: r.payload_sha256,
            estimated_cost_micros: r.estimated_cost_micros,
            standing_limit_micros: r.standing_limit_micros,
            status: ApprovalStatus::parse(&r.status)?,
            decided_by: r.decided_by,
            decided_at_unix: r.decided_at_unix,
        })
    }
}

pub(super) fn decode_task(
    r: TaskRow,
    steps: Vec<TaskStepRecord>,
    artifacts: Vec<TaskArtifactRecord>,
    pending_approval: Option<TaskApprovalPayload>,
) -> Result<StoredTaskMeta, StoreError> {
    Ok(StoredTaskMeta {
        task: TaskCenterTask {
            id: r.id,
            space_id: r.space_id,
            title: r.title,
            prompt: r.prompt,
            department: r.department,
            status: TaskCenterTaskStatus::parse(&r.status)?,
            provenance: ProvenanceState::parse(&r.provenance)?,
            attention: AttentionRing::parse(&r.attention)?,
            risk: ActionRiskTier::parse(&r.risk)?,
            payer_mode: PayerMode::parse(&r.payer_mode)?,
            estimated_cost_micros: r.estimated_cost_micros,
            actual_cost_micros: r.actual_cost_micros,
            idempotency_key: r.idempotency_key,
            receipt_id: r.receipt_id,
            output_summary: r.output_summary,
            error_message: r.error_message,
            pending_approval,
            steps,
            artifacts,
            archived: r.archived,
            created_at_unix: r.created_at_unix,
            updated_at_unix: r.updated_at_unix,
        },
        request_digest: r.request_digest,
        usage_event_id: r.usage_event_id,
    })
}
