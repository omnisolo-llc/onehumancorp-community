#![allow(clippy::result_large_err)]
// SPDX-License-Identifier: Apache-2.0
//! Workspace-scoped authorization, onboarding wizard, and discovery helpers for `MyHubService`.

use std::pin::Pin;
use std::sync::Arc;

use ::server_auth::orchestration::AuthInfo;
use ::server_omnisolo::orchestration::*;
use sqlx::Row;
use tokio_stream::Stream;
use tonic::{Request, Status};

const RESERVED_CONTROL_CHANNELS: [&str; 6] = [
    "agent_jobs",
    "ralph_jobs",
    "task.assigned",
    "agent_events",
    "tasks",
    "products_inbox",
];

pub fn require_auth_info<T>(request: &Request<T>) -> Result<AuthInfo, Status> {
    let auth = request
        .extensions()
        .get::<AuthInfo>()
        .cloned()
        .ok_or_else(|| Status::unauthenticated("Missing authentication context"))?;
    let org = auth.org_id.trim();
    if org.is_empty() || org.eq_ignore_ascii_case("system") {
        return Err(Status::permission_denied(
            "Valid workspace organization context is required",
        ));
    }
    if auth.agent_id.trim().is_empty() {
        return Err(Status::permission_denied(
            "Valid authenticated agent identity is required",
        ));
    }
    Ok(AuthInfo {
        org_id: org.to_string(),
        agent_id: auth.agent_id.trim().to_string(),
        spiffe_id: auth.spiffe_id,
    })
}

pub fn require_org_match(auth: &AuthInfo, requested_org_id: &str) -> Result<String, Status> {
    let requested = requested_org_id.trim();
    if !requested.is_empty() && requested != auth.org_id {
        return Err(Status::permission_denied(
            "Cross-organization access is denied",
        ));
    }
    Ok(auth.org_id.clone())
}

pub fn scope_mesh_channel(org_id: &str, raw_topic: &str) -> Result<String, Status> {
    let org = org_id.trim();
    if org.is_empty() || org.eq_ignore_ascii_case("system") {
        return Err(Status::permission_denied(
            "Valid workspace context is required",
        ));
    }
    let topic = raw_topic.trim();
    if topic.is_empty() {
        return Err(Status::invalid_argument("topic/channel is required"));
    }
    if RESERVED_CONTROL_CHANNELS
        .iter()
        .any(|reserved| topic.eq_ignore_ascii_case(reserved))
    {
        return Err(Status::permission_denied(
            "Access to internal control mesh topic is denied",
        ));
    }
    let own_prefix = format!("tenant:{org}:");
    if topic.starts_with("tenant:") {
        if let Some(suffix) = topic.strip_prefix(&own_prefix) {
            if suffix.trim().is_empty() {
                return Err(Status::invalid_argument("topic/channel suffix is required"));
            }
            return Ok(topic.to_string());
        }
        return Err(Status::permission_denied(
            "Cross-tenant mesh topic access is denied",
        ));
    }
    Ok(format!("{own_prefix}{topic}"))
}

pub(crate) async fn authorize_open_meeting_participants(
    hub: &crate::hub::Hub,
    auth: &AuthInfo,
    req: &OpenMeetingRequest,
) -> Result<(), Status> {
    let meeting_id = req.meeting_id.trim();
    if meeting_id.is_empty() {
        return Err(Status::invalid_argument("meeting_id is required"));
    }
    if !req.participants.iter().any(|p| p == &auth.agent_id) {
        return Err(Status::permission_denied(
            "Caller must be a participant to open a meeting",
        ));
    }
    if hub
        .meeting_belongs_to_other_org(meeting_id, &auth.org_id)
        .await
    {
        return Err(Status::permission_denied(
            "Meeting belongs to another organization",
        ));
    }
    for participant in &req.participants {
        if let Some(agent) = hub.get_agent(participant).await
            && agent.organization_id != auth.org_id
        {
            return Err(Status::permission_denied(
                "Meeting participants must belong to caller organization",
            ));
        }
    }
    Ok(())
}

