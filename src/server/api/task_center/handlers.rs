//! Task lifecycle HTTP handlers for `/api/v1/task-center/tasks/*`.
use super::executor::{
    ExecutionJob, SseEventKind, TaskCenterExecutor, evaluate_standing_authority,
};
use super::types::{
    ActionRiskTier, ApprovalStatus, AttentionRing, CancelOutcome, MAX_PROMPT_CHARS,
    MAX_TASK_TITLE_CHARS, NewStep, NewTaskBundle, PayerMode, ProvenanceState, StepKind,
    TaskApprovalPayload, TaskCenterTask, TaskCenterTaskStatus, TaskStepRecord,
    canonical_sha256_json,
};
use super::{
    ApiError, TaskCenterState, normalize_space_health, parse_idempotency_key, private_json,
    require_owner_or_admin, require_tenant, valid_id,
};
use axum::{
    Json,
    extract::{Extension, Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use chrono::Utc;
use serde::Deserialize;
use server_common::Claims;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateTaskRequest {
    prompt: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    department: Option<String>,
    #[serde(default)]
    requested_action: Option<String>,
    #[serde(default)]
    estimated_cost_micros: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ApproveTaskRequest {
    approved: bool,
    payload_sha256: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PostTaskMessageRequest {
    #[serde(alias = "message", alias = "prompt")]
    content: String,
    #[serde(default)]
    estimated_cost_micros: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PinArtifactRequest {
    space_id: String,
    pinned: bool,
}

pub(super) async fn list_tasks(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
) -> Result<Response, ApiError> {
    let tenant_id = require_tenant(&claims)?;
    let tasks = state.store.list_tasks(&tenant_id).await?;
    Ok(private_json(
        StatusCode::OK,
        serde_json::json!({ "tasks": tasks }),
    ))
}

pub(super) async fn get_task(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
    Path(task_id): Path<String>,
) -> Result<Response, ApiError> {
    let tenant_id = require_tenant(&claims)?;
    let meta = state
        .store
        .get_task_meta(&tenant_id, &task_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "Task not found"))?;
    Ok(private_json(StatusCode::OK, meta.task))
}

fn build_approval_payload(
    action: &str,
    department: &str,
    prompt: &str,
    space_id: Option<&str>,
    estimated_cost_micros: i64,
    standing_limit_micros: i64,
) -> Result<TaskApprovalPayload, ApiError> {
    let mut preview = serde_json::Map::new();
    preview.insert("action_kind".into(), serde_json::json!(action));
    preview.insert("department".into(), serde_json::json!(department));
    preview.insert(
        "estimated_cost_micros".into(),
        serde_json::json!(estimated_cost_micros),
    );
    preview.insert("prompt".into(), serde_json::json!(prompt));
    if let Some(sid) = space_id {
        preview.insert("space_id".into(), serde_json::json!(sid));
    }
    let payload_sha256 = canonical_sha256_json(&serde_json::Value::Object(preview.clone()))?;
    Ok(TaskApprovalPayload {
        approval_id: format!("apr-{}", Uuid::new_v4()),
        action_kind: action.into(),
        department: department.into(),
        risk: ActionRiskTier::DraftForReview,
        summary: format!(
            "Approval required for '{action}' in '{department}' (est. {estimated_cost_micros} micro-USD vs standing limit {standing_limit_micros} micro-USD)"
        ),
        payload_preview: preview,
        payload_sha256,
        estimated_cost_micros,
        standing_limit_micros,
        status: ApprovalStatus::Pending,
        decided_by: None,
        decided_at_unix: None,
    })
}

pub(super) async fn create_task(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
    headers: HeaderMap,
    Json(req): Json<CreateTaskRequest>,
) -> Result<Response, ApiError> {
    let tenant_id = require_tenant(&claims)?;
    let idempotency_key = parse_idempotency_key(&headers)?;
    let prompt = req.prompt.trim();
    if prompt.is_empty() || prompt.chars().count() > MAX_PROMPT_CHARS || prompt.contains('\0') {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Task prompt must be non-empty and within 16,000 characters",
        ));
    }
    let action = req
        .requested_action
        .unwrap_or_else(|| "run_task".into())
        .trim()
        .to_ascii_lowercase();
    if !valid_id(&action, 64) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid requested_action",
        ));
    }
    let space = match req.space_id.as_deref() {
        Some(sid) => {
            if !valid_id(sid, 128) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "Invalid space_id",
                ));
            }
            Some(
                state
                    .store
                    .get_space(&tenant_id, sid)
                    .await?
                    .ok_or_else(|| {
                        ApiError::new(StatusCode::NOT_FOUND, "not_found", "Space not found")
                    })?,
            )
        }
        None => None,
    };
    let department = req
        .department
        .map(|d| d.trim().to_ascii_lowercase())
        .or_else(|| space.as_ref().map(|s| s.department.clone()))
        .unwrap_or_else(|| "operations".into());
    if !valid_id(&department, 64) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid department",
        ));
    }
    let title = match req.title.as_deref().map(str::trim) {
        Some(t) if !t.is_empty() && t.chars().count() <= MAX_TASK_TITLE_CHARS => t.to_string(),
        Some(_) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Task title must be 1..=160 characters",
            ));
        }
        None => prompt
            .lines()
            .next()
            .unwrap_or("Task")
            .trim()
            .chars()
            .take(MAX_TASK_TITLE_CHARS)
            .collect(),
    };

    let request_digest = canonical_sha256_json(&serde_json::json!({
        "department": &department,
        "estimated_cost_micros": req.estimated_cost_micros,
        "prompt": prompt,
        "requested_action": &action,
        "space_id": space.as_ref().map(|s| s.id.as_str()),
        "title": &title,
    }))?;
    if let Some(existing) = state
        .store
        .find_task_by_idempotency(&tenant_id, &idempotency_key)
        .await?
    {
        return if existing.request_digest == request_digest {
            Ok(private_json(StatusCode::OK, existing.task))
        } else {
            Err(ApiError::new(
                StatusCode::CONFLICT,
                "idempotency_key_conflict",
                "Idempotency-Key was already used with a different task payload",
            ))
        };
    }

    let Some(llm_cfg) = state.executor.resolve_for_tenant(&tenant_id).await else {
        return Err(ApiError::unconfigured(
            "No LLM inference provider is configured for this runtime; no task was dispatched",
        ));
    };
    let est_cost =
        TaskCenterExecutor::estimate_cost_micros(&llm_cfg, prompt, req.estimated_cost_micros)?;
    let policy = state
        .store
        .get_standing_policy(&tenant_id, &department)
        .await?;
    let risk = evaluate_standing_authority(&policy, &action, est_cost);

    let task_id = format!("tsk-{}", Uuid::new_v4());
    let usage_event_id = format!("use-{}", Uuid::new_v4());
    let tracked = state
        .executor
        .reserve_usage(
            &tenant_id,
            &task_id,
            &usage_event_id,
            &request_digest,
            &llm_cfg,
            est_cost,
        )
        .await?;
    let stored_usage_id = tracked.then_some(usage_event_id);
    let now = Utc::now().timestamp();

    let (status, provenance, attention, pending_approval, step) = match risk {
        ActionRiskTier::AutoExecute => (
            TaskCenterTaskStatus::Queued,
            ProvenanceState::Draft,
            AttentionRing::None,
            None,
            TaskStepRecord {
                id: format!("stp-{}", Uuid::new_v4()),
                task_id: task_id.clone(),
                seq: 1,
                kind: StepKind::Plan,
                title: "Admitted under standing authority".into(),
                detail: format!(
                    "Action '{action}' in '{department}' within {} micro-USD limit",
                    policy.auto_approve_max_micros
                ),
                status: TaskCenterTaskStatus::Queued,
                provenance: ProvenanceState::Draft,
                tool_name: None,
                cost_micros: 0,
                created_at_unix: now,
            },
        ),
        ActionRiskTier::DraftForReview => {
            let approval = build_approval_payload(
                &action,
                &department,
                prompt,
                space.as_ref().map(|s| s.id.as_str()),
                est_cost,
                policy.auto_approve_max_micros,
            )?;
            let step = TaskStepRecord {
                id: format!("stp-{}", Uuid::new_v4()),
                task_id: task_id.clone(),
                seq: 1,
                kind: StepKind::ApprovalGate,
                title: "Awaiting owner approval".into(),
                detail: approval.summary.clone(),
                status: TaskCenterTaskStatus::AwaitingApproval,
                provenance: ProvenanceState::AwaitingApproval,
                tool_name: None,
                cost_micros: 0,
                created_at_unix: now,
            };
            (
                TaskCenterTaskStatus::AwaitingApproval,
                ProvenanceState::AwaitingApproval,
                AttentionRing::PermissionRequired,
                Some(approval),
                step,
            )
        }
    };

    let task = TaskCenterTask {
        id: task_id.clone(),
        space_id: space.as_ref().map(|s| s.id.clone()),
        title,
        prompt: prompt.into(),
        department,
        status,
        provenance,
        attention,
        risk,
        payer_mode: llm_cfg.payer_mode,
        estimated_cost_micros: est_cost,
        actual_cost_micros: None,
        idempotency_key: idempotency_key.clone(),
        receipt_id: None,
        output_summary: None,
        error_message: None,
        pending_approval: pending_approval.clone(),
        steps: vec![step],
        artifacts: vec![],
        archived: false,
        created_at_unix: now,
        updated_at_unix: now,
    };
    let bundle = NewTaskBundle {
        actor_id: &claims.sub,
        requested_action: &action,
        request_digest: &request_digest,
        usage_event_id: stored_usage_id.as_deref(),
        task: &task,
        initial_steps: &task.steps,
        pending_approval: pending_approval.as_ref(),
    };
    if let Err(err) = state.store.insert_task_bundle(&tenant_id, &bundle).await {
        if let Some(eid) = stored_usage_id.as_deref() {
            state
                .executor
                .cancel_usage_before_dispatch(&tenant_id, eid)
                .await;
        }
        if let Ok(Some(existing)) = state
            .store
            .find_task_by_idempotency(&tenant_id, &idempotency_key)
            .await
        {
            return if existing.request_digest == request_digest {
                Ok(private_json(StatusCode::OK, existing.task))
            } else {
                Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "idempotency_key_conflict",
                    "Idempotency-Key conflict",
                ))
            };
        }
        return Err(err.into());
    }

    state.events.publish(
        &tenant_id,
        SseEventKind::TaskUpdated,
        serde_json::to_value(&task).unwrap_or_default(),
    );
    if let Some(approval) = &pending_approval {
        state.events.publish(
            &tenant_id,
            SseEventKind::ApprovalRequired,
            serde_json::json!({ "task_id": task.id, "approval": approval }),
        );
        return Ok(private_json(StatusCode::CREATED, task));
    }

    let exec_prompt = match space
        .as_ref()
        .filter(|s| !s.prompt_template.trim().is_empty())
    {
        Some(s) => format!(
            "Space context ({}): {}\n\nTask:\n{}",
            s.name, s.prompt_template, prompt
        ),
        None => prompt.into(),
    };
    state.executor.spawn_execution(ExecutionJob {
        store: state.store.clone(),
        events: state.events.clone(),
        tenant_id,
        task_id: task.id.clone(),
        space_id: task.space_id.clone(),
        prompt: exec_prompt,
        cfg: llm_cfg,
        usage_event_id: stored_usage_id,
    });
    Ok(private_json(StatusCode::ACCEPTED, task))
}

