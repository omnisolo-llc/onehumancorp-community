use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::super::harness::HarnessAdapterError;
use super::policy::{
    AcpActionRisk, AcpApprovalTicket, AcpPermissionDecision, AcpPermissionOutcome,
    AcpStandingPolicy,
};

const FINANCIAL_TOOL_KEYWORDS: &[&str] = &[
    "financial_commit",
    "financial",
    "charge_card",
    "charge_customer",
    "stripe_charge",
    "stripe_refund",
    "stripe_payout",
    "refund",
    "payout",
    "transfer_funds",
    "wire_transfer",
    "send_invoice",
    "pay_invoice",
    "billing_commit",
    "place_order",
    "purchase",
    "approve_spend",
    "payment",
];

const EXTERNAL_SEND_KEYWORDS: &[&str] = &[
    "send_email",
    "send_sms",
    "send_message",
    "post_webhook",
    "publish",
    "slack_send",
    "slack_post",
    "twilio_send",
    "email_send",
    "dispatch_outreach",
    "notify_customer",
    "tweet",
    "post_social",
];

const EXTERNAL_SEND_PREFIXES: &[&str] =
    &["send_", "post_", "publish_", "email_", "sms_", "notify_"];

const MUTATION_OR_EXEC_KEYWORDS: &[&str] = &[
    "bash",
    "sh",
    "zsh",
    "shell",
    "exec",
    "execute",
    "run_command",
    "terminal",
    "write_file",
    "edit_file",
    "delete_file",
    "patch_file",
    "apply_patch",
    "rm",
    "mv",
    "chmod",
    "chown",
];

const MUTATION_OR_EXEC_PREFIXES: &[&str] = &[
    "write_", "edit_", "delete_", "remove_", "exec_", "run_", "shell_", "mutate_",
];

pub(super) struct ParsedOptions {
    pub(super) allow_once: Option<String>,
    pub(super) reject_option: Option<String>,
}

pub(super) struct DecisionContext<'a> {
    pub(super) request_id: Value,
    pub(super) policy: &'a AcpStandingPolicy,
    pub(super) session_id: &'a str,
    pub(super) tool_call_id: String,
    pub(super) tool_name: String,
    pub(super) risk_class: AcpActionRisk,
    pub(super) estimated_cost_micros: i64,
    pub(super) payload_sha256: String,
    pub(super) reject_option: Option<String>,
}

/// Computes a canonical SHA-256 hex digest over any JSON value by sorting all
/// object keys recursively before serialization.
pub fn canonical_payload_sha256(value: &Value) -> String {
    let canonical = canonicalize_value(value);
    let encoded = serde_json::to_vec(&canonical).expect("canonical JSON value serializes");
    format!("{:x}", Sha256::digest(&encoded))
}

fn canonicalize_value(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, Value> = map
                .iter()
                .map(|(k, v)| (k, canonicalize_value(v)))
                .collect();
            let mut out = Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k.clone(), v);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize_value).collect()),
        _ => value.clone(),
    }
}

pub(super) fn parse_options(value: Option<&Value>) -> Result<ParsedOptions, HarnessAdapterError> {
    let options = value.and_then(Value::as_array).ok_or_else(|| {
        HarnessAdapterError::InvalidResponse("ACP permission request missing options".to_owned())
    })?;
    let mut allow_once = None;
    let mut reject_once = None;
    let mut reject_always = None;
    for option in options {
        let Some(obj) = option.as_object() else {
            continue;
        };
        let Some(option_id) = obj.get("optionId").and_then(Value::as_str) else {
            continue;
        };
        let Some(kind) = obj.get("kind").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "allow_once" if allow_once.is_none() => allow_once = Some(option_id.to_owned()),
            "reject_once" if reject_once.is_none() => reject_once = Some(option_id.to_owned()),
            "reject_always" if reject_always.is_none() => {
                reject_always = Some(option_id.to_owned())
            }
            _ => {}
        }
    }
    Ok(ParsedOptions {
        allow_once,
        reject_option: reject_once.or(reject_always),
    })
}

