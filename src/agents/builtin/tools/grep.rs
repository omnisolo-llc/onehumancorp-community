use async_recursion::async_recursion;
use omnisolo_builtin_agent_core::types::ToolError;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;

use super::{
    Tool,
    pydantic::{PydanticAdapter, PydanticToolExecutor},
    workspace_path,
};

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    include: Option<String>,
}

fn default_path() -> String {
    ".".to_string()
}

struct GrepExecutor {
    working_dir: Option<std::path::PathBuf>,
}

#[async_trait::async_trait]
impl PydanticToolExecutor<GrepArgs> for GrepExecutor {
    async fn execute_typed(&self, args: GrepArgs) -> Result<String, ToolError> {
        let pattern = &args.pattern;
        let path = &args.path;
        let case_insensitive = args.case_insensitive;
        let include_pattern = args.include;

        let re = if case_insensitive {
            Regex::new(&format!("(?i){}", pattern))
        } else {
            Regex::new(pattern)
        }
        .map_err(|e| ToolError::LlmRecoverable(format!("grep: invalid regex: {}", e)))?;

        let root = workspace_path::required_root(self.working_dir.clone())?;
        let actual_path = workspace_path::resolve_workspace_path(&root, path, true).await?;

        let mut results = Vec::new();
        let meta = tokio::fs::symlink_metadata(&actual_path)
            .await
            .map_err(|e| ToolError::LlmRecoverable(format!("grep: {}: {}", path, e)))?;
        if meta.file_type().is_symlink() {
            return Err(ToolError::LlmRecoverable(
                "grep: symlink targets are not allowed".to_string(),
            ));
        }

        if meta.is_file() {
            search_file(
                &root,
                &actual_path,
                &re,
                include_pattern.as_deref(),
                &mut results,
            )
            .await
            .map_err(|e| ToolError::LlmRecoverable(e.to_string()))?;
        } else {
            search_directory(
                &root,
                &actual_path,
                &re,
                include_pattern.as_deref(),
                &mut results,
            )
            .await
            .map_err(|e| ToolError::LlmRecoverable(e.to_string()))?;
        }

        if results.is_empty() {
            return Ok("No matches found.".to_string());
        }

        if results.len() > 100 {
            results.truncate(100);
            results.push("... (truncated to 100 results to save context. Please refine your regex pattern, specify a more restrictive include filter, or use glob/find to narrow the search.)".to_string());
        }

        Ok(results.join("\n"))
    }
}

async fn search_file(
    canonical_root: &Path,
    path: &Path,
    re: &Regex,
    include: Option<&str>,
    results: &mut Vec<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if let Some(inc) = include
        && !matches_include(name, inc)
    {
        return Ok(());
    }
    let display_path = path.strip_prefix(canonical_root).unwrap_or(path);
    use tokio::io::{AsyncBufReadExt, BufReader};
    if let Ok(file) = tokio::fs::File::open(path).await {
        let mut reader = BufReader::new(file);
        let mut line_buffer = String::new();
        let mut line_num = 1;

        while let Ok(bytes) = reader.read_line(&mut line_buffer).await {
            if bytes == 0 {
                break;
            }
            if re.is_match(&line_buffer) {
                results.push(format!(
                    "{}:{}:{}",
                    display_path.display(),
                    line_num,
                    line_buffer.trim_end_matches('\n').trim_end_matches('\r')
                ));
                if results.len() >= 100 {
                    return Ok(());
                }
            }
            line_num += 1;
            line_buffer.clear();
        }
    }
    Ok(())
}

