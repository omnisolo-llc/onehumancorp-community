use omnisolo_builtin_agent_core::types::ToolError;
use serde_json::json;

use crate::glob::glob_tool;

#[tokio::test]
async fn test_glob_success() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/glob.rs"), "fn main() {}").unwrap();

    let tool = glob_tool(Some(dir.path().to_path_buf()));

    let args = json!({
        "pattern": "**/*.rs",
        "path": "."
    });

    let result = tool
        .execute
        .execute(args)
        .await
        .expect("Execution should succeed");

    assert!(result.contains("src/glob.rs") || result.contains("glob.rs"));
}

#[tokio::test]
async fn test_glob_missing_pattern() {
    let tool = glob_tool(None);

    let args = json!({
        "path": "src/agents/builtin/tools"
    });

    let result = tool.execute.execute(args).await;

    assert!(result.is_err());

    if let Err(ToolError::LlmRecoverable(msg)) = result {
        assert!(msg.contains("Validation Error"));
    } else {
        panic!("Expected LlmRecoverable error");
    }
}

#[tokio::test]
async fn test_glob_blocks_none_working_dir_double_slash_traversal_and_symlinks() {
    let unconfined = glob_tool(None);
    assert!(
        unconfined
            .execute
            .execute(json!({"pattern": "*.rs", "path": "."}))
            .await
            .is_err()
    );

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.rs"), "secret").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        outside.path().join("secret.rs"),
        root.path().join("link.rs"),
    )
    .unwrap();

    let tool = glob_tool(Some(root.path().to_path_buf()));
    for bad in [
        json!({"pattern": "*", "path": "//etc"}),
        json!({"pattern": "*", "path": "/etc"}),
        json!({"pattern": "*", "path": ".."}),
        json!({"pattern": "../../*", "path": "."}),
        json!({"pattern": "/etc/*", "path": "."}),
    ] {
        assert!(tool.execute.execute(bad.clone()).await.is_err(), "{bad}");
    }

    let result = tool
        .execute
        .execute(json!({"pattern": "*.rs", "path": "."}))
        .await
        .unwrap();
    assert_eq!(result, "No files found.");
}