pub(super) async fn approve_task(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
    Path(task_id): Path<String>,
    Json(req): Json<ApproveTaskRequest>,
) -> Result<Response, ApiError> {
    let tenant_id = require_owner_or_admin(&claims)?;
    let sha = req.payload_sha256.trim();
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "payload_sha256 must be a 64-character hex SHA-256 digest",
        ));
    }
    let reason = match req.reason.as_deref().map(str::trim) {
        Some(r) if r.chars().count() > 1000 || r.contains('\0') => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid approval reason",
            ));
        }
        Some(r) if !r.is_empty() => Some(r),
        _ => None,
    };
    let llm_cfg = if req.approved {
        Some(
            state
                .executor
                .resolve_for_tenant(&tenant_id)
                .await
                .ok_or_else(|| {
                    ApiError::unconfigured(
                        "No LLM inference provider is configured; approval cannot dispatch work",
                    )
                })?,
        )
    } else {
        None
    };

    let outcome = state
        .store
        .decide_approval_cas(
            &tenant_id,
            &task_id,
            &sha.to_ascii_lowercase(),
            req.approved,
            &claims.sub,
            reason,
            Utc::now().timestamp(),
        )
        .await?;
    state.events.publish(
        &tenant_id,
        SseEventKind::TaskUpdated,
        serde_json::to_value(&outcome.task).unwrap_or_default(),
    );

    if !req.approved {
        if let Some(eid) = outcome.usage_event_id.as_deref() {
            state
                .executor
                .cancel_usage_before_dispatch(&tenant_id, eid)
                .await;
        }
        return Ok(private_json(StatusCode::OK, outcome.task));
    }

    let cfg = llm_cfg.expect("checked above");
    let mut usage_event_id = outcome.usage_event_id;
    if usage_event_id.is_none() && cfg.payer_mode == PayerMode::ManagedApi {
        let new_eid = format!("use-{}", Uuid::new_v4());
        let digest = if outcome.request_digest.len() == 64 {
            outcome.request_digest
        } else {
            canonical_sha256_json(&serde_json::json!({ "task_id": &outcome.task.id }))?
        };
        if state
            .executor
            .reserve_usage(
                &tenant_id,
                &outcome.task.id,
                &new_eid,
                &digest,
                &cfg,
                outcome.task.estimated_cost_micros,
            )
            .await?
        {
            usage_event_id = Some(new_eid);
        }
    }
    state.executor.spawn_execution(ExecutionJob {
        store: state.store.clone(),
        events: state.events.clone(),
        tenant_id,
        task_id: outcome.task.id.clone(),
        space_id: outcome.task.space_id.clone(),
        prompt: outcome.task.prompt.clone(),
        cfg,
        usage_event_id,
    });
    Ok(private_json(StatusCode::OK, outcome.task))
}

