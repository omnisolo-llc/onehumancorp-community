//! Real HTTP LLM provider wire client, response parser, and artifact sanitizer.
use super::executor::TaskCenterLlmConfig;
use super::types::{MAX_ARTIFACT_CONTENT_BYTES, contains_unsafe_html};
use futures_util::StreamExt;
use server_harness::middleware::usage_ledger::TokenCounts;
use uuid::Uuid;

const MAX_PROVIDER_RESPONSE_BYTES: usize = 2 * 1024 * 1024;

pub(super) struct ProviderResponse {
    pub content: String,
    pub provider_request_id: String,
    pub counts: Option<TokenCounts>,
}

pub(super) async fn invoke_provider(
    client: &reqwest::Client,
    cfg: &TaskCenterLlmConfig,
    prompt: &str,
) -> Result<ProviderResponse, &'static str> {
    let sys = "You are Omni Assistant in the OHC Task Center. Answer concisely and truthfully based only on the provided tenant prompt. Never fabricate external actions or claim unobserved side effects.";
    let (body, req) = match cfg.provider.as_str() {
        "anthropic" => {
            let mut r = client
                .post(&cfg.endpoint)
                .header("anthropic-version", "2023-06-01");
            if let Some(k) = &cfg.api_key {
                r = r.header("x-api-key", k.expose());
            }
            (
                serde_json::json!({
                    "model": cfg.model,
                    "max_tokens": cfg.max_output_tokens,
                    "system": sys,
                    "messages": [{ "role": "user", "content": prompt }]
                }),
                r,
            )
        }
        "ollama" => (
            serde_json::json!({
                "model": cfg.model,
                "stream": false,
                "messages": [
                    { "role": "system", "content": sys },
                    { "role": "user", "content": prompt }
                ]
            }),
            client.post(&cfg.endpoint),
        ),
        _ => {
            let mut r = client.post(&cfg.endpoint);
            if let Some(k) = &cfg.api_key {
                r = r.bearer_auth(k.expose());
            }
            (
                serde_json::json!({
                    "model": cfg.model,
                    "max_tokens": cfg.max_output_tokens,
                    "temperature": 0.0,
                    "messages": [
                        { "role": "system", "content": sys },
                        { "role": "user", "content": prompt }
                    ]
                }),
                r,
            )
        }
    };
    let resp = req
        .json(&body)
        .send()
        .await
        .map_err(|_| "Provider transport failed; outcome is unknown")?;
    if !resp.status().is_success() {
        return Err("Provider returned an error status; outcome is unknown");
    }
    let mut bytes = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| "Provider stream failed; outcome is unknown")?;
        if bytes.len().saturating_add(chunk.len()) > MAX_PROVIDER_RESPONSE_BYTES {
            return Err("Provider response exceeded 2 MiB limit; outcome is unknown");
        }
        bytes.extend_from_slice(&chunk);
    }
    let json: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| "Provider returned invalid JSON; outcome is unknown")?;
    parse_provider_response(&cfg.provider, &json)
}

fn parse_provider_response(
    provider: &str,
    json: &serde_json::Value,
) -> Result<ProviderResponse, &'static str> {
    let valid_id = |v: Option<&str>| {
        v.filter(|s| !s.trim().is_empty() && s.len() <= 255)
            .map(str::to_string)
            .unwrap_or_else(|| format!("unknown:{}", Uuid::new_v4()))
    };
    match provider {
        "anthropic" => {
            if json.get("stop_reason").and_then(|v| v.as_str()) != Some("end_turn") {
                return Err("Anthropic response did not complete cleanly; outcome is unknown");
            }
            let text = json["content"]
                .as_array()
                .and_then(|a| a.iter().find_map(|b| b["text"].as_str()))
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or("Anthropic response missing text; outcome is unknown")?;
            let counts = json.get("usage").and_then(|u| {
                let raw_in = u["input_tokens"].as_i64()?;
                let cached = u["cache_read_input_tokens"].as_i64().unwrap_or(0);
                let out = u["output_tokens"].as_i64()?;
                let total_in = raw_in.checked_add(cached)?;
                (total_in > 0 && out > 0 && cached >= 0 && cached <= total_in).then_some(
                    TokenCounts {
                        input: total_in,
                        output: out,
                        cached_input: cached,
                    },
                )
            });
            Ok(ProviderResponse {
                content: text.into(),
                provider_request_id: valid_id(json["id"].as_str()),
                counts,
            })
        }
        "ollama" => {
            let text = json["message"]["content"]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or("Ollama response missing content; outcome is unknown")?;
            Ok(ProviderResponse {
                content: text.into(),
                provider_request_id: format!("ollama-{}", Uuid::new_v4()),
                counts: Some(TokenCounts {
                    input: json["prompt_eval_count"].as_i64().unwrap_or(1).max(1),
                    output: json["eval_count"].as_i64().unwrap_or(1).max(1),
                    cached_input: 0,
                }),
            })
        }
        _ => {
            let choice = json["choices"]
                .as_array()
                .and_then(|a| a.first())
                .ok_or("Provider response missing choices; outcome is unknown")?;
            if choice["finish_reason"].as_str() != Some("stop") {
                return Err("Provider completion did not finish with stop; outcome is unknown");
            }
            let text = choice["message"]["content"]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or("Provider response missing content; outcome is unknown")?;
            let counts = json.get("usage").and_then(|u| {
                let input = u["prompt_tokens"].as_i64()?;
                let output = u["completion_tokens"].as_i64()?;
                let cached = u["prompt_tokens_details"]["cached_tokens"]
                    .as_i64()
                    .unwrap_or(0);
                (input > 0 && output > 0 && cached >= 0 && cached <= input).then_some(TokenCounts {
                    input,
                    output,
                    cached_input: cached,
                })
            });
            Ok(ProviderResponse {
                content: text.into(),
                provider_request_id: valid_id(json["id"].as_str()),
                counts,
            })
        }
    }
}

pub(super) fn sanitize_artifact_content(raw: &str) -> String {
    let mut s = raw.to_string();
    if contains_unsafe_html(&s) {
        s = s
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace("javascript:", "");
    }
    if s.len() <= MAX_ARTIFACT_CONTENT_BYTES {
        return s;
    }
    let mut end = MAX_ARTIFACT_CONTENT_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
