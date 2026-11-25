#![allow(clippy::comparison_to_empty)]
use omnisolo_builtin_agent_core::types::ToolError;
use serde::Deserialize;
use serde_json::json;
use std::path::{Component, Path};
use std::sync::Arc;

use super::{
    Tool,
    pydantic::{PydanticAdapter, PydanticToolExecutor},
    workspace_path,
};

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
}

fn default_path() -> String {
    ".".to_string()
}

struct GlobExecutor {
    working_dir: Option<std::path::PathBuf>,
}

fn validate_glob_pattern(pattern: &str) -> Result<&str, ToolError> {
    let trimmed = pattern.trim();
    if trimmed.is_empty()
        || trimmed.contains('\0')
        || trimmed.starts_with('/')
        || trimmed.starts_with('\\')
    {
        return Err(ToolError::LlmRecoverable(
            "glob: pattern must be a non-empty relative glob without leading '/'".to_string(),
        ));
    }
    for component in Path::new(trimmed).components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(ToolError::LlmRecoverable(
                "glob: path traversal via '..' and absolute patterns are not allowed".to_string(),
            ));
        }
    }
    Ok(trimmed)
}

#[async_trait::async_trait]
impl PydanticToolExecutor<GlobArgs> for GlobExecutor {
    async fn execute_typed(&self, args: GlobArgs) -> Result<String, ToolError> {
        let root = workspace_path::required_root(self.working_dir.clone())?;
        let base_dir = workspace_path::resolve_workspace_path(&root, &args.path, true).await?;
        let safe_pattern = validate_glob_pattern(&args.pattern)?;

        let full_pattern = format!("{}/{}", base_dir.display(), safe_pattern);

        let matches: Vec<String> = glob::glob(&full_pattern)
            .map_err(|e| ToolError::LlmRecoverable(format!("glob: invalid pattern: {}", e)))?
            .filter_map(|r| r.ok())
            .filter(|p| workspace_path::is_contained_non_symlink_sync(&root, p))
            .filter_map(|p| {
                let canon = std::fs::canonicalize(&p).ok()?;
                canon
                    .strip_prefix(&root)
                    .ok()
                    .map(|rel| rel.display().to_string())
            })
            .collect();

        if matches.is_empty() {
            return Ok("No files found.".to_string());
        }

        let mut output_matches = matches;
        if output_matches.len() > 50 {
            output_matches.truncate(50);
            output_matches.push("... (truncated to 50 results. Please use a more specific glob pattern or use grep/find.)".to_string());
        }

        Ok(output_matches.join("\n"))
    }
}

pub fn glob_tool(working_dir: Option<std::path::PathBuf>) -> Tool {
    Tool {
        name: "Glob".to_string(),
        description: "Find files matching a glob pattern. Returns newline-separated paths. Used for Context Management (Preventing Context Rot): JetBrains JIT Retrieval (grep, glob).".to_string(),
        is_read_only: true,
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern to match (e.g. '**/*.go', 'src/*.rs')."
                },
                "path": {
                    "type": "string",
                    "description": "Base directory to search from (default '.')."
                }
            },
            "required": ["pattern"]
        }),
        execute: Arc::new(PydanticAdapter::new(GlobExecutor { working_dir })),
    }
}
