use super::{
    Tool, network_policy,
    pydantic::{PydanticAdapter, PydanticToolExecutor},
};
use omnisolo_builtin_agent_core::types::ToolError;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

const MAX_OLLAMA_RESPONSE_BYTES: usize = 1_048_576;
const OLLAMA_HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OllamaArgs {
    action: String,
    model_name: Option<String>,
}

struct OllamaExecutor;

fn parse_operator_base_url() -> Result<Url, ToolError> {
    let raw = std::env::var("OLLAMA_BASE_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ToolError::LlmRecoverable(
                "ollama: OLLAMA_BASE_URL is not configured by the operator".to_string(),
            )
        })?;
    let parsed = Url::parse(&raw)
        .map_err(|e| ToolError::LlmRecoverable(format!("ollama: invalid OLLAMA_BASE_URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ToolError::LlmRecoverable(
            "ollama: OLLAMA_BASE_URL must use http or https".to_string(),
        ));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ToolError::LlmRecoverable(
            "ollama: OLLAMA_BASE_URL must not contain credentials".to_string(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ToolError::LlmRecoverable(
            "ollama: OLLAMA_BASE_URL must not contain query or fragment".to_string(),
        ));
    }
    if !parsed.path().is_empty() && parsed.path() != "/" {
        return Err(ToolError::LlmRecoverable(
            "ollama: OLLAMA_BASE_URL must not contain a custom path".to_string(),
        ));
    }
    Ok(parsed)
}

fn validate_model_name(model_name: &str) -> Result<&str, ToolError> {
    let trimmed = model_name.trim();
    if trimmed.is_empty()
        || trimmed.len() > 256
        || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(ToolError::LlmRecoverable(
            "ollama: model_name must be a non-empty identifier without whitespace or control characters".to_string(),
        ));
    }
    Ok(trimmed)
}