pub(super) fn extract_tool_name(tool_call: &Map<String, Value>) -> String {
    let candidate = tool_call
        .get("toolName")
        .or_else(|| tool_call.get("tool_name"))
        .or_else(|| tool_call.get("name"))
        .or_else(|| tool_call.get("_meta").and_then(|m| m.get("toolName")))
        .or_else(|| tool_call.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("unknown_tool");
    candidate.trim().to_ascii_lowercase()
}

pub(super) fn extract_raw_input(tool_call: &Map<String, Value>) -> Value {
    tool_call
        .get("rawInput")
        .or_else(|| tool_call.get("raw_input"))
        .or_else(|| tool_call.get("arguments"))
        .or_else(|| tool_call.get("input"))
        .cloned()
        .unwrap_or_else(|| json!({}))
}

pub(super) fn extract_estimated_cost_micros(
    tool_call: &Map<String, Value>,
    raw_input: &Value,
) -> Result<i64, &'static str> {
    let candidate = tool_call
        .get("estimatedCostMicros")
        .or_else(|| tool_call.get("estimated_cost_micros"))
        .or_else(|| tool_call.get("costMicros"))
        .or_else(|| tool_call.get("cost_micros"))
        .or_else(|| {
            tool_call
                .get("_meta")
                .and_then(|meta| meta.get("estimatedCostMicros"))
        })
        .or_else(|| raw_input.get("estimated_cost_micros"))
        .or_else(|| raw_input.get("estimatedCostMicros"))
        .or_else(|| raw_input.get("cost_micros"))
        .or_else(|| raw_input.get("amount_micros"));

    let Some(value) = candidate else {
        return Ok(0);
    };
    if value.is_null() {
        return Ok(0);
    }
    let Some(micros) = value.as_i64() else {
        return Err("invalid_estimated_cost_micros");
    };
    if micros < 0 {
        return Err("negative_estimated_cost_micros");
    }
    Ok(micros)
}

pub(super) fn classify_action_risk(
    policy: &AcpStandingPolicy,
    tool_name: &str,
    acp_kind: Option<&str>,
    tool_call: &Map<String, Value>,
) -> AcpActionRisk {
    let explicit_category = tool_call
        .get("riskClass")
        .or_else(|| tool_call.get("category"))
        .or_else(|| tool_call.get("_meta").and_then(|m| m.get("riskClass")))
        .and_then(Value::as_str)
        .map(|s| s.trim().to_ascii_lowercase());

    if explicit_category.as_deref() == Some("financial_commit")
        || FINANCIAL_TOOL_KEYWORDS
            .iter()
            .any(|kw| tool_name == *kw || tool_name.contains(kw))
    {
        return AcpActionRisk::FinancialCommit;
    }

    if explicit_category.as_deref() == Some("external_send")
        || acp_kind == Some("fetch")
        || EXTERNAL_SEND_KEYWORDS
            .iter()
            .any(|kw| tool_name == *kw || tool_name.contains(kw))
        || EXTERNAL_SEND_PREFIXES
            .iter()
            .any(|prefix| tool_name.starts_with(prefix))
    {
        return AcpActionRisk::ExternalSend;
    }

    if matches!(
        acp_kind,
        Some("edit" | "delete" | "move" | "execute" | "other")
    ) || MUTATION_OR_EXEC_KEYWORDS.contains(&tool_name)
        || MUTATION_OR_EXEC_PREFIXES
            .iter()
            .any(|prefix| tool_name.starts_with(prefix))
    {
        return AcpActionRisk::WorkspaceMutationOrExecute;
    }

    if matches!(acp_kind, None | Some("read" | "search" | "think"))
        && policy.allows_read_tool(tool_name)
    {
        return AcpActionRisk::ReadOnly;
    }

    AcpActionRisk::Unknown
}

