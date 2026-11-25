use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::super::harness::HarnessAdapterError;
use super::super::json_rpc::JsonRpcServerRequest;
use super::super::types::sanitize_credential_value;
use super::super::usage_ledger::{Admission, LedgerError, PayerMode, UsageLedger, UsageScope};
use super::classifier::{
    DecisionContext, build_escalation, build_rejection, classify_action_risk,
    extract_estimated_cost_micros, extract_raw_input, extract_tool_name, parse_options,
    ticket_digest,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcpStandingPolicy {
    pub tenant_id: String,
    pub department: String,
    pub auto_approve_max_micros: i64,
    pub allow_external_send: bool,
    pub allow_financial_commit: bool,
    pub allowed_read_tools: BTreeSet<String>,
}

impl AcpStandingPolicy {
    /// Fail-closed default standing policy: zero spend ceiling, no external
    /// sends, no financial commits, and an empty read-tool allowlist.
    pub fn fail_closed(tenant_id: impl Into<String>, department: impl Into<String>) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            department: department.into(),
            auto_approve_max_micros: 0,
            allow_external_send: false,
            allow_financial_commit: false,
            allowed_read_tools: BTreeSet::new(),
        }
    }

    pub fn validate(&self) -> Result<(), HarnessAdapterError> {
        if self.tenant_id.trim().is_empty()
            || self.tenant_id.len() > 255
            || self.tenant_id.chars().any(char::is_control)
        {
            return Err(HarnessAdapterError::InvalidRequest(
                "AcpStandingPolicy tenant_id must be a non-empty identifier".to_owned(),
            ));
        }
        if self.department.trim().is_empty()
            || self.department.len() > 128
            || self.department.chars().any(char::is_control)
        {
            return Err(HarnessAdapterError::InvalidRequest(
                "AcpStandingPolicy department must be a non-empty identifier".to_owned(),
            ));
        }
        if self.auto_approve_max_micros < 0 {
            return Err(HarnessAdapterError::InvalidRequest(
                "AcpStandingPolicy auto_approve_max_micros must be non-negative".to_owned(),
            ));
        }
        Ok(())
    }

    pub(super) fn allows_read_tool(&self, normalized_tool: &str) -> bool {
        self.allowed_read_tools
            .iter()
            .any(|allowed| allowed.trim().to_ascii_lowercase() == normalized_tool)
    }
}

#[derive(Clone)]
pub struct AcpBudgetGuard {
    pub ledger: UsageLedger,
    pub scope: UsageScope,
}

impl AcpBudgetGuard {
    pub fn new(ledger: UsageLedger, scope: UsageScope) -> Self {
        Self { ledger, scope }
    }
}

