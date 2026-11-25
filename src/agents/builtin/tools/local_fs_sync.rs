use super::{Tool, ToolExecutor, workspace_path};
use omnisolo_builtin_agent_core::types::ToolError;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use tokio::fs;

struct LocalFSSyncExecutor {
    working_dir: Option<std::path::PathBuf>,
}

#[async_trait::async_trait]
impl ToolExecutor for LocalFSSyncExecutor {
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let action = args["Action"].as_str().ok_or_else(|| {
            ToolError::LlmRecoverable("local_fs_sync: Action is required".to_string())
        })?;
        let path = args["Path"].as_str().ok_or_else(|| {
            ToolError::LlmRecoverable("local_fs_sync: Path is required".to_string())
        })?;

        if !path.starts_with(".agent-task/") || path.contains("..") || path.contains('\0') {
            return Err(ToolError::LlmRecoverable(
                "sandbox violation: path must start with .agent-task/".to_string(),
            ));
        }

        let root = workspace_path::required_root(self.working_dir.clone())?;
        let task_root = root.join(".agent-task");
        if let Ok(meta) = fs::symlink_metadata(&task_root).await
            && meta.file_type().is_symlink()
        {
            return Err(ToolError::LlmRecoverable(
                "sandbox violation: .agent-task must not be a symlink".to_string(),
            ));
        }

        match action {
            "read" => {
                let resolved = workspace_path::resolve_workspace_path(&root, path, true).await?;
                if !resolved.starts_with(&task_root) {
                    return Err(ToolError::LlmRecoverable(
                        "sandbox violation: path escapes .agent-task/".to_string(),
                    ));
                }
                let content = fs::read_to_string(&resolved)
                    .await
                    .map_err(|e| ToolError::LlmRecoverable(e.to_string()))?;
                Ok(content)
            }
            "write" => {
                let content = args["Content"].as_str().ok_or_else(|| {
                    ToolError::LlmRecoverable(
                        "local_fs_sync: Content is required for write".to_string(),
                    )
                })?;
                let resolved = workspace_path::resolve_workspace_path(&root, path, false).await?;
                if !resolved.starts_with(&task_root) {
                    return Err(ToolError::LlmRecoverable(
                        "sandbox violation: path escapes .agent-task/".to_string(),
                    ));
                }
                if let Some(parent) = resolved.parent() {
                    fs::create_dir_all(parent)
                        .await
                        .map_err(|e| ToolError::LlmRecoverable(e.to_string()))?;
                }
                let verified = workspace_path::resolve_workspace_path(&root, path, false).await?;
                if !verified.starts_with(&task_root) {
                    return Err(ToolError::LlmRecoverable(
                        "sandbox violation: path escapes .agent-task/".to_string(),
                    ));
                }
                fs::write(&verified, content)
                    .await
                    .map_err(|e| ToolError::LlmRecoverable(e.to_string()))?;
                Ok(json!({"status":"written"}).to_string())
            }
            "sync" => {
                let resolved = workspace_path::resolve_workspace_path(&root, path, true).await?;
                if !resolved.starts_with(&task_root) || !Path::new(&resolved).exists() {
                    return Err(ToolError::LlmRecoverable("file not found".to_string()));
                }
                Ok(json!({"status":"synced"}).to_string())
            }
            _ => Err(ToolError::LlmRecoverable("invalid action".to_string())),
        }
    }
}

pub fn local_fs_sync_tool(working_dir: Option<std::path::PathBuf>) -> Tool {
    Tool {
        name: "local_fs_sync".to_string(),
        description: "Performs local file system operations restricted to .agent-task/ directory."
            .to_string(),
        is_read_only: false,
        parameters: json!({
            "type": "object",
            "properties": {
                "Action": {
                    "type": "string",
                    "description": "Action to perform: read, write, sync"
                },
                "Path": {
                    "type": "string",
                    "description": "Path to the file, must start with .agent-task/"
                },
                "Content": {
                    "type": "string",
                    "description": "Content to write (required for write action)"
                }
            },
            "required": ["Action", "Path"]
        }),
        execute: Arc::new(LocalFSSyncExecutor { working_dir }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_fs_sync_enforces_working_dir_and_blocks_symlink_escape() {
        let unconfined = local_fs_sync_tool(None);
        assert!(
            unconfined
                .execute
                .execute(json!({"Action": "read", "Path": ".agent-task/a.txt"}))
                .await
                .is_err()
        );

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, "secret").await.unwrap();
        fs::create_dir_all(root.path().join(".agent-task"))
            .await
            .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, root.path().join(".agent-task/escape.txt")).unwrap();

        let tool = local_fs_sync_tool(Some(root.path().to_path_buf()));
        assert!(
            tool.execute
                .execute(json!({"Action": "read", "Path": ".agent-task/escape.txt"}))
                .await
                .is_err()
        );
        assert!(
            tool.execute
                .execute(
                    json!({"Action": "write", "Path": ".agent-task/escape.txt", "Content": "pwn"})
                )
                .await
                .is_err()
        );
        assert_eq!(fs::read_to_string(&secret).await.unwrap(), "secret");

        // Normal write and read inside .agent-task succeed
        tool.execute
            .execute(json!({"Action": "write", "Path": ".agent-task/notes.txt", "Content": "ok"}))
            .await
            .unwrap();
        let out = tool
            .execute
            .execute(json!({"Action": "read", "Path": ".agent-task/notes.txt"}))
            .await
            .unwrap();
        assert_eq!(out, "ok");
    }
}