pub(crate) async fn authorize_publish_for_tenant(
    hub: &crate::hub::Hub,
    auth: &AuthInfo,
    req: &PublishMessageRequest,
) -> Result<(), Status> {
    let Some(msg) = req.message.as_ref() else {
        return Err(Status::invalid_argument("message is required"));
    };
    if auth.agent_id != msg.from_agent {
        return Err(Status::permission_denied(
            "Authenticated identity does not match from_agent",
        ));
    }
    if let Some(sender) = hub.get_agent(&msg.from_agent).await
        && sender.organization_id != auth.org_id
    {
        return Err(Status::permission_denied(
            "Sender agent belongs to another organization",
        ));
    }
    let recipient_id = msg.to_agent.trim();
    if !recipient_id.is_empty()
        && recipient_id != auth.agent_id
        && let Some(recipient) = hub.get_agent(recipient_id).await
        && recipient.organization_id != auth.org_id
    {
        return Err(Status::permission_denied(
            "Recipient agent belongs to another organization",
        ));
    }
    if !msg.meeting_id.is_empty() {
        let org_meetings = hub.get_meetings_by_org(&auth.org_id).await;
        if !org_meetings.iter().any(|m| m.id == msg.meeting_id) {
            return Err(Status::permission_denied(
                "Meeting does not belong to caller organization",
            ));
        }
    }
    Ok(())
}

pub(crate) async fn authorize_delegate_for_tenant(
    hub: &crate::hub::Hub,
    auth: &AuthInfo,
    req: &DelegateTaskRequest,
) -> Result<(), Status> {
    if auth.agent_id != req.from_agent_id {
        return Err(Status::permission_denied(
            "Authenticated identity does not match from_agent_id",
        ));
    }
    let sender = hub
        .get_agent(&req.from_agent_id)
        .await
        .ok_or_else(|| Status::permission_denied("Sender agent is not registered"))?;
    if sender.organization_id != auth.org_id {
        return Err(Status::permission_denied(
            "Sender agent belongs to another organization",
        ));
    }
    let recipient = hub
        .get_agent(&req.to_agent_id)
        .await
        .ok_or_else(|| Status::permission_denied("Recipient agent is not registered"))?;
    if recipient.organization_id != auth.org_id {
        return Err(Status::permission_denied(
            "Recipient agent belongs to another organization",
        ));
    }
    Ok(())
}

pub(crate) fn poll_tasks_for_org(
    task_manager: &crate::tasks::TaskManager,
    auth: &AuthInfo,
    requested_agent_id: &str,
    limit: usize,
) -> Result<Vec<crate::tasks::SharedTask>, Status> {
    if !requested_agent_id.trim().is_empty() && requested_agent_id.trim() != auth.agent_id {
        return Err(Status::permission_denied(
            "Cannot poll tasks on behalf of another agent",
        ));
    }
    let effective_limit = limit.min(100);
    if effective_limit == 0 {
        return Ok(Vec::new());
    }
    Ok(task_manager.claim_pending_tasks_for_org(&auth.org_id, &auth.agent_id, effective_limit))
}

pub(crate) fn update_task_status_for_org(
    task_manager: &crate::tasks::TaskManager,
    auth: &AuthInfo,
    req: &UpdateTaskStatusRequest,
) -> Result<(), Status> {
    if !req.agent_id.trim().is_empty() && req.agent_id.trim() != auth.agent_id {
        return Err(Status::permission_denied(
            "Cannot update task status as another agent",
        ));
    }
    let task = task_manager
        .get_task(&req.task_id)
        .map_err(|_| Status::not_found("Task not found"))?;
    if task.organization_id != auth.org_id {
        return Err(Status::permission_denied(
            "Task belongs to another organization",
        ));
    }
    match req.status.as_str() {
        "REVIEW" => task_manager
            .review_task(&req.task_id, &auth.agent_id)
            .map_err(Status::failed_precondition),
        "COMPLETED" => task_manager
            .complete_task(&req.task_id, &auth.agent_id, req.result.clone())
            .map_err(Status::failed_precondition),
        _ => task_manager
            .update_task_status(&req.task_id, req.status.clone())
            .map_err(Status::internal),
    }
}

pub(crate) fn authorize_invite_tenant<T>(request: &Request<T>) -> Result<String, Status> {
    let auth = require_auth_info(request)?;
    if let Some(header_val) = request.metadata().get("x-omnisolo-tenant-id") {
        let header_tenant = header_val
            .to_str()
            .map_err(|_| Status::invalid_argument("Invalid x-omnisolo-tenant-id header"))?
            .trim();
        if !header_tenant.is_empty() && header_tenant != auth.org_id {
            return Err(Status::permission_denied(
                "x-omnisolo-tenant-id header does not match authenticated organization",
            ));
        }
    }
    Ok(auth.org_id)
}