impl std::fmt::Debug for AcpBudgetGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcpBudgetGuard")
            .field("tenant_id", &self.scope.tenant_id)
            .field("task_id", &self.scope.task_id)
            .field("attempt_id", &self.scope.attempt_id)
            .field("provider", &self.scope.provider)
            .field("model", &self.scope.model)
            .field("payer", &self.scope.payer)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpActionRisk {
    ReadOnly,
    WorkspaceMutationOrExecute,
    ExternalSend,
    FinancialCommit,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcpApprovalTicket {
    pub ticket_id: Uuid,
    pub tenant_id: String,
    pub department: String,
    pub session_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub risk_class: AcpActionRisk,
    pub estimated_cost_micros: i64,
    pub payload_sha256: String,
    pub reason: String,
    pub sanitized_input: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AcpPermissionOutcome {
    AutoApproved {
        option_id: String,
        reserved_micros: i64,
        ledger_event_id: Option<String>,
        payload_sha256: String,
    },
    AwaitingApproval {
        ticket: AcpApprovalTicket,
        acp_decision: String,
        http_status: u16,
        selected_option_id: Option<String>,
    },
    Rejected {
        reason: String,
        acp_decision: String,
        http_status: u16,
        payload_sha256: String,
        selected_option_id: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AcpPermissionDecision {
    pub request_id: Value,
    pub tool_call_id: String,
    pub tool_name: String,
    pub risk_class: AcpActionRisk,
    pub estimated_cost_micros: i64,
    pub outcome: AcpPermissionOutcome,
    pub acp_response_result: Value,
}

/// Evaluates an incoming ACP `session/request_permission` request against
/// server-enforced standing authority and optional atomic `UsageLedger` guard.
pub async fn evaluate_permission_request(
    policy: &AcpStandingPolicy,
    budget_guard: Option<&AcpBudgetGuard>,
    expected_session_id: &str,
    request: &JsonRpcServerRequest,
) -> Result<AcpPermissionDecision, HarnessAdapterError> {
    policy.validate()?;
    let params = request.params.as_object().ok_or_else(|| {
        HarnessAdapterError::InvalidResponse(
            "ACP permission request params must be an object".to_owned(),
        )
    })?;
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            HarnessAdapterError::InvalidResponse(
                "ACP permission request missing sessionId".to_owned(),
            )
        })?;
    let tool_call = params
        .get("toolCall")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            HarnessAdapterError::InvalidResponse(
                "ACP permission request missing toolCall".to_owned(),
            )
        })?;
    let tool_call_id = tool_call
        .get("toolCallId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            HarnessAdapterError::InvalidResponse(
                "ACP permission request missing toolCall.toolCallId".to_owned(),
            )
        })?
        .to_owned();
    let options = parse_options(params.get("options"))?;

    let tool_name = extract_tool_name(tool_call);
    let acp_kind = tool_call
        .get("kind")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase());
    let raw_input = extract_raw_input(tool_call);
    let sanitized_input = sanitize_credential_value(&raw_input);

    if session_id != expected_session_id {
        let payload_sha256 = ticket_digest(
            policy,
            session_id,
            &tool_call_id,
            &tool_name,
            acp_kind.as_deref(),
            0,
            &raw_input,
        );
        return Ok(build_rejection(
            DecisionContext {
                request_id: request.id.as_value(),
                policy,
                session_id,
                tool_call_id,
                tool_name,
                risk_class: AcpActionRisk::Unknown,
                estimated_cost_micros: 0,
                payload_sha256,
                reject_option: options.reject_option,
            },
            "session_id_mismatch",
            403,
        ));
    }

    let estimated_cost_micros = match extract_estimated_cost_micros(tool_call, &raw_input) {
        Ok(cost) => cost,
        Err(reason) => {
            let payload_sha256 = ticket_digest(
                policy,
                session_id,
                &tool_call_id,
                &tool_name,
                acp_kind.as_deref(),
                0,
                &raw_input,
            );
            return Ok(build_rejection(
                DecisionContext {
                    request_id: request.id.as_value(),
                    policy,
                    session_id,
                    tool_call_id,
                    tool_name,
                    risk_class: AcpActionRisk::Unknown,
                    estimated_cost_micros: 0,
                    payload_sha256,
                    reject_option: options.reject_option,
                },
                reason,
                403,
            ));
        }
    };

    let payload_sha256 = ticket_digest(
        policy,
        session_id,
        &tool_call_id,
        &tool_name,
        acp_kind.as_deref(),
        estimated_cost_micros,
        &raw_input,
    );
    let risk_class = classify_action_risk(policy, &tool_name, acp_kind.as_deref(), tool_call);
    let ctx = DecisionContext {
        request_id: request.id.as_value(),
        policy,
        session_id,
        tool_call_id,
        tool_name,
        risk_class,
        estimated_cost_micros,
        payload_sha256,
        reject_option: options.reject_option,
    };

    // Step 1: Server-side standing authority risk gate (runs before any budget hold).
    let escalation_reason = match risk_class {
        AcpActionRisk::FinancialCommit if !policy.allow_financial_commit => {
            Some("financial_commit_requires_approval")
        }
        AcpActionRisk::ExternalSend if !policy.allow_external_send => {
            Some("external_send_requires_approval")
        }
        AcpActionRisk::WorkspaceMutationOrExecute => {
            Some("workspace_mutation_or_shell_requires_approval")
        }
        AcpActionRisk::Unknown => Some("tool_not_in_standing_read_allowlist"),
        AcpActionRisk::ReadOnly | AcpActionRisk::ExternalSend | AcpActionRisk::FinancialCommit => {
            None
        }
    };
    if let Some(reason) = escalation_reason {
        return Ok(build_escalation(ctx, reason, sanitized_input));
    }

    // Step 2: Hard UsageLedger headroom check & standing spend ceiling check.
    if let Err((reason, status)) =
        check_budget_headroom(policy, budget_guard, estimated_cost_micros).await
    {
        return Ok(build_rejection(ctx, reason, status));
    }
    if estimated_cost_micros > policy.auto_approve_max_micros {
        return Ok(build_escalation(
            ctx,
            "exceeds_standing_auto_approve_ceiling",
            sanitized_input,
        ));
    }

    // Step 3: Refuse blanket `allow_always`; require an `allow_once` option.
    let Some(allow_once_id) = options.allow_once else {
        return Ok(build_rejection(ctx, "no_allow_once_option_offered", 403));
    };

    // Step 4: Atomic UsageLedger reservation before returning allow_once.
    let (reserved_micros, ledger_event_id) = match reserve_action_budget(
        budget_guard,
        session_id,
        &ctx.tool_call_id,
        &ctx.payload_sha256,
        estimated_cost_micros,
    )
    .await
    {
        Ok(reserved) => reserved,
        Err((reason, status)) => return Ok(build_rejection(ctx, reason, status)),
    };

    let acp_response_result = json!({
        "outcome": {
            "outcome": "selected",
            "optionId": allow_once_id,
        },
        "_meta": {
            "ohcDecision": "auto_approved",
            "payloadSha256": ctx.payload_sha256,
            "reservedMicros": reserved_micros,
        }
    });

    Ok(AcpPermissionDecision {
        request_id: ctx.request_id,
        tool_call_id: ctx.tool_call_id,
        tool_name: ctx.tool_name,
        risk_class: ctx.risk_class,
        estimated_cost_micros: ctx.estimated_cost_micros,
        outcome: AcpPermissionOutcome::AutoApproved {
            option_id: allow_once_id,
            reserved_micros,
            ledger_event_id,
            payload_sha256: ctx.payload_sha256,
        },
        acp_response_result,
    })
}

