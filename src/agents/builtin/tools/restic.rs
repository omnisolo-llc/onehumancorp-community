use omnisolo_builtin_agent_core::types::ToolError;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::{
    Tool,
    pydantic::{PydanticAdapter, PydanticToolExecutor},
    workspace_path,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResticArgs {
    action: String,
    target: Option<String>,
    snapshot_id: Option<String>,
}

struct ResticExecutor {
    working_dir: Option<PathBuf>,
    runner: Arc<dyn crate::runner::CommandRunner>,
}

/// ResticBackup is disabled by default and requires explicit standalone opt-in.
/// Checks every execution-mode environment variable case-insensitively so cloud/cluster
/// deployments cannot accidentally enable host backup operations.
pub fn restic_tools_enabled() -> bool {
    let opted_in = std::env::var("OMNISOLO_ENABLE_RESTIC_BACKUP").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    });
    if !opted_in {
        return false;
    }

    for mode_var in [
        "OMNISOLO_AGENT_EXECUTION_MODE",
        "OMNISOLO_EXECUTION_MODE",
        "OMNISOLO_SOURCE_MODE",
    ] {
        if let Ok(raw) = std::env::var(mode_var) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("standalone") {
                return false;
            }
        }
    }

    if let Ok(standalone) = std::env::var("OMNISOLO_STANDALONE_MODE") {
        let lower = standalone.trim().to_ascii_lowercase();
        if !lower.is_empty() && !matches!(lower.as_str(), "1" | "true" | "yes" | "on") {
            return false;
        }
    }

    true
}

fn is_non_standalone_mode() -> bool {
    for mode_var in [
        "OMNISOLO_AGENT_EXECUTION_MODE",
        "OMNISOLO_EXECUTION_MODE",
        "OMNISOLO_SOURCE_MODE",
    ] {
        if let Ok(raw) = std::env::var(mode_var) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("standalone") {
                return true;
            }
        }
    }
    if let Ok(standalone) = std::env::var("OMNISOLO_STANDALONE_MODE") {
        let lower = standalone.trim().to_ascii_lowercase();
        if !lower.is_empty() && !matches!(lower.as_str(), "1" | "true" | "yes" | "on") {
            return true;
        }
    }
    false
}

fn validate_snapshot_id(snapshot_id: &str) -> Result<&str, ToolError> {
    let trimmed = snapshot_id.trim();
    if trimmed == "latest" {
        return Ok(trimmed);
    }
    let valid_hex =
        (8..=64).contains(&trimmed.len()) && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit());
    if !valid_hex {
        return Err(ToolError::LlmRecoverable(
            "restic: snapshot_id must be 'latest' or an 8-64 character hexadecimal snapshot ID"
                .to_string(),
        ));
    }
    Ok(trimmed)
}

fn validate_repository(root: &Path) -> Result<String, ToolError> {
    let repo = match std::env::var("RESTIC_REPOSITORY") {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => root.join(".restic-repo").to_string_lossy().into_owned(),
    };
    if repo.starts_with('-') || repo.chars().any(char::is_control) {
        return Err(ToolError::LlmRecoverable(
            "restic: RESTIC_REPOSITORY must not start with '-' or contain control characters"
                .to_string(),
        ));
    }
    Ok(repo)
}

async fn resolve_target(
    root: &Path,
    raw_target: &str,
    must_exist: bool,
) -> Result<String, ToolError> {
    let trimmed = raw_target.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return Err(ToolError::LlmRecoverable(
            "restic: target must be a non-empty workspace path not starting with '-'".to_string(),
        ));
    }
    let resolved = workspace_path::resolve_workspace_path(root, trimmed, must_exist).await?;
    let rendered = resolved.to_string_lossy().into_owned();
    if rendered.starts_with('-') {
        return Err(ToolError::LlmRecoverable(
            "restic: resolved target path must not start with '-'".to_string(),
        ));
    }
    Ok(rendered)
}