pub(crate) fn discover_agents_stream(
    hub: Arc<crate::hub::Hub>,
    auth: AuthInfo,
) -> Pin<Box<dyn Stream<Item = Result<AgentCapabilities, Status>> + Send>> {
    let rx = hub.subscribe_capabilities();
    let org_id = auth.org_id;
    let stream = futures::stream::StreamExt::filter_map(
        tokio_stream::wrappers::BroadcastStream::new(rx),
        move |item| {
            let hub = hub.clone();
            let org_id = org_id.clone();
            async move {
                match item {
                    Ok(caps) => {
                        let belongs = hub
                            .get_agent(&caps.agent_id)
                            .await
                            .is_some_and(|agent| agent.organization_id == org_id);
                        if belongs { Some(Ok(caps)) } else { None }
                    }
                    Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "discover_agents subscriber lagged");
                        None
                    }
                }
            }
        },
    );
    Box::pin(stream)
}

pub(crate) async fn save_wizard_state_in_db(
    pool: &sqlx::PgPool,
    auth: &AuthInfo,
    mut state: std::collections::HashMap<String, String>,
) -> Result<(), Status> {
    state.remove("admin_password");
    let current_step = state
        .get("step")
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0);
    let state_json = serde_json::to_value(&state).unwrap_or(serde_json::json!({}));
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
    ::server_common::auth_utils::set_org_context(&mut *tx, &auth.org_id)
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
    let row = sqlx::query(
        "SELECT state_json, current_step FROM onboarding_state WHERE tenant_id = $1 AND user_id = $2",
    )
    .bind(&auth.org_id)
    .bind(&auth.spiffe_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| Status::internal(e.to_string()))?;
    let (mut merged_state, prev_step) = if let Some(record) = row {
        (
            record
                .try_get("state_json")
                .unwrap_or_else(|_| serde_json::json!({})),
            record.try_get("current_step").unwrap_or(0),
        )
    } else {
        (serde_json::json!({}), 0)
    };
    if let (Some(existing_obj), Some(new_obj)) =
        (merged_state.as_object_mut(), state_json.as_object())
    {
        for (k, v) in new_obj {
            existing_obj.insert(k.clone(), v.clone());
        }
    } else {
        merged_state = state_json;
    }
    let new_step = std::cmp::max(prev_step, current_step);
    sqlx::query(
        "INSERT INTO onboarding_state (tenant_id, user_id, current_step, state_json, updated_at) VALUES ($1, $2, $3, $4, CURRENT_TIMESTAMP) ON CONFLICT (tenant_id, user_id) DO UPDATE SET state_json = EXCLUDED.state_json, current_step = EXCLUDED.current_step, updated_at = CURRENT_TIMESTAMP",
    )
    .bind(&auth.org_id)
    .bind(&auth.spiffe_id)
    .bind(new_step)
    .bind(&merged_state)
    .execute(&mut *tx)
    .await
    .map_err(|e| Status::internal(e.to_string()))?;
    tx.commit()
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
    Ok(())
}

pub(crate) async fn get_wizard_state_in_db(
    pool: &sqlx::PgPool,
    auth: &AuthInfo,
) -> Result<std::collections::HashMap<String, String>, Status> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
    ::server_common::auth_utils::set_org_context(&mut *tx, &auth.org_id)
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
    let row = sqlx::query(
        "SELECT state_json FROM onboarding_state WHERE tenant_id = $1 AND user_id = $2",
    )
    .bind(&auth.org_id)
    .bind(&auth.spiffe_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| Status::internal(e.to_string()))?;
    tx.commit()
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
    let mut state = std::collections::HashMap::new();
    if let Some(record) = row
        && let Some(json_obj) = record.get::<serde_json::Value, _>("state_json").as_object()
    {
        for (k, v) in json_obj {
            state.insert(
                k.clone(),
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string()),
            );
        }
    }
    Ok(state)
}