async fn read_bounded_body(mut resp: reqwest::Response) -> Result<Vec<u8>, ToolError> {
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| ToolError::LlmRecoverable(format!("ollama: failed reading response: {e}")))?
    {
        if chunk.len() > MAX_OLLAMA_RESPONSE_BYTES.saturating_sub(body.len()) {
            return Err(ToolError::LlmRecoverable(
                "ollama: response exceeds 1 MiB limit".to_string(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[async_trait::async_trait]
impl PydanticToolExecutor<OllamaArgs> for OllamaExecutor {
    async fn execute_typed(&self, args: OllamaArgs) -> Result<String, ToolError> {
        let action = args.action.as_str();
        if !matches!(action, "list_models" | "pull_model" | "check_health") {
            return Err(ToolError::LlmRecoverable("invalid action".to_string()));
        }

        let base_url = parse_operator_base_url()?;
        let allow_private = network_policy::private_network_allowed();
        let addresses = network_policy::validate_and_resolve(&base_url, allow_private).await?;
        let client = network_policy::pin_resolved_addresses(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .timeout(OLLAMA_HTTP_TIMEOUT),
            &base_url,
            &addresses,
        )
        .build()
        .map_err(|e| ToolError::Unexpected(format!("ollama: failed to build HTTP client: {e}")))?;

        match action {
            "list_models" => {
                let endpoint = base_url
                    .join("api/tags")
                    .map_err(|e| ToolError::LlmRecoverable(format!("ollama: invalid URL: {e}")))?;
                let resp = client
                    .get(endpoint)
                    .send()
                    .await
                    .map_err(|e| ToolError::LlmRecoverable(e.to_string()))?;
                if !resp.status().is_success() {
                    return Err(ToolError::LlmRecoverable(format!(
                        "ollama returned status {}",
                        resp.status()
                    )));
                }
                let bytes = read_bounded_body(resp).await?;
                let result: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| ToolError::LlmRecoverable(e.to_string()))?;
                Ok(result.to_string())
            }
            "pull_model" => {
                let model_name =
                    validate_model_name(args.model_name.as_deref().ok_or_else(|| {
                        ToolError::LlmRecoverable(
                            "ollama: model_name is required for pull".to_string(),
                        )
                    })?)?;
                let endpoint = base_url
                    .join("api/pull")
                    .map_err(|e| ToolError::LlmRecoverable(format!("ollama: invalid URL: {e}")))?;
                let payload = json!({
                    "name": model_name,
                    "stream": false,
                });
                let resp = client
                    .post(endpoint)
                    .json(&payload)
                    .send()
                    .await
                    .map_err(|e| ToolError::LlmRecoverable(e.to_string()))?;
                if !resp.status().is_success() {
                    return Err(ToolError::LlmRecoverable(format!(
                        "ollama returned status {}",
                        resp.status()
                    )));
                }
                let _ = read_bounded_body(resp).await?;
                Ok(json!({"status":"pulled"}).to_string())
            }
            "check_health" => {
                let model_name =
                    validate_model_name(args.model_name.as_deref().ok_or_else(|| {
                        ToolError::LlmRecoverable(
                            "ollama: model_name is required for health check".to_string(),
                        )
                    })?)?;
                let endpoint = base_url
                    .join("api/generate")
                    .map_err(|e| ToolError::LlmRecoverable(format!("ollama: invalid URL: {e}")))?;
                let payload = json!({
                    "model": model_name,
                    "prompt": "Hello",
                    "stream": false,
                });
                let resp = client
                    .post(endpoint)
                    .json(&payload)
                    .send()
                    .await
                    .map_err(|e| ToolError::LlmRecoverable(e.to_string()))?;
                if resp.status().is_success() {
                    let _ = read_bounded_body(resp).await?;
                    Ok(json!({"status":"healthy"}).to_string())
                } else {
                    Err(ToolError::LlmRecoverable(format!(
                        "health check failed with status {}",
                        resp.status()
                    )))
                }
            }
            _ => unreachable!(),
        }
    }
}

pub fn ollama_tool() -> Tool {
    Tool {
        name: "ollama".to_string(),
        description: "Manage operator-configured Ollama instances (list, pull, check health)."
            .to_string(),
        is_read_only: false,
        parameters: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Action to perform: list_models, pull_model, check_health"
                },
                "model_name": {
                    "type": "string",
                    "description": "Model name (required for pull_model and check_health)"
                }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        execute: Arc::new(PydanticAdapter::new(OllamaExecutor)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn ollama_tool_is_mutating_and_does_not_expose_url_parameter() {
        let tool = ollama_tool();
        assert!(!tool.is_read_only);
        let props = tool.parameters["properties"].as_object().unwrap();
        assert!(!props.contains_key("url"));
    }

    #[tokio::test]
    async fn test_ollama_pydantic_adapter_missing_action() {
        let tool = ollama_tool();
        let args = json!({});
        let result = tool.execute.execute(args).await;

        assert!(result.is_err());
        if let Err(ToolError::LlmRecoverable(msg)) = result {
            assert!(msg.contains("Validation Error (Pydantic-first tool schema)"));
            assert!(msg.contains("missing field `action`"));
        } else {
            panic!("Expected LlmRecoverable error for missing required action");
        }
    }

    #[tokio::test]
    async fn test_ollama_pydantic_adapter_invalid_type() {
        let tool = ollama_tool();
        let args = json!({ "action": 123 });
        let result = tool.execute.execute(args).await;

        assert!(result.is_err());
        if let Err(ToolError::LlmRecoverable(msg)) = result {
            assert!(msg.contains("Validation Error (Pydantic-first tool schema)"));
        } else {
            panic!("Expected LlmRecoverable error for invalid type");
        }
    }

    #[tokio::test]
    async fn test_ollama_rejects_caller_supplied_url_field() {
        let tool = ollama_tool();
        let args = json!({
            "action": "list_models",
            "url": "http://169.254.169.254/latest/meta-data/?"
        });
        let result = tool.execute.execute(args).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_ollama_executor_invalid_action() {
        let tool = ollama_tool();
        let args = json!({ "action": "invalid_action" });
        let result = tool.execute.execute(args).await;

        assert!(result.is_err());
        if let Err(ToolError::LlmRecoverable(msg)) = result {
            assert_eq!(msg, "invalid action");
        } else {
            panic!("Expected LlmRecoverable error for invalid action");
        }
    }

    #[tokio::test]
    async fn test_ollama_blocks_private_and_malformed_operator_urls() {
        let tool = ollama_tool();
        for bad_url in [
            None,
            Some("http://169.254.169.254"),
            Some("http://127.0.0.1:11434"),
            Some("https://user:pass@example.com"),
            Some("https://example.com/?x=1"),
            Some("https://example.com/#frag"),
            Some("https://example.com/custom/path"),
            Some("file:///etc/passwd"),
        ] {
            temp_env::async_with_vars(
                [
                    ("OLLAMA_BASE_URL", bad_url),
                    ("OMNISOLO_AGENT_ALLOW_PRIVATE_NETWORK", Some("false")),
                ],
                async {
                    let res = tool.execute.execute(json!({"action": "list_models"})).await;
                    assert!(res.is_err(), "accepted bad OLLAMA_BASE_URL: {bad_url:?}");
                },
            )
            .await;
        }
    }

    #[tokio::test]
    async fn test_ollama_loopback_enforces_no_redirect_and_response_size_cap() {
        // 1. Valid response succeeds when private network explicitly allowed
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ok_server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut req = [0u8; 2048];
            let _ = sock.read(&mut req).await;
            let body = r#"{"models":[{"name":"llama3"}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });

        let tool = ollama_tool();
        let base_url = format!("http://{addr}");
        temp_env::async_with_vars(
            [
                ("OLLAMA_BASE_URL", Some(base_url.as_str())),
                ("OMNISOLO_AGENT_ALLOW_PRIVATE_NETWORK", Some("true")),
            ],
            async {
                let res = tool
                    .execute
                    .execute(json!({"action": "list_models"}))
                    .await
                    .unwrap();
                assert!(res.contains("llama3"));
            },
        )
        .await;
        ok_server.await.unwrap();

        // 2. Redirect response is rejected (Policy::none)
        let redir_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redir_addr = redir_listener.local_addr().unwrap();
        let redir_server = tokio::spawn(async move {
            let (mut sock, _) = redir_listener.accept().await.unwrap();
            let mut req = [0u8; 2048];
            let _ = sock.read(&mut req).await;
            let resp = "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            sock.write_all(resp.as_bytes()).await.unwrap();
        });
        let redir_url = format!("http://{redir_addr}");
        temp_env::async_with_vars(
            [
                ("OLLAMA_BASE_URL", Some(redir_url.as_str())),
                ("OMNISOLO_AGENT_ALLOW_PRIVATE_NETWORK", Some("true")),
            ],
            async {
                let err = tool
                    .execute
                    .execute(json!({"action": "list_models"}))
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("302"));
            },
        )
        .await;
        redir_server.await.unwrap();

        // 3. Oversized (> 1 MiB) response is rejected
        let big_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let big_addr = big_listener.local_addr().unwrap();
        let big_server = tokio::spawn(async move {
            let (mut sock, _) = big_listener.accept().await.unwrap();
            let mut req = [0u8; 2048];
            let _ = sock.read(&mut req).await;
            let oversized = vec![b'a'; MAX_OLLAMA_RESPONSE_BYTES + 128];
            let hdr = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                oversized.len()
            );
            let _ = sock.write_all(hdr.as_bytes()).await;
            let _ = sock.write_all(&oversized).await;
        });
        let big_url = format!("http://{big_addr}");
        temp_env::async_with_vars(
            [
                ("OLLAMA_BASE_URL", Some(big_url.as_str())),
                ("OMNISOLO_AGENT_ALLOW_PRIVATE_NETWORK", Some("true")),
            ],
            async {
                let err = tool
                    .execute
                    .execute(json!({"action": "list_models"}))
                    .await
                    .unwrap_err();
                assert!(err.to_string().contains("1 MiB"));
            },
        )
        .await;
        big_server.await.unwrap();
    }
}
