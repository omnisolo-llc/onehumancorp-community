#![allow(clippy::all)]
use omnisolo_builtin_agent_core::types::{
    HumanInLoopSpectrum, PermissionArchitecture, ToolCall, ToolError,
};
use sha2::{Digest, Sha256};

fn canonicalize_json_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let mut ordered = serde_json::Map::with_capacity(entries.len());
            for (k, v) in entries {
                ordered.insert(k.clone(), canonicalize_json_value(v));
            }
            serde_json::Value::Object(ordered)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize_json_value).collect())
        }
        _ => value.clone(),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// HumanInLoopManager implements the SOTA Harness Patterns (2025-2026): 5. Human-in-loop as spectrum -> not binary autonomy vs control.
pub struct HumanInLoopManager;

impl HumanInLoopManager {
    /// Computes the canonical lowercase hex SHA-256 digest of a tool call's JSON arguments.
    pub fn canonical_args_sha256(arguments: &serde_json::Value) -> String {
        let canonical = canonicalize_json_value(arguments);
        let serialized = serde_json::to_vec(&canonical).unwrap_or_default();
        let digest = Sha256::digest(&serialized);
        hex::encode(digest)
    }

    /// Computes an unambiguous cryptographic approval binding token over
    /// `(tool_call_id, tool_name, canonical_args)`.
    /// Bare `tool_call_id` approvals and delimiter-collision spoofing (e.g. `:` in MCP tool names)
    /// are rejected to prevent replay across different tools or mutated arguments.
    pub fn approval_binding_token(
        tool_call_id: &str,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> String {
        let canonical_args = canonicalize_json_value(arguments);
        let encoded =
            serde_json::to_vec(&(tool_call_id, tool_name, &canonical_args)).unwrap_or_default();
        let digest = hex::encode(Sha256::digest(&encoded));
        format!("v1:{digest}")
    }

    /// Evaluates if a given tool call requires human intervention based on the
    /// 5-point Human-in-the-Loop spectrum and current confidence/trust thresholds.
    pub fn evaluate_escalation_tier(
        tc: &ToolCall,
        is_read_only: bool,
        is_high_risk: bool,
        hil_spectrum: &HumanInLoopSpectrum,
        confidence: f32,
        confidence_threshold: f32,
        permission_architecture: &PermissionArchitecture,
        approved_tool_calls: &[String],
        manually_approved_tool_calls: &[String],
    ) -> Result<(), ToolError> {
        let expected_token = Self::approval_binding_token(&tc.id, &tc.name, &tc.arguments);
        let expected_bytes = expected_token.as_bytes();
        let is_approved = approved_tool_calls
            .iter()
            .chain(manually_approved_tool_calls.iter())
            .any(|candidate| constant_time_eq(candidate.as_bytes(), expected_bytes));

        if is_approved {
            return Ok(());
        }

        // High-risk tools always require explicit confirmation (UserFixable)
        if is_high_risk {
            return Err(ToolError::UserFixable(format!(
                "High-risk tool '{}' requires explicit user confirmation. Approve this tool call to proceed.",
                tc.name
            )));
        }

        // Master Catalog C.5. Permission Architecture Check
        if permission_architecture == &PermissionArchitecture::Restrictive && !is_read_only {
            return Err(ToolError::UserFixable(format!(
                "Mutating tool '{}' requires human approval due to restrictive base architecture.",
                tc.name
            )));
        }

        match hil_spectrum {
            HumanInLoopSpectrum::Autonomous => {
                // Fully autonomous. No human intervention needed unless high-risk (handled above).
                Ok(())
            }
            HumanInLoopSpectrum::ApprovalOnMutate => {
                // Classic "Restrictive" mode. Requires approval only for mutating tools.
                if !is_read_only {
                    Err(ToolError::UserFixable(format!(
                        "Mutating tool '{}' requires human approval.",
                        tc.name
                    )))
                } else {
                    Ok(())
                }
            }
            HumanInLoopSpectrum::ApprovalOnAll => {
                // Requires explicit human approval before ANY tool executes.
                Err(ToolError::UserFixable(format!(
                    "Tool '{}' requires explicit user confirmation under 'ApprovalOnAll' mode.",
                    tc.name
                )))
            }
            HumanInLoopSpectrum::CollaborativeEdit => {
                // Expects the human to actively review and edit tool arguments.
                Err(ToolError::UserFixable(format!(
                    "Collaborative Edit required for tool '{}'. Please review and optionally edit the tool arguments to proceed.",
                    tc.name
                )))
            }
            HumanInLoopSpectrum::Supervisory => {
                // Triggers human intervention ONLY if confidence is below the threshold.
                if confidence < confidence_threshold {
                    Err(ToolError::UserFixable(format!(
                        "Low confidence ({:.2} < threshold {:.2}) for tool '{}'. Human supervision required.",
                        confidence, confidence_threshold, tc.name
                    )))
                } else {
                    // Fall back to autonomous if confidence is sufficient
                    // Note: Restrictive architecture is already checked above.
                    Ok(())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_tool_call(name: &str) -> ToolCall {
        ToolCall {
            id: "test_id".to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({}),
        }
    }

    #[test]
    fn test_high_risk_always_requires_approval() {
        let tc = create_tool_call("delete_database");
        let result = HumanInLoopManager::evaluate_escalation_tier(
            &tc,
            false, // is_read_only
            true,  // is_high_risk
            &HumanInLoopSpectrum::Autonomous,
            1.0,
            0.5,
            &PermissionArchitecture::Permissive,
            &[],
            &[],
        );

        assert!(matches!(result, Err(ToolError::UserFixable(_))));
        if let Err(ToolError::UserFixable(msg)) = result {
            assert!(msg.contains("High-risk tool"));
        }
    }

    #[test]
    fn test_autonomous_mode() {
        let tc = create_tool_call("read_file");
        let result = HumanInLoopManager::evaluate_escalation_tier(
            &tc,
            false, // is_read_only (Wait, false means it's mutating, but permissive allows it in autonomous)
            false,
            &HumanInLoopSpectrum::Autonomous,
            1.0,
            0.5,
            &PermissionArchitecture::Permissive,
            &[],
            &[],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_restrictive_architecture_overrides_autonomous() {
        let tc_mutate = create_tool_call("write_file");
        let result = HumanInLoopManager::evaluate_escalation_tier(
            &tc_mutate,
            false, // is_read_only
            false,
            &HumanInLoopSpectrum::Autonomous,
            1.0,
            0.5,
            &PermissionArchitecture::Restrictive,
            &[],
            &[],
        );
        assert!(matches!(result, Err(ToolError::UserFixable(_))));
        if let Err(ToolError::UserFixable(msg)) = result {
            assert!(msg.contains("restrictive base architecture"));
        }

        // Even with Restrictive, read_only is OK
        let tc_read = create_tool_call("read_file");
        let result_read = HumanInLoopManager::evaluate_escalation_tier(
            &tc_read,
            true, // is_read_only
            false,
            &HumanInLoopSpectrum::Autonomous,
            1.0,
            0.5,
            &PermissionArchitecture::Restrictive,
            &[],
            &[],
        );
        assert!(result_read.is_ok());
    }

    #[test]
    fn test_approval_on_mutate() {
        let tc_mutate = create_tool_call("write_file");
        let tc_read = create_tool_call("read_file");

        let result_mutate = HumanInLoopManager::evaluate_escalation_tier(
            &tc_mutate,
            false, // not read only
            false,
            &HumanInLoopSpectrum::ApprovalOnMutate,
            1.0,
            0.5,
            &PermissionArchitecture::Permissive,
            &[],
            &[],
        );
        assert!(matches!(result_mutate, Err(ToolError::UserFixable(_))));

        let result_read = HumanInLoopManager::evaluate_escalation_tier(
            &tc_read,
            true, // read only
            false,
            &HumanInLoopSpectrum::ApprovalOnMutate,
            1.0,
            0.5,
            &PermissionArchitecture::Permissive,
            &[],
            &[],
        );
        assert!(result_read.is_ok());
    }

    #[test]
    fn test_approval_on_all() {
        let tc = create_tool_call("read_file");
        let result = HumanInLoopManager::evaluate_escalation_tier(
            &tc,
            true, // read only, shouldn't matter
            false,
            &HumanInLoopSpectrum::ApprovalOnAll,
            1.0,
            0.5,
            &PermissionArchitecture::Permissive,
            &[],
            &[],
        );
        assert!(matches!(result, Err(ToolError::UserFixable(_))));
    }

    #[test]
    fn test_approval_on_mutate_read_only() {
        let tc = create_tool_call("read_file");
        let result = HumanInLoopManager::evaluate_escalation_tier(
            &tc,
            true, // is_read_only
            false,
            &HumanInLoopSpectrum::ApprovalOnMutate,
            1.0,
            0.5,
            &PermissionArchitecture::Permissive,
            &[],
            &[],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_manually_approved_tool_calls_bypass_escalation() {
        let tc = create_tool_call("delete_database");
        let manually_approved_calls = vec![HumanInLoopManager::approval_binding_token(
            &tc.id,
            &tc.name,
            &tc.arguments,
        )];

        let result = HumanInLoopManager::evaluate_escalation_tier(
            &tc,
            false,
            true,
            &HumanInLoopSpectrum::Autonomous,
            1.0,
            0.5,
            &PermissionArchitecture::Permissive,
            &[],
            &manually_approved_calls,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn test_supervisory_mode_autonomous_fallback() {
        let tc = create_tool_call("read_file");
        let result = HumanInLoopManager::evaluate_escalation_tier(
            &tc,
            false, // is_read_only = false
            false, // is_high_risk = false
            &HumanInLoopSpectrum::Supervisory,
            0.9,                                 // high confidence
            0.5,                                 // low threshold
            &PermissionArchitecture::Permissive, // NOT Restrictive -> fallback to Autonomous OK
            &[],
            &[],
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_collaborative_edit() {
        let tc = create_tool_call("read_file");
        let result = HumanInLoopManager::evaluate_escalation_tier(
            &tc,
            true,
            false,
            &HumanInLoopSpectrum::CollaborativeEdit,
            1.0,
            0.5,
            &PermissionArchitecture::Permissive,
            &[],
            &[],
        );
        assert!(matches!(result, Err(ToolError::UserFixable(_))));
        if let Err(ToolError::UserFixable(msg)) = result {
            assert!(msg.contains("Collaborative Edit required"));
        }
    }

    #[test]
    fn test_supervisory_mode() {
        let tc = create_tool_call("read_file");

        // High confidence -> OK
        let result_high_conf = HumanInLoopManager::evaluate_escalation_tier(
            &tc,
            true,
            false,
            &HumanInLoopSpectrum::Supervisory,
            0.9,
            0.8, // threshold
            &PermissionArchitecture::Permissive,
            &[],
            &[],
        );
        assert!(result_high_conf.is_ok());

        // Low confidence -> UserFixable
        let result_low_conf = HumanInLoopManager::evaluate_escalation_tier(
            &tc,
            true,
            false,
            &HumanInLoopSpectrum::Supervisory,
            0.4,
            0.8, // threshold
            &PermissionArchitecture::Permissive,
            &[],
            &[],
        );
        assert!(matches!(result_low_conf, Err(ToolError::UserFixable(_))));
        if let Err(ToolError::UserFixable(msg)) = result_low_conf {
            assert!(msg.contains("Low confidence"));
        }

        // High confidence but restrictive architecture -> UserFixable for mutating tools
        let tc_mutate = create_tool_call("write_file");
        let result_restrictive = HumanInLoopManager::evaluate_escalation_tier(
            &tc_mutate,
            false, // not read only
            false,
            &HumanInLoopSpectrum::Supervisory,
            0.9,
            0.8, // threshold
            &PermissionArchitecture::Restrictive,
            &[],
            &[],
        );
        assert!(matches!(result_restrictive, Err(ToolError::UserFixable(_))));
    }

    #[test]
    fn test_approved_tool_calls_bypass_escalation() {
        let tc = create_tool_call("delete_database"); // High risk
        let approved_calls = vec![HumanInLoopManager::approval_binding_token(
            &tc.id,
            &tc.name,
            &tc.arguments,
        )];

        let result = HumanInLoopManager::evaluate_escalation_tier(
            &tc,
            false,
            true, // is_high_risk
            &HumanInLoopSpectrum::Autonomous,
            1.0,
            0.5,
            &PermissionArchitecture::Permissive,
            &approved_calls,
            &[],
        );

        assert!(result.is_ok()); // Should bypass because it's approved with bound digest
    }

    #[test]
    fn test_approval_rejects_bare_id_or_mismatched_tool_or_mutated_args() {
        let approved_tc = ToolCall {
            id: "call_42".to_string(),
            name: "Bash".to_string(),
            arguments: serde_json::json!({"command": "ls", "timeout": 10}),
        };
        let bound_token = HumanInLoopManager::approval_binding_token(
            &approved_tc.id,
            &approved_tc.name,
            &approved_tc.arguments,
        );

        // Bare tool_call_id must NOT be accepted
        let bare_res = HumanInLoopManager::evaluate_escalation_tier(
            &approved_tc,
            false,
            true,
            &HumanInLoopSpectrum::ApprovalOnMutate,
            1.0,
            0.5,
            &PermissionArchitecture::Restrictive,
            &["call_42".to_string()],
            &[],
        );
        assert!(matches!(bare_res, Err(ToolError::UserFixable(_))));

        // Reordered keys with identical canonical semantics MUST match
        let reordered_tc = ToolCall {
            id: "call_42".to_string(),
            name: "Bash".to_string(),
            arguments: serde_json::json!({"timeout": 10, "command": "ls"}),
        };
        assert!(
            HumanInLoopManager::evaluate_escalation_tier(
                &reordered_tc,
                false,
                true,
                &HumanInLoopSpectrum::ApprovalOnMutate,
                1.0,
                0.5,
                &PermissionArchitecture::Restrictive,
                &[bound_token.clone()],
                &[],
            )
            .is_ok()
        );

        // Mutated arguments with same call_id & tool_name must be rejected
        let mutated_args_tc = ToolCall {
            id: "call_42".to_string(),
            name: "Bash".to_string(),
            arguments: serde_json::json!({"command": "rm -rf /", "timeout": 10}),
        };
        assert!(
            HumanInLoopManager::evaluate_escalation_tier(
                &mutated_args_tc,
                false,
                true,
                &HumanInLoopSpectrum::ApprovalOnMutate,
                1.0,
                0.5,
                &PermissionArchitecture::Restrictive,
                &[bound_token.clone()],
                &[],
            )
            .is_err()
        );

        // Different tool_name with same call_id & arguments must be rejected
        let swapped_tool_tc = ToolCall {
            id: "call_42".to_string(),
            name: "ResticBackup".to_string(),
            arguments: approved_tc.arguments.clone(),
        };
        assert!(
            HumanInLoopManager::evaluate_escalation_tier(
                &swapped_tool_tc,
                false,
                true,
                &HumanInLoopSpectrum::ApprovalOnMutate,
                1.0,
                0.5,
                &PermissionArchitecture::Restrictive,
                &[bound_token],
                &[],
            )
            .is_err()
        );

        // Delimiter collision test: ("a", "b:c") must NOT collide with ("a:b", "c")
        let tok_a = HumanInLoopManager::approval_binding_token("a", "b:c", &serde_json::json!({}));
        let tok_b = HumanInLoopManager::approval_binding_token("a:b", "c", &serde_json::json!({}));
        assert_ne!(tok_a, tok_b);
        let colon_tc = ToolCall {
            id: "a:b".to_string(),
            name: "c".to_string(),
            arguments: serde_json::json!({}),
        };
        assert!(
            HumanInLoopManager::evaluate_escalation_tier(
                &colon_tc,
                false,
                true,
                &HumanInLoopSpectrum::ApprovalOnMutate,
                1.0,
                0.5,
                &PermissionArchitecture::Restrictive,
                &[tok_a],
                &[],
            )
            .is_err()
        );
    }
}