pub(crate) async fn reset_wizard_state_in_db(
    pool: &sqlx::PgPool,
    auth: &AuthInfo,
) -> Result<(), Status> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
    ::server_common::auth_utils::set_org_context(&mut *tx, &auth.org_id)
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
    sqlx::query("DELETE FROM onboarding_state WHERE tenant_id = $1 AND user_id = $2")
        .bind(&auth.org_id)
        .bind(&auth.spiffe_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
    tx.commit()
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
    Ok(())
}

pub(crate) fn map_pending_approvals(
    approvals: Vec<crate::orchestration::departments::types::ApprovalRequest>,
) -> Vec<::server_omnisolo::orchestration::SharedTask> {
    approvals
        .into_iter()
        .map(|task| {
            let mut proposed_content = String::new();
            if let Some(payload) = &task.payload {
                if let Some(draft) = payload.get("draft_copy") {
                    proposed_content = draft.as_str().unwrap_or("").to_string();
                } else if let Some(r#gen) = payload.get("generated_response") {
                    proposed_content = r#gen.as_str().unwrap_or("").to_string();
                } else if payload
                    .get("action_type")
                    .is_some_and(|v| v.as_str() == Some("DRAFT_EMAIL"))
                {
                    proposed_content = "Drafted email...".to_string();
                }
                if proposed_content.is_empty() {
                    proposed_content = serde_json::to_string(payload).unwrap_or_default();
                }
            }
            ::server_omnisolo::orchestration::SharedTask {
                id: task.id,
                organization_id: task.tenant_id,
                parent_plan_id: String::new(),
                dependencies: vec![],
                title: format!("{:?}", task.department),
                description: task.description,
                status: match task.status {
                    crate::orchestration::departments::types::ApprovalStatus::PendingApproval => {
                        "PENDING_APPROVAL".to_string()
                    }
                    crate::orchestration::departments::types::ApprovalStatus::Approved => {
                        "APPROVED".to_string()
                    }
                    crate::orchestration::departments::types::ApprovalStatus::Rejected => {
                        "REJECTED".to_string()
                    }
                    crate::orchestration::departments::types::ApprovalStatus::Paused => {
                        "PAUSED".to_string()
                    }
                },
                assigned_agent_id: String::new(),
                priority: "High".to_string(),
                payload: "{}".to_string(),
                locked_until_unix: 0,
                created_at_unix: 0,
                updated_at_unix: 0,
                action_risk: match task.action_risk {
                    crate::orchestration::departments::types::ActionRisk::AutoExecute => 1,
                    crate::orchestration::departments::types::ActionRisk::DraftForReview => 2,
                },
                approval_status: match task.status {
                    crate::orchestration::departments::types::ApprovalStatus::PendingApproval => {
                        "PENDING".to_string()
                    }
                    crate::orchestration::departments::types::ApprovalStatus::Approved => {
                        "APPROVED".to_string()
                    }
                    crate::orchestration::departments::types::ApprovalStatus::Rejected => {
                        "REJECTED".to_string()
                    }
                    crate::orchestration::departments::types::ApprovalStatus::Paused => {
                        "PAUSED".to_string()
                    }
                },
                proposed_content,
            }
        })
        .collect()
}

pub fn env_config_from_verified(
    cfg: crate::services::onboarding::env_verifier::EnvConfig,
) -> EnvConfig {
    EnvConfig {
        mode: cfg.mode,
        multi_tenant: cfg.multi_tenant,
        headless: cfg.headless,
        telemetry_enabled: cfg.telemetry_enabled,
        api_endpoint: cfg.api_endpoint,
        database_url: cfg.database_url,
    }
}

pub fn wizard_profile_response(mode: &str) -> GetWizardProfileResponse {
    let profile = match mode {
        "cloud" => Some(EnvConfig {
            mode: "cloud".to_string(),
            multi_tenant: true,
            headless: false,
            telemetry_enabled: true,
            api_endpoint: String::new(),
            database_url: "postgresql://user:pass@localhost:5432/ohc".to_string(),
        }),
        "standalone" => Some(EnvConfig {
            mode: "standalone".to_string(),
            multi_tenant: false,
            headless: false,
            telemetry_enabled: false,
            api_endpoint: String::new(),
            database_url: "sqlite://local.db".to_string(),
        }),
        _ => {
            return GetWizardProfileResponse {
                status: "error".to_string(),
                profile: None,
                error: "Invalid mode requested".to_string(),
            };
        }
    };
    GetWizardProfileResponse {
        status: "success".to_string(),
        profile,
        error: String::new(),
    }
}