async fn check_budget_headroom(
    policy: &AcpStandingPolicy,
    budget_guard: Option<&AcpBudgetGuard>,
    estimated_cost_micros: i64,
) -> Result<(), (&'static str, u16)> {
    let Some(guard) = budget_guard else {
        return if estimated_cost_micros > 0 {
            Err(("usage_budget_unconfigured", 409))
        } else {
            Ok(())
        };
    };
    if guard.scope.tenant_id != policy.tenant_id {
        return Err(("budget_guard_tenant_mismatch", 403));
    }
    let summary = guard
        .ledger
        .summary(&guard.scope.tenant_id)
        .await
        .map_err(|err| match err {
            LedgerError::Unconfigured => ("usage_budget_unconfigured", 409),
            _ => ("usage_ledger_unavailable", 503),
        })?;
    let remaining = summary
        .limit_micros
        .saturating_sub(summary.spent_micros)
        .saturating_sub(summary.reserved_micros);
    if summary.spent_micros > summary.limit_micros
        || summary.reserved_micros > summary.limit_micros.saturating_sub(summary.spent_micros)
        || estimated_cost_micros > remaining
    {
        return Err(("budget_exhausted", 409));
    }
    Ok(())
}

async fn reserve_action_budget(
    budget_guard: Option<&AcpBudgetGuard>,
    session_id: &str,
    tool_call_id: &str,
    payload_sha256: &str,
    estimated_cost_micros: i64,
) -> Result<(i64, Option<String>), (&'static str, u16)> {
    let Some(guard) = budget_guard else {
        return Ok((0, None));
    };
    if guard.scope.payer == PayerMode::NativeSubscription && estimated_cost_micros > 0 {
        return Err(("native_subscription_cannot_bill_managed_spend", 403));
    }
    let reserve_amount = if guard.scope.payer == PayerMode::ManagedApi {
        estimated_cost_micros
    } else {
        // BYOK and Local usage stay visible in the ledger without rebilling by OHC.
        0
    };
    let event_id = format!(
        "acp:{session_id}:{tool_call_id}:{}",
        Uuid::new_v4().simple()
    );
    match guard
        .ledger
        .reserve(&guard.scope, &event_id, payload_sha256, reserve_amount)
        .await
    {
        Ok(Admission::Admitted | Admission::Duplicate) => Ok((reserve_amount, Some(event_id))),
        Err(LedgerError::Limit) => Err(("budget_exhausted", 409)),
        Err(LedgerError::Unconfigured) => Err(("usage_budget_unconfigured", 409)),
        Err(LedgerError::Conflict) => Err(("usage_event_conflict", 409)),
        Err(_) => Err(("usage_ledger_error", 503)),
    }
}