#[async_recursion]
async fn search_directory(
    canonical_root: &Path,
    dir: &Path,
    re: &Regex,
    include: Option<&str>,
    results: &mut Vec<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| ToolError::LlmRecoverable(e.to_string()))?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let Ok(meta) = tokio::fs::symlink_metadata(&path).await else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let Ok(canonical_entry) = tokio::fs::canonicalize(&path).await else {
            continue;
        };
        if !canonical_entry.starts_with(canonical_root) {
            continue;
        }

        if meta.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            search_directory(canonical_root, &canonical_entry, re, include, results)
                .await
                .map_err(|e| ToolError::LlmRecoverable(e.to_string()))?;
            if results.len() >= 100 {
                return Ok(());
            }
        } else if meta.is_file() {
            search_file(canonical_root, &canonical_entry, re, include, results).await?;
            if results.len() >= 100 {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn matches_include(filename: &str, include: &str) -> bool {
    if let Some(ext) = include.strip_prefix("*.") {
        if ext.starts_with('{') && ext.ends_with('}') {
            let exts = &ext[1..ext.len() - 1];
            for e in exts.split(',') {
                if filename.ends_with(&format!(".{}", e.trim())) {
                    return true;
                }
            }
            return false;
        }
        return filename.ends_with(&format!(".{}", ext));
    }
    filename.contains(include)
}

pub fn grep_tool(working_dir: Option<std::path::PathBuf>) -> Tool {
    Tool {
        name: "Grep".to_string(),
        description: "Search for a regex pattern in files under a directory. Returns file:line:content matches. Used for Context Management (Preventing Context Rot): JetBrains JIT Retrieval (grep, glob).".to_string(),
        is_read_only: true,
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regular expression pattern to search for."
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search (default '.')."
                },
                "include": {
                    "type": "string",
                    "description": "File extension filter (e.g. '*.go', '*.rs')."
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Case-insensitive search."
                }
            },
            "required": ["pattern"]
        }),
        execute: Arc::new(PydanticAdapter::new(GrepExecutor { working_dir })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn test_grep_large_file_streaming() {
        let test_dir = tempfile::tempdir().unwrap();
        let test_file = test_dir.path().join("large_log.txt");

        let mut file = std::fs::File::create(&test_file).unwrap();
        for i in 1..=5000 {
            if i == 4500 {
                writeln!(file, "Error: critical failure found here!").unwrap();
            } else {
                writeln!(file, "Line {}", i).unwrap();
            }
        }

        let executor = PydanticAdapter::new(GrepExecutor {
            working_dir: Some(test_dir.path().to_path_buf()),
        });

        let args = json!({
            "pattern": "critical failure",
            "path": ".",
        });

        let result = crate::ToolExecutor::execute(&executor, args).await.unwrap();
        assert!(result.contains("critical failure found here!"));
        assert!(result.contains(":4500:"));
    }

    #[tokio::test]
    async fn test_grep_blocks_traversal_absolute_symlinks_and_missing_working_dir() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret_file = outside.path().join("secret.txt");
        std::fs::write(&secret_file, "TOP_SECRET_TOKEN\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret_file, root.path().join("leak.txt")).unwrap();

        let unconfined = grep_tool(None);
        assert!(
            unconfined
                .execute
                .execute(json!({"pattern": "root", "path": "."}))
                .await
                .is_err()
        );

        let tool = grep_tool(Some(root.path().to_path_buf()));
        for bad_path in ["../..", "/etc", "//etc", "leak.txt"] {
            assert!(
                tool.execute
                    .execute(json!({"pattern": "TOP_SECRET_TOKEN", "path": bad_path}))
                    .await
                    .is_err(),
                "grep should reject {bad_path}"
            );
        }

        // Recursive search from "." must skip symlinks pointing outside workspace
        let scan = tool
            .execute
            .execute(json!({"pattern": "TOP_SECRET_TOKEN", "path": "."}))
            .await
            .unwrap();
        assert_eq!(scan, "No matches found.");
    }
}

#[tokio::test]
async fn test_grep_tool_missing_args() {
    let tool = grep_tool(None);

    let invalid_args = json!({});
    let res = crate::ToolExecutor::execute(&*tool.execute, invalid_args).await;

    assert!(res.is_err());
    let err_msg = res.unwrap_err().to_string();
    assert!(err_msg.contains("Validation Error (Pydantic-first tool schema)"));
}

#[tokio::test]
async fn test_grep_basic_match() {
    let test_dir = tempfile::tempdir().unwrap();
    let test_file = test_dir.path().join("test.txt");
    std::fs::write(&test_file, "hello\nworld\nthis is a match\n").unwrap();

    let executor = PydanticAdapter::new(GrepExecutor {
        working_dir: Some(test_dir.path().to_path_buf()),
    });

    let args = json!({
        "pattern": "is a match",
        "path": ".",
    });

    let result = crate::ToolExecutor::execute(&executor, args).await.unwrap();
    assert!(result.contains("is a match"));
    assert!(result.contains("test.txt"));
}