#[async_trait::async_trait]
impl PydanticToolExecutor<ResticArgs> for ResticExecutor {
    async fn execute_typed(&self, args: ResticArgs) -> Result<String, ToolError> {
        let action = args.action.as_str();

        if is_non_standalone_mode() {
            return Err(ToolError::LlmRecoverable(
                "restic: unsupported in cloud or cluster mode".to_string(),
            ));
        }
        if !restic_tools_enabled() {
            return Err(ToolError::LlmRecoverable(
                "restic: ResticBackup is disabled by default; set OMNISOLO_ENABLE_RESTIC_BACKUP=true in standalone mode to enable".to_string(),
            ));
        }

        let password = std::env::var("RESTIC_PASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                ToolError::LlmRecoverable(
                    "RESTIC_PASSWORD environment variable is not set. \
                     Set RESTIC_PASSWORD to enable backup operations."
                        .to_string(),
                )
            })?;

        let root = workspace_path::required_root(self.working_dir.clone())?;
        let repo = validate_repository(&root)?;
        let timeout = Duration::from_secs(300);
        let env_vars = vec![("RESTIC_PASSWORD".to_string(), password)];

        if !matches!(action, "snapshot" | "restore" | "status") {
            return Err(ToolError::LlmRecoverable(
                "Invalid action. Allowed: snapshot, restore, status".to_string(),
            ));
        }

        // Validate all arguments before invoking restic init or action command.
        let action_args: Vec<String> = match action {
            "snapshot" => {
                let raw_target = args.target.as_deref().unwrap_or(".");
                let target = resolve_target(&root, raw_target, true).await?;
                vec![
                    "-r".to_string(),
                    repo.clone(),
                    "backup".to_string(),
                    "--".to_string(),
                    target,
                ]
            }
            "restore" => {
                let snapshot_id =
                    validate_snapshot_id(args.snapshot_id.as_deref().unwrap_or("latest"))?;
                let raw_target = args.target.as_deref().unwrap_or("restic-restore");
                let target = resolve_target(&root, raw_target, false).await?;
                vec![
                    "-r".to_string(),
                    repo.clone(),
                    "restore".to_string(),
                    "--target".to_string(),
                    target,
                    "--".to_string(),
                    snapshot_id.to_string(),
                ]
            }
            "status" => vec!["-r".to_string(), repo.clone(), "snapshots".to_string()],
            _ => unreachable!(),
        };

        // Initialize repo if it doesn't exist yet
        let init_args = ["-r", repo.as_str(), "init"];
        let init_res = tokio::time::timeout(
            timeout,
            self.runner
                .run("restic", &init_args, Some(&root), env_vars.clone()),
        )
        .await;
        if let Ok(Ok(output)) = init_res {
            if !output.status.success() {
                tracing::debug!("Restic init output: {:?}", output);
            }
        } else {
            tracing::warn!("Restic init failed or timed out: {:?}", init_res);
        }

        let arg_refs: Vec<&str> = action_args.iter().map(String::as_str).collect();
        let output_res = tokio::time::timeout(
            timeout,
            self.runner.run("restic", &arg_refs, Some(&root), env_vars),
        )
        .await;

        let output = output_res
            .map_err(|_| ToolError::LlmRecoverable("restic: command timed out".to_string()))?
            .map_err(|e| ToolError::LlmRecoverable(format!("restic: failed to execute: {}", e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        if !output.status.success() {
            return Err(ToolError::LlmRecoverable(format!(
                "Command failed with exit code {}\nSTDOUT: {}\nSTDERR: {}",
                output.status.code().unwrap_or(-1),
                stdout,
                stderr
            )));
        }

        Ok(format!("{} complete:\n{}", action, stdout))
    }
}

pub fn restic_tool(
    working_dir: Option<PathBuf>,
    runner: Arc<dyn crate::runner::CommandRunner>,
) -> Tool {
    Tool {
        name: "ResticBackup".to_string(),
        description: "Perform local workspace snapshotting and restores using Restic (standalone opt-in only). \
            Supported actions: snapshot, restore, status."
            .to_string(),
        is_read_only: false,
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "The action to perform: 'snapshot', 'restore', or 'status'."
                },
                "target": {
                    "type": "string",
                    "description": "Relative workspace path for 'snapshot' (default '.') or 'restore' (default 'restic-restore')."
                },
                "snapshot_id": {
                    "type": "string",
                    "description": "For 'restore', 'latest' or an 8-64 character hex snapshot ID."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        execute: Arc::new(PydanticAdapter::new(ResticExecutor {
            working_dir,
            runner,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct CapturingRunner {
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl CapturingRunner {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::runner::CommandRunner for CapturingRunner {
        async fn run(
            &self,
            _program: &str,
            args: &[&str],
            _current_dir: Option<&std::path::Path>,
            _envs: Vec<(String, String)>,
        ) -> std::io::Result<std::process::Output> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|s| s.to_string()).collect());
            Ok(std::process::Output {
                status: std::process::ExitStatus::default(),
                stdout: b"ok".to_vec(),
                stderr: b"".to_vec(),
            })
        }
    }

    #[test]
    fn restic_disabled_by_default_and_checks_all_modes_case_insensitively() {
        temp_env::with_vars(
            [
                ("OMNISOLO_ENABLE_RESTIC_BACKUP", None::<&str>),
                ("OMNISOLO_EXECUTION_MODE", Some("standalone")),
                ("OMNISOLO_AGENT_EXECUTION_MODE", None::<&str>),
                ("OMNISOLO_SOURCE_MODE", None::<&str>),
                ("OMNISOLO_STANDALONE_MODE", None::<&str>),
            ],
            || assert!(!restic_tools_enabled()),
        );

        for (var, val) in [
            ("OMNISOLO_EXECUTION_MODE", "CLOUD"),
            ("OMNISOLO_EXECUTION_MODE", "cluster"),
            ("OMNISOLO_AGENT_EXECUTION_MODE", "Cloud"),
            ("OMNISOLO_SOURCE_MODE", "CLUSTER"),
            ("OMNISOLO_STANDALONE_MODE", "false"),
        ] {
            temp_env::with_vars(
                [
                    ("OMNISOLO_ENABLE_RESTIC_BACKUP", Some("true")),
                    ("OMNISOLO_EXECUTION_MODE", None::<&str>),
                    ("OMNISOLO_AGENT_EXECUTION_MODE", None::<&str>),
                    ("OMNISOLO_SOURCE_MODE", None::<&str>),
                    ("OMNISOLO_STANDALONE_MODE", None::<&str>),
                    (var, Some(val)),
                ],
                || assert!(!restic_tools_enabled(), "enabled for {var}={val}"),
            );
        }

        temp_env::with_vars(
            [
                ("OMNISOLO_ENABLE_RESTIC_BACKUP", Some("true")),
                ("OMNISOLO_EXECUTION_MODE", Some("Standalone")),
                ("OMNISOLO_AGENT_EXECUTION_MODE", Some("standalone")),
                ("OMNISOLO_SOURCE_MODE", None::<&str>),
                ("OMNISOLO_STANDALONE_MODE", Some("true")),
            ],
            || assert!(restic_tools_enabled()),
        );
    }

    #[tokio::test]
    async fn test_missing_restic_password_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        temp_env::async_with_vars(
            vec![
                ("OMNISOLO_ENABLE_RESTIC_BACKUP", Some("true")),
                ("OMNISOLO_EXECUTION_MODE", Some("standalone")),
                ("OMNISOLO_AGENT_EXECUTION_MODE", None::<&str>),
                ("OMNISOLO_SOURCE_MODE", None::<&str>),
                ("OMNISOLO_STANDALONE_MODE", None::<&str>),
                ("RESTIC_PASSWORD", None::<&str>),
            ],
            async {
                let executor = ResticExecutor {
                    working_dir: Some(dir.path().to_path_buf()),
                    runner: Arc::new(CapturingRunner::new()),
                };
                let args = ResticArgs {
                    action: "status".to_string(),
                    target: None,
                    snapshot_id: None,
                };
                let result = executor.execute_typed(args).await;
                assert!(
                    result.is_err(),
                    "Should error when RESTIC_PASSWORD is not set"
                );
                match result.unwrap_err() {
                    ToolError::LlmRecoverable(msg) => {
                        assert!(
                            msg.contains("RESTIC_PASSWORD"),
                            "Error should mention RESTIC_PASSWORD: {}",
                            msg
                        );
                    }
                    other => panic!("Expected LlmRecoverable, got: {:?}", other),
                }
            },
        )
        .await;
    }

    #[tokio::test]
    async fn test_cloud_mode_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        temp_env::async_with_vars(
            vec![
                ("OMNISOLO_ENABLE_RESTIC_BACKUP", Some("true")),
                ("RESTIC_PASSWORD", Some("test_pass")),
                ("OMNISOLO_EXECUTION_MODE", Some("CLOUD")),
            ],
            async {
                let executor = ResticExecutor {
                    working_dir: Some(dir.path().to_path_buf()),
                    runner: Arc::new(CapturingRunner::new()),
                };
                let args = ResticArgs {
                    action: "status".to_string(),
                    target: None,
                    snapshot_id: None,
                };
                let result = executor.execute_typed(args).await;
                assert!(result.is_err(), "Should error in cloud mode");
                match result.unwrap_err() {
                    ToolError::LlmRecoverable(msg) => {
                        assert!(
                            msg.contains("cloud"),
                            "Error should mention cloud mode: {}",
                            msg
                        );
                    }
                    other => panic!("Expected LlmRecoverable, got: {:?}", other),
                }
            },
        )
        .await;
    }

    #[tokio::test]
    async fn test_restic_rejects_option_injection_and_path_escapes_and_inserts_double_dash() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("data")).unwrap();

        temp_env::async_with_vars(
            vec![
                ("OMNISOLO_ENABLE_RESTIC_BACKUP", Some("true")),
                ("OMNISOLO_EXECUTION_MODE", Some("standalone")),
                ("OMNISOLO_AGENT_EXECUTION_MODE", None::<&str>),
                ("OMNISOLO_SOURCE_MODE", None::<&str>),
                ("OMNISOLO_STANDALONE_MODE", None::<&str>),
                ("RESTIC_PASSWORD", Some("test_pass")),
                ("RESTIC_REPOSITORY", None::<&str>),
            ],
            async {
                let runner = Arc::new(CapturingRunner::new());
                let executor = ResticExecutor {
                    working_dir: Some(dir.path().to_path_buf()),
                    runner: runner.clone(),
                };

                // Option injection via target or snapshot_id must fail before invoking runner
                for bad in [
                    ResticArgs {
                        action: "snapshot".to_string(),
                        target: Some("--password-command=id".to_string()),
                        snapshot_id: None,
                    },
                    ResticArgs {
                        action: "snapshot".to_string(),
                        target: Some("/etc".to_string()),
                        snapshot_id: None,
                    },
                    ResticArgs {
                        action: "snapshot".to_string(),
                        target: Some("../outside".to_string()),
                        snapshot_id: None,
                    },
                    ResticArgs {
                        action: "restore".to_string(),
                        target: Some("/tmp/overwrite".to_string()),
                        snapshot_id: Some("latest".to_string()),
                    },
                    ResticArgs {
                        action: "restore".to_string(),
                        target: Some("restore-out".to_string()),
                        snapshot_id: Some("-o=s3.layout=x".to_string()),
                    },
                    ResticArgs {
                        action: "restore".to_string(),
                        target: Some("restore-out".to_string()),
                        snapshot_id: Some("not-hex!".to_string()),
                    },
                ] {
                    assert!(executor.execute_typed(bad).await.is_err());
                }
                assert!(runner.calls.lock().unwrap().is_empty());

                // Missing working_dir fails closed
                let unconfined = ResticExecutor {
                    working_dir: None,
                    runner: runner.clone(),
                };
                assert!(
                    unconfined
                        .execute_typed(ResticArgs {
                            action: "snapshot".to_string(),
                            target: Some(".".to_string()),
                            snapshot_id: None,
                        })
                        .await
                        .is_err()
                );

                // Valid snapshot and restore succeed and insert "--" before positional args
                executor
                    .execute_typed(ResticArgs {
                        action: "snapshot".to_string(),
                        target: Some("data".to_string()),
                        snapshot_id: None,
                    })
                    .await
                    .unwrap();
                executor
                    .execute_typed(ResticArgs {
                        action: "restore".to_string(),
                        target: Some("restore-out".to_string()),
                        snapshot_id: Some("deadbeef01234567".to_string()),
                    })
                    .await
                    .unwrap();

                let calls = runner.calls.lock().unwrap();
                let backup_call = calls
                    .iter()
                    .find(|c| c.iter().any(|a| a == "backup"))
                    .unwrap();
                assert!(
                    backup_call
                        .windows(2)
                        .any(|w| w[0] == "--" && w[1].ends_with("/data"))
                );
                let restore_call = calls
                    .iter()
                    .find(|c| c.iter().any(|a| a == "restore"))
                    .unwrap();
                assert!(
                    restore_call
                        .windows(2)
                        .any(|w| w == ["--", "deadbeef01234567"])
                );
            },
        )
        .await;
    }

    #[tokio::test]
    async fn test_restic_source_has_no_password_fallback() {
        let source = include_str!("restic.rs");
        let implementation = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(
            !implementation.contains("dummy_password"),
            "Hardcoded 'dummy_password' should have been removed"
        );
    }
}