pub(super) async fn cancel_task(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
    Path(task_id): Path<String>,
) -> Result<Response, ApiError> {
    let tenant_id = require_tenant(&claims)?;
    let (task, outcome, usage_event_id) = state
        .store
        .cancel_task_cas(&tenant_id, &task_id, &claims.sub, Utc::now().timestamp())
        .await?;
    match outcome {
        CancelOutcome::CancelledPreDispatch => {
            state.executor.signal_cancel(&task_id);
            if let Some(eid) = usage_event_id.as_deref() {
                state
                    .executor
                    .cancel_usage_before_dispatch(&tenant_id, eid)
                    .await;
            }
            state.events.publish(
                &tenant_id,
                SseEventKind::TaskUpdated,
                serde_json::to_value(&task).unwrap_or_default(),
            );
        }
        CancelOutcome::MarkedOutcomeUnknownInFlight => {
            state.executor.signal_cancel(&task_id);
            if let Some(eid) = usage_event_id.as_deref() {
                state
                    .executor
                    .mark_usage_reconciliation_required(&tenant_id, eid)
                    .await;
            }
            state.events.publish(
                &tenant_id,
                SseEventKind::TaskUpdated,
                serde_json::to_value(&task).unwrap_or_default(),
            );
        }
        CancelOutcome::PreservedOutcomeUnknown | CancelOutcome::AlreadyCancelled => {}
    }
    Ok(private_json(StatusCode::OK, task))
}