pub(super) fn ticket_digest(
    policy: &AcpStandingPolicy,
    session_id: &str,
    tool_call_id: &str,
    tool_name: &str,
    acp_kind: Option<&str>,
    estimated_cost_micros: i64,
    raw_input: &Value,
) -> String {
    canonical_payload_sha256(&json!({
        "tenant_id": policy.tenant_id,
        "department": policy.department,
        "session_id": session_id,
        "tool_call_id": tool_call_id,
        "tool_name": tool_name,
        "acp_kind": acp_kind,
        "estimated_cost_micros": estimated_cost_micros,
        "raw_input": raw_input,
    }))
}

pub(super) fn build_escalation(
    ctx: DecisionContext<'_>,
    reason: &str,
    sanitized_input: Value,
) -> AcpPermissionDecision {
    let ticket_id = Uuid::new_v4();
    let acp_response_result = build_deny_wire_result(
        ctx.reject_option.as_deref(),
        "awaiting_approval",
        403,
        reason,
        &ctx.payload_sha256,
        Some(ticket_id),
    );
    let ticket = AcpApprovalTicket {
        ticket_id,
        tenant_id: ctx.policy.tenant_id.clone(),
        department: ctx.policy.department.clone(),
        session_id: ctx.session_id.to_owned(),
        tool_call_id: ctx.tool_call_id.clone(),
        tool_name: ctx.tool_name.clone(),
        risk_class: ctx.risk_class,
        estimated_cost_micros: ctx.estimated_cost_micros,
        payload_sha256: ctx.payload_sha256,
        reason: reason.to_owned(),
        sanitized_input,
    };
    AcpPermissionDecision {
        request_id: ctx.request_id,
        tool_call_id: ctx.tool_call_id,
        tool_name: ctx.tool_name,
        risk_class: ctx.risk_class,
        estimated_cost_micros: ctx.estimated_cost_micros,
        outcome: AcpPermissionOutcome::AwaitingApproval {
            ticket,
            acp_decision: "awaiting_approval".to_owned(),
            http_status: 403,
            selected_option_id: ctx.reject_option,
        },
        acp_response_result,
    }
}

pub(super) fn build_rejection(
    ctx: DecisionContext<'_>,
    reason: &str,
    http_status: u16,
) -> AcpPermissionDecision {
    let acp_response_result = build_deny_wire_result(
        ctx.reject_option.as_deref(),
        "reject",
        http_status,
        reason,
        &ctx.payload_sha256,
        None,
    );
    AcpPermissionDecision {
        request_id: ctx.request_id,
        tool_call_id: ctx.tool_call_id,
        tool_name: ctx.tool_name,
        risk_class: ctx.risk_class,
        estimated_cost_micros: ctx.estimated_cost_micros,
        outcome: AcpPermissionOutcome::Rejected {
            reason: reason.to_owned(),
            acp_decision: "reject".to_owned(),
            http_status,
            payload_sha256: ctx.payload_sha256,
            selected_option_id: ctx.reject_option,
        },
        acp_response_result,
    }
}

fn build_deny_wire_result(
    reject_option: Option<&str>,
    ohc_decision: &str,
    http_status: u16,
    reason: &str,
    payload_sha256: &str,
    ticket_id: Option<Uuid>,
) -> Value {
    let outcome = match reject_option {
        Some(option_id) => json!({ "outcome": "selected", "optionId": option_id }),
        None => json!({ "outcome": "cancelled" }),
    };
    let mut meta = Map::new();
    meta.insert(
        "ohcDecision".to_owned(),
        Value::String(ohc_decision.to_owned()),
    );
    meta.insert("httpStatus".to_owned(), Value::from(http_status));
    meta.insert("reason".to_owned(), Value::String(reason.to_owned()));
    meta.insert(
        "payloadSha256".to_owned(),
        Value::String(payload_sha256.to_owned()),
    );
    if let Some(id) = ticket_id {
        meta.insert("ticketId".to_owned(), Value::String(id.to_string()));
    }
    json!({
        "outcome": outcome,
        "_meta": Value::Object(meta),
    })
}