pub(super) async fn post_task_message(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
    Path(task_id): Path<String>,
    Json(req): Json<PostTaskMessageRequest>,
) -> Result<Response, ApiError> {
    let tenant_id = require_tenant(&claims)?;
    let content = req.content.trim();
    if content.is_empty() || content.chars().count() > MAX_PROMPT_CHARS || content.contains('\0') {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Message content must be non-empty and within 16,000 characters",
        ));
    }
    let meta = state
        .store
        .get_task_meta(&tenant_id, &task_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "Task not found"))?;
    match meta.task.status {
        TaskCenterTaskStatus::OutcomeUnknown => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "outcome_unknown_no_retry",
                "Task outcome is unknown; reconcile externally before starting new work (automatic retry is disabled)",
            ));
        }
        TaskCenterTaskStatus::AwaitingApproval => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "approval_pending",
                "Resolve the pending owner approval first",
            ));
        }
        TaskCenterTaskStatus::Queued | TaskCenterTaskStatus::Running => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "task_in_progress",
                "Task is already executing",
            ));
        }
        _ => {}
    }
    let Some(llm_cfg) = state.executor.resolve_for_tenant(&tenant_id).await else {
        return Err(ApiError::unconfigured(
            "No LLM inference provider is configured; follow-up turn was not dispatched",
        ));
    };
    let est_cost =
        TaskCenterExecutor::estimate_cost_micros(&llm_cfg, content, req.estimated_cost_micros)?;
    let usage_event_id = format!("use-{}", Uuid::new_v4());
    let turn_digest = canonical_sha256_json(&serde_json::json!({
        "task_id": &task_id,
        "content": content,
        "event_id": &usage_event_id,
    }))?;
    let tracked = state
        .executor
        .reserve_usage(
            &tenant_id,
            &task_id,
            &usage_event_id,
            &turn_digest,
            &llm_cfg,
            est_cost,
        )
        .await?;
    let stored_usage_id = tracked.then_some(usage_event_id);
    let now = Utc::now().timestamp();
    if !state
        .store
        .prepare_followup_cas(&tenant_id, &task_id, stored_usage_id.as_deref(), now)
        .await?
    {
        if let Some(eid) = stored_usage_id.as_deref() {
            state
                .executor
                .cancel_usage_before_dispatch(&tenant_id, eid)
                .await;
        }
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "task_state_changed",
            "Task state changed concurrently",
        ));
    }
    let step = state
        .store
        .append_step(
            &tenant_id,
            &task_id,
            &NewStep {
                kind: StepKind::Plan,
                title: "Follow-up turn submitted",
                detail: content,
                status: TaskCenterTaskStatus::Queued,
                provenance: ProvenanceState::Draft,
                tool_name: None,
                cost_micros: 0,
                now,
            },
        )
        .await?;
    state.events.publish(
        &tenant_id,
        SseEventKind::StepAppended,
        serde_json::to_value(&step).unwrap_or_default(),
    );
    let updated = state
        .store
        .get_task_meta(&tenant_id, &task_id)
        .await?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found", "Task not found"))?
        .task;
    let prompt = format!(
        "Original task:\n{}\n\nPrevious summary:\n{}\n\nFollow-up instruction:\n{}",
        updated.prompt,
        updated.output_summary.as_deref().unwrap_or("(none)"),
        content
    );
    state.executor.spawn_execution(ExecutionJob {
        store: state.store.clone(),
        events: state.events.clone(),
        tenant_id,
        task_id: updated.id.clone(),
        space_id: updated.space_id.clone(),
        prompt,
        cfg: llm_cfg,
        usage_event_id: stored_usage_id,
    });
    Ok(private_json(StatusCode::ACCEPTED, updated))
}

pub(super) async fn pin_task_artifact(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
    Path((task_id, artifact_id)): Path<(String, String)>,
    Json(req): Json<PinArtifactRequest>,
) -> Result<Response, ApiError> {
    let tenant_id = require_tenant(&claims)?;
    if !valid_id(&req.space_id, 128) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid space_id",
        ));
    }
    let (artifact, mut space) = state
        .store
        .pin_artifact(
            &tenant_id,
            &task_id,
            &artifact_id,
            &req.space_id,
            req.pinned,
            Utc::now().timestamp(),
        )
        .await?;
    normalize_space_health(
        &mut space,
        state.executor.runtime_info(&tenant_id).await.llm_configured,
    );
    state.events.publish(
        &tenant_id,
        SseEventKind::SpaceUpdated,
        serde_json::to_value(&space).unwrap_or_default(),
    );
    Ok(private_json(StatusCode::OK, artifact))
}
