use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use futures_util::StreamExt;
use reqwest::Url;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::sentinel::{
    ResponseLeakScanner, Sentinel, SentinelError, SsrfGuard, is_explicit_loopback_host,
};

use super::types::{ModelApiDialect, ResolvedModelSelection};
use super::usage_meter::{RequestMeter, UsageCapture, UsageMeterSettings};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Clone)]
struct ProviderFacadeState {
    client: reqwest::Client,
    request_timeout: Duration,
    upstream_base_url: Url,
    is_anthropic: bool,
    anthropic_version: String,
    model_id: String,
    reasoning_effort: Option<Value>,
    max_output_tokens: Option<u64>,
    revoked: watch::Receiver<bool>,
    external_revoked: Option<watch::Receiver<bool>>,
    inference_allowed: bool,
    surrogate_token: String,
    tenant_id: String,
    session_id: String,
    sentinel: Sentinel,
    meter: Option<Arc<RequestMeter>>,
}

#[derive(Clone)]
pub struct ProviderFacadeConfig {
    pub upstream_url: String,
    pub upstream_api_key: String,
    pub selection: ResolvedModelSelection,
    pub request_timeout: Duration,
    pub inference_allowed: bool,
    pub metering: Option<UsageMeterSettings>,
    pub tenant_id: Option<String>,
    pub session_id: Option<String>,
    pub allowed_upstream_hosts: Option<Vec<String>>,
    pub sentinel: Option<Sentinel>,
    pub ssrf_guard: Option<SsrfGuard>,
    pub leak_scanner: Option<ResponseLeakScanner>,
    pub revocation_rx: Option<watch::Receiver<bool>>,
}

impl ProviderFacadeConfig {
    pub fn new(
        upstream_url: impl Into<String>,
        upstream_api_key: impl Into<String>,
        selection: ResolvedModelSelection,
    ) -> Self {
        Self {
            upstream_url: upstream_url.into(),
            upstream_api_key: upstream_api_key.into(),
            selection,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            inference_allowed: true,
            metering: None,
            tenant_id: None,
            session_id: None,
            allowed_upstream_hosts: None,
            sentinel: None,
            ssrf_guard: None,
            leak_scanner: None,
            revocation_rx: None,
        }
    }

    pub fn with_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    pub fn with_tenant_session(
        mut self,
        tenant_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        self.tenant_id = Some(tenant_id.into());
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_allowed_upstream_hosts(mut self, hosts: Vec<String>) -> Self {
        self.allowed_upstream_hosts = Some(hosts);
        self
    }

    pub fn with_sentinel(mut self, sentinel: Sentinel) -> Self {
        self.sentinel = Some(sentinel);
        self
    }

    pub fn with_ssrf_guard(mut self, ssrf_guard: SsrfGuard) -> Self {
        self.ssrf_guard = Some(ssrf_guard);
        self
    }

    pub fn with_leak_scanner(mut self, leak_scanner: ResponseLeakScanner) -> Self {
        self.leak_scanner = Some(leak_scanner);
        self
    }

    pub fn with_revocation(mut self, revocation_rx: watch::Receiver<bool>) -> Self {
        self.revocation_rx = Some(revocation_rx);
        self
    }
}

impl std::fmt::Debug for ProviderFacadeConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderFacadeConfig")
            .field("upstream_url", &"<configured>")
            .field("upstream_api_key", &"<configured>")
            .field("model_id", &self.selection.model_id)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderFacadeRoute {
    base_url: String,
    token: String,
}

impl std::fmt::Debug for ProviderFacadeRoute {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderFacadeRoute")
            .field("base_url", &self.base_url)
            .field("token", &"<scoped>")
            .finish()
    }
}

impl ProviderFacadeRoute {
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

#[derive(Debug)]
pub enum ProviderFacadeError {
    InvalidConfiguration(String),
    Bind(String),
    Server(String),
}

impl std::fmt::Display for ProviderFacadeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => {
                write!(
                    formatter,
                    "invalid provider facade configuration: {message}"
                )
            }
            Self::Bind(message) => write!(formatter, "provider facade bind failed: {message}"),
            Self::Server(message) => write!(formatter, "provider facade server failed: {message}"),
        }
    }
}

impl std::error::Error for ProviderFacadeError {}

pub struct ProviderFacade {
    route: ProviderFacadeRoute,
    revoke: watch::Sender<bool>,
    sentinel: Sentinel,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), std::io::Error>>>,
}

impl std::fmt::Debug for ProviderFacade {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderFacade")
            .field("route", &self.route)
            .field("running", &self.task.is_some())
            .finish()
    }
}

impl ProviderFacade {
    pub async fn start(
        upstream_url: impl AsRef<str>,
        upstream_api_key: impl Into<String>,
        selection: ResolvedModelSelection,
    ) -> Result<Self, ProviderFacadeError> {
        Self::start_with_config(ProviderFacadeConfig::new(
            upstream_url.as_ref().to_owned(),
            upstream_api_key,
            selection,
        ))
        .await
    }

    pub async fn start_with_config(
        config: ProviderFacadeConfig,
    ) -> Result<Self, ProviderFacadeError> {
        let upstream_base_url = parse_upstream_url(&config.upstream_url)?;
        if config.upstream_api_key.trim().is_empty()
            || config.upstream_api_key.chars().any(char::is_control)
        {
            return Err(ProviderFacadeError::InvalidConfiguration(
                "upstream API key must be non-empty and contain no control characters".to_owned(),
            ));
        }
        if config.selection.model_id.trim().is_empty()
            || config.selection.model_id.chars().any(char::is_control)
        {
            return Err(ProviderFacadeError::InvalidConfiguration(
                "resolved model id must be non-empty and contain no control characters".to_owned(),
            ));
        }
        if config.request_timeout.is_zero() {
            return Err(ProviderFacadeError::InvalidConfiguration(
                "request timeout must be greater than zero".to_owned(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| {
                ProviderFacadeError::Server(redact(&error.to_string(), &config.upstream_api_key))
            })?;
        let meter = match &config.metering {
            Some(settings) if config.inference_allowed => {
                Some(settings.connect().await.map_err(|error| {
                    ProviderFacadeError::InvalidConfiguration(error.to_string())
                })?)
            }
            _ => None,
        };

        let upstream_host = upstream_base_url
            .host_str()
            .map(|h| h.trim_matches(|c| c == '[' || c == ']').to_owned())
            .ok_or_else(|| {
                ProviderFacadeError::InvalidConfiguration("upstream URL missing host".to_owned())
            })?;
        let tenant_id = config
            .tenant_id
            .clone()
            .or_else(|| config.metering.as_ref().map(|m| m.scope.tenant_id.clone()))
            .unwrap_or_else(|| "default-tenant".to_owned());
        let session_id = config
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let provider_id = if config.selection.provider_route.trim().is_empty() {
            "provider".to_owned()
        } else {
            config.selection.provider_route.clone()
        };

        let broker = config
            .sentinel
            .as_ref()
            .map(|s| s.broker.clone())
            .unwrap_or_default();

        let ssrf_guard = if let Some(guard) = config.ssrf_guard {
            guard
        } else if let Some(sentinel) = &config.sentinel {
            sentinel.ssrf_guard.clone()
        } else {
            let allowlist = config
                .allowed_upstream_hosts
                .clone()
                .unwrap_or_else(|| vec![upstream_host.clone()]);
            let mut guard = SsrfGuard::new(allowlist);
            if config.allowed_upstream_hosts.is_none() && is_explicit_loopback_host(&upstream_host)
            {
                guard = guard.with_loopback_allowed_for_tests(true);
            }
            guard
        };

        let leak_scanner = config
            .leak_scanner
            .or_else(|| config.sentinel.as_ref().map(|s| s.leak_scanner.clone()))
            .unwrap_or_default();

        let sentinel = Sentinel::new(broker, ssrf_guard, leak_scanner);

        let (revoke, revoked) = watch::channel(false);
        if config.revocation_rx.as_ref().is_some_and(|rx| *rx.borrow()) {
            revoke.send_replace(true);
        }

        let surrogate_hosts = config.allowed_upstream_hosts.clone().unwrap_or_else(|| {
            let guard_hosts = sentinel.ssrf_guard.allowed_domains();
            if guard_hosts.is_empty() {
                vec![upstream_host.clone()]
            } else {
                guard_hosts.to_vec()
            }
        });
        let broker_revoked_rx = config
            .revocation_rx
            .clone()
            .unwrap_or_else(|| revoked.clone());
        let surrogate_token = sentinel
            .broker
            .issue(
                tenant_id.clone(),
                session_id.clone(),
                provider_id,
                config.upstream_api_key,
                surrogate_hosts,
                broker_revoked_rx,
            )
            .map_err(|error| ProviderFacadeError::InvalidConfiguration(error.to_string()))?;

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| ProviderFacadeError::Bind(error.to_string()))?;
        let address = listener
            .local_addr()
            .map_err(|error| ProviderFacadeError::Bind(error.to_string()))?;
        let route = ProviderFacadeRoute {
            base_url: format!("http://{address}/v1"),
            token: surrogate_token.clone(),
        };
        let is_anthropic = upstream_base_url.host_str() == Some("api.anthropic.com")
            || config.selection.api_dialect == ModelApiDialect::AnthropicMessages
            || config
                .selection
                .provider_route
                .eq_ignore_ascii_case("anthropic_api")
            || config
                .selection
                .provider_route
                .eq_ignore_ascii_case("anthropic")
            || config.metering.as_ref().is_some_and(|m| {
                m.scope.provider.eq_ignore_ascii_case("anthropic_api")
                    || m.scope.provider.eq_ignore_ascii_case("anthropic")
            });
        let anthropic_version = std::env::var("OMNISOLO_ANTHROPIC_VERSION")
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| valid_anthropic_version(v))
            .unwrap_or_else(|| DEFAULT_ANTHROPIC_VERSION.to_owned());
        let state = ProviderFacadeState {
            client,
            request_timeout: config.request_timeout,
            upstream_base_url,
            is_anthropic,
            anthropic_version,
            model_id: config.selection.model_id,
            max_output_tokens: config.selection.max_output_tokens,
            reasoning_effort: config.selection.reasoning_effort.map(|effort| {
                serde_json::to_value(effort).expect("reasoning effort serializes as a string")
            }),
            revoked,
            external_revoked: config.revocation_rx,
            inference_allowed: config.inference_allowed,
            surrogate_token,
            tenant_id,
            session_id,
            sentinel: sentinel.clone(),
            meter,
        };
        let app = Router::new()
            .route("/v1/models", get(models))
            .route("/v1/responses", post(responses))
            .route("/v1/chat/completions", post(chat_completions))
            .route("/v1/messages", post(messages))
            .with_state(state);
        let (shutdown, mut shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = (&mut shutdown_receiver).await;
                })
                .await
        });
        Ok(Self {
            route,
            revoke,
            sentinel,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    pub fn route(&self) -> &ProviderFacadeRoute {
        &self.route
    }

    pub fn sentinel(&self) -> &Sentinel {
        &self.sentinel
    }

    pub fn revocation(&self) -> watch::Receiver<bool> {
        self.revoke.subscribe()
    }

    pub fn revoke(&self) {
        self.revoke.send_replace(true);
        self.sentinel.broker.revoke_token(self.route.token());
    }

    pub async fn shutdown(mut self) -> Result<(), ProviderFacadeError> {
        self.revoke();
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            let result = task
                .await
                .map_err(|error| ProviderFacadeError::Server(error.to_string()))?;
            result.map_err(|error| ProviderFacadeError::Server(error.to_string()))?;
        }
        Ok(())
    }
}

impl Drop for ProviderFacade {
    fn drop(&mut self) {
        self.revoke();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn models(State(state): State<ProviderFacadeState>, headers: HeaderMap) -> Response {
    let presented = match authorize_surrogate_header(&state, &headers) {
        Ok(tok) => tok,
        Err(resp) => return resp,
    };
    let version_override = requested_anthropic_version(&headers);
    forward(
        &state,
        &presented,
        reqwest::Method::GET,
        "models",
        None,
        version_override.as_deref(),
    )
    .await
}

async fn responses(
    State(state): State<ProviderFacadeState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    json_proxy(&state, headers, body, "responses").await
}

async fn chat_completions(
    State(state): State<ProviderFacadeState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    json_proxy(&state, headers, body, "chat/completions").await
}

async fn messages(
    State(state): State<ProviderFacadeState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    json_proxy(&state, headers, body, "messages").await
}

async fn json_proxy(
    state: &ProviderFacadeState,
    headers: HeaderMap,
    body: Bytes,
    path: &str,
) -> Response {
    let presented = match authorize_surrogate_header(state, &headers) {
        Ok(tok) => tok,
        Err(resp) => return resp,
    };
    if !state.inference_allowed {
        return error_response(
            StatusCode::FORBIDDEN,
            "provider inference requires an active attempt",
        );
    }
    if state.is_anthropic && path != "messages" {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Anthropic upstream requires the /v1/messages endpoint",
        );
    }
    if !state.is_anthropic && path == "messages" {
        return error_response(
            StatusCode::BAD_REQUEST,
            "/v1/messages requires an Anthropic upstream configuration",
        );
    }
    let mut payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return error_response(StatusCode::BAD_REQUEST, "request body must be JSON"),
    };
    let Some(model) = payload.get("model").and_then(Value::as_str) else {
        return error_response(StatusCode::BAD_REQUEST, "request model is required");
    };
    if model != state.model_id {
        return error_response(StatusCode::BAD_REQUEST, "request model is not admitted");
    }
    if let Err(message) = bind_reasoning(&mut payload, path, state.reasoning_effort.as_ref()) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }
    if let Err(message) = bind_output_limit(&mut payload, path, state.max_output_tokens) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }
    if let Err(message) = bind_stream_options(&mut payload, path) {
        return error_response(StatusCode::BAD_REQUEST, message);
    }
    let version_override = requested_anthropic_version(&headers);
    let body = Bytes::from(serde_json::to_vec(&payload).expect("JSON value serializes"));
    forward(
        state,
        &presented,
        reqwest::Method::POST,
        path,
        Some(body),
        version_override.as_deref(),
    )
    .await
}

fn bind_stream_options(payload: &mut Value, path: &str) -> Result<(), &'static str> {
    if path != "chat/completions" {
        return Ok(());
    }
    let object = payload
        .as_object_mut()
        .ok_or("request body must be an object")?;
    if let Some(stream) = object.get("stream").and_then(Value::as_bool)
        && stream
    {
        let options = object
            .entry("stream_options")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(options_obj) = options.as_object_mut() {
            options_obj.insert("include_usage".to_owned(), Value::Bool(true));
        } else {
            return Err("stream_options must be an object");
        }
    }
    Ok(())
}

fn is_revoked(state: &ProviderFacadeState) -> bool {
    *state.revoked.borrow()
        || state
            .external_revoked
            .as_ref()
            .is_some_and(|rx| *rx.borrow())
}

fn valid_anthropic_version(value: &str) -> bool {
    value.len() >= 4
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn requested_anthropic_version(headers: &HeaderMap) -> Option<String> {
    headers
        .get("anthropic-version")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| valid_anthropic_version(value))
        .map(ToOwned::to_owned)
}

#[allow(clippy::result_large_err)]
fn authorize_surrogate_header(
    state: &ProviderFacadeState,
    headers: &HeaderMap,
) -> Result<String, Response> {
    if is_revoked(state) {
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "provider facade route revoked",
        ));
    }
    let bearer_token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let api_key_token = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok());
    let presented = match (bearer_token, api_key_token) {
        (Some(bearer), None) => bearer,
        (None, Some(api_key)) => api_key,
        (Some(bearer), Some(api_key)) if bearer == api_key => bearer,
        _ => {
            return Err(error_response(
                StatusCode::UNAUTHORIZED,
                "provider facade authorization failed",
            ));
        }
    };

    if presented != state.surrogate_token {
        // Check whether a surrogate from another tenant/session was presented to return explicit 403.
        let host = state
            .upstream_base_url
            .host_str()
            .unwrap_or_default()
            .trim_matches(|c| c == '[' || c == ']');
        if let Err(SentinelError::TenantMismatch | SentinelError::SessionMismatch) = state
            .sentinel
            .broker
            .resolve_for_egress(presented, &state.tenant_id, Some(&state.session_id), host)
        {
            return Err(error_response(
                StatusCode::FORBIDDEN,
                "surrogate credential does not belong to active tenant session",
            ));
        }
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "provider facade authorization failed",
        ));
    }
    Ok(presented.to_owned())
}

async fn forward(
    state: &ProviderFacadeState,
    presented_surrogate: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Bytes>,
    anthropic_version_override: Option<&str>,
) -> Response {
    if is_revoked(state) {
        return error_response(StatusCode::UNAUTHORIZED, "provider facade route revoked");
    }

    if let Some(meter) = state.meter.as_ref()
        && meter.scope.payer == super::usage_ledger::PayerMode::NativeSubscription
    {
        return error_response(
            StatusCode::FORBIDDEN,
            "Provider-permitted native-client subscription hosting is not supported for proxying. \
                 Session-token relay, pooling, silent paid fallback, and rebilling direct inference are disabled.",
        );
    }

    let upstream_host = state
        .upstream_base_url
        .host_str()
        .unwrap_or_default()
        .trim_matches(|c| c == '[' || c == ']');

    let is_byok = state
        .meter
        .as_ref()
        .is_some_and(|meter| meter.scope.payer == super::usage_ledger::PayerMode::ByokApi);

    // 1. Validate BYOK origin/vault rules first if BYOK mode, otherwise validate SSRF & resolve JIT surrogate.
    let upstream_api_key: Zeroizing<String> = if is_byok {
        let meter = state.meter.as_ref().expect("byok meter");
        // This supported BYOK route uses a verified provider API key. Never relay
        // a consumer login or send customer credentials to an arbitrary base URL.
        if state.upstream_base_url.scheme() != "https"
            || (state.upstream_base_url.host_str() != Some("api.openai.com")
                && state.upstream_base_url.host_str() != Some("api.anthropic.com"))
        {
            return error_response(
                StatusCode::FORBIDDEN,
                "The BYOK credential is bound to its verified provider origin",
            );
        }
        let provider_id = if state.upstream_base_url.host_str() == Some("api.anthropic.com") {
            "anthropic_api"
        } else {
            "openai_api"
        };

        let vault = match super::connection_vault::ConnectionVault::from_environment(
            meter.ledger.clone(),
        ) {
            Ok(vault) => vault,
            Err(_) => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Connection vault unavailable",
                );
            }
        };
        match vault.read_key(&meter.scope.tenant_id, provider_id).await {
            Ok(key) => key,
            Err(_) => {
                return error_response(
                    StatusCode::FORBIDDEN,
                    "Tenant API connection is absent or revoked",
                );
            }
        }
    } else {
        match state.sentinel.broker.resolve_for_egress(
            presented_surrogate,
            &state.tenant_id,
            Some(&state.session_id),
            upstream_host,
        ) {
            Ok(resolved) => resolved.secret_zeroizing(),
            Err(SentinelError::Revoked) => {
                return error_response(StatusCode::UNAUTHORIZED, "provider facade route revoked");
            }
            Err(SentinelError::TenantMismatch | SentinelError::SessionMismatch) => {
                return error_response(
                    StatusCode::FORBIDDEN,
                    "surrogate credential does not belong to active tenant session",
                );
            }
            Err(SentinelError::HostNotAllowed { .. }) => {
                return error_response(
                    StatusCode::FORBIDDEN,
                    "upstream provider host is not in surrogate allowlist",
                );
            }
            Err(_) => {
                return error_response(
                    StatusCode::UNAUTHORIZED,
                    "provider facade authorization failed",
                );
            }
        }
    };

    // 2. Enforce SSRF guard (domain allowlist + IP validation + DNS resolution) BEFORE dialing upstream.
    let validated_addrs = match state
        .sentinel
        .ssrf_guard
        .validate_url(&state.upstream_base_url)
        .await
    {
        Ok(addrs) => addrs,
        Err(SentinelError::SsrfBlocked { reason, .. }) => {
            return error_response(
                StatusCode::FORBIDDEN,
                &format!("upstream blocked by sentinel SSRF guard: {reason}"),
            );
        }
        Err(SentinelError::HostNotAllowed { .. }) => {
            return error_response(
                StatusCode::FORBIDDEN,
                "upstream host is not permitted by sentinel allowlist",
            );
        }
        Err(SentinelError::DnsResolutionFailed { .. }) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                "upstream provider DNS resolution failed",
            );
        }
        Err(err) => {
            return error_response(StatusCode::FORBIDDEN, &err.to_string());
        }
    };

    let request_id = Uuid::new_v4().to_string();
    let meter = if method == reqwest::Method::POST {
        state.meter.clone()
    } else {
        None
    };
    if is_revoked(state) {
        return error_response(StatusCode::UNAUTHORIZED, "provider facade route revoked");
    }
    if let Some(meter) = &meter {
        let Some(payload) = body.as_ref() else {
            return error_response(StatusCode::BAD_REQUEST, "metered inference requires a body");
        };
        if let Err(error) = meter.admit(&request_id, payload).await {
            let status = if error == super::usage_ledger::LedgerError::Limit {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            return error_response(status, &error.to_string());
        }
        if let Err(error) = meter.dispatched(&request_id).await {
            let _ = meter
                .ledger
                .cancel_before_dispatch(&meter.scope.tenant_id, &request_id)
                .await;
            return error_response(StatusCode::SERVICE_UNAVAILABLE, &error.to_string());
        }
    }
    let scanner = state
        .sentinel
        .leak_scanner
        .with_additional_secrets(std::slice::from_ref(&upstream_api_key));

    // Pin DNS-resolved validated addresses on hostname upstreams to prevent DNS rebinding TOCTOU.
    let client = if upstream_host.parse::<IpAddr>().is_ok() {
        state.client.clone()
    } else {
        match reqwest::Client::builder()
            .timeout(state.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .resolve_to_addrs(upstream_host, &validated_addrs)
            .build()
        {
            Ok(pinned) => pinned,
            Err(error) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    &scanner.redact_text(&error.to_string()),
                );
            }
        }
    };

    let url = upstream_url(&state.upstream_base_url, path, state.is_anthropic);
    let mut request = client.request(method, url);
    if state.is_anthropic {
        let version = anthropic_version_override.unwrap_or(&state.anthropic_version);
        request = request
            .header("x-api-key", upstream_api_key.as_str())
            .header("anthropic-version", version);
    } else {
        request = request
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", upstream_api_key.as_str()),
            )
            .header("Idempotency-Key", &request_id);
    }
    if let Some(body) = body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    let response = match tokio::select! {
        biased;
        _ = wait_for_any_revocation(state.revoked.clone(), state.external_revoked.clone()) => {
            return error_response(StatusCode::UNAUTHORIZED, "provider facade route revoked");
        }
        response = request.send() => response,
    } {
        Ok(response) => response,
        Err(error) => {
            return error_response(
                if error.is_timeout() {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                },
                &scanner.redact_text(&error.to_string()),
            );
        }
    };

    // 3. Scan ALL upstream response headers for reflected secrets (raw, base64, URL-encoded, shapes).
    let mut sanitized_content_type: Option<HeaderValue> = None;
    let mut sanitized_request_id: Option<String> = None;
    for (hdr_name, hdr_value) in response.headers() {
        if let Ok(val_str) = hdr_value.to_str() {
            let report = match scanner.scan_header_value(hdr_name.as_str(), val_str) {
                Ok(report) => report,
                Err(_) => {
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        "upstream response blocked by sentinel leak scanner",
                    );
                }
            };
            if report.leaks_detected > 0 {
                tracing::warn!(
                    header = %hdr_name,
                    "Sentinel scrubbed reflected secret in upstream response header"
                );
            }
            if hdr_name == reqwest::header::CONTENT_TYPE {
                sanitized_content_type = HeaderValue::from_str(&report.sanitized).ok();
            } else if hdr_name.as_str().eq_ignore_ascii_case("x-request-id") {
                sanitized_request_id = Some(report.sanitized);
            }
        }
    }

    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !response.status().is_success() {
        let body = tokio::select! {
            biased;
            _ = wait_for_any_revocation(state.revoked.clone(), state.external_revoked.clone()) => {
                return error_response(StatusCode::UNAUTHORIZED, "provider facade route revoked");
            }
            body = response.bytes() => body.unwrap_or_default(),
        };
        let scanned_body = match scanner.scan_text(&String::from_utf8_lossy(&body)) {
            Ok(report) => report.sanitized.into_bytes(),
            Err(_) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    "upstream error response blocked by sentinel leak scanner",
                );
            }
        };
        return response_with_body(status, sanitized_content_type.as_ref(), scanned_body);
    }

    let errors_scanner = scanner.clone();
    let capture = Arc::new(Mutex::new(UsageCapture::default()));
    if let Some(id) = sanitized_request_id {
        capture.lock().expect("capture mutex").provider_request_id = Some(id);
    }
    let collecting = capture.clone();
    let stream = response.bytes_stream().map(move |chunk| {
        match &chunk {
            Ok(bytes) => collecting.lock().expect("capture mutex").push(bytes),
            Err(_) => collecting.lock().expect("capture mutex").invalidate(),
        }
        chunk.map_err(|error| std::io::Error::other(errors_scanner.redact_text(&error.to_string())))
    });
    let finished_request = request_id.clone();
    let settlement = futures_util::stream::once(async move {
        if let Some(meter) = meter {
            let capture = std::mem::take(&mut *capture.lock().expect("capture mutex"));
            if let Err(error) = meter.finish(&finished_request, capture).await {
                tracing::warn!(usage_event = %finished_request, reason = %error, "Provider usage requires reconciliation; reservation retained");
            }
        }
        Ok::<Bytes, std::io::Error>(Bytes::new())
    });

    // Dropped/cancelled streams retain their durable in-flight reservation.
    let stream = redact_success_stream(stream.chain(settlement), scanner).take_until(
        wait_for_any_revocation(state.revoked.clone(), state.external_revoked.clone()),
    );
    let mut output = Response::new(Body::from_stream(stream));
    output.headers_mut().insert(
        "x-omnisolo-usage-id",
        HeaderValue::from_str(&request_id).expect("UUID header"),
    );
    *output.status_mut() = status;
    if let Some(content_type) = sanitized_content_type {
        output
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
    }
    output
}

fn response_with_body(
    status: StatusCode,
    content_type: Option<&HeaderValue>,
    body: Vec<u8>,
) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    if let Some(content_type) = content_type {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type.clone());
    }
    response
}

fn error_response(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({"error": {
        "message": message,
        "type": if status.is_client_error() { "invalid_request_error" } else { "provider_error" },
        "code": status.as_u16(),
    }});
    response_with_body(
        status,
        Some(&HeaderValue::from_static("application/json")),
        serde_json::to_vec(&body).expect("provider error JSON"),
    )
}

fn parse_upstream_url(value: &str) -> Result<Url, ProviderFacadeError> {
    let value = value.trim().trim_end_matches('/');
    let url = Url::parse(value).map_err(|_| {
        ProviderFacadeError::InvalidConfiguration(
            "upstream URL must be an absolute HTTP(S) URL".to_owned(),
        )
    })?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderFacadeError::InvalidConfiguration(
            "upstream URL must not contain credentials, query parameters, or fragments".to_owned(),
        ));
    }
    Ok(url)
}

fn upstream_url(base: &Url, path: &str, is_anthropic: bool) -> Url {
    let mut url = base.clone();
    let trimmed = base.path().trim_end_matches('/');
    let full_path = if is_anthropic && trimmed.is_empty() {
        format!("/v1/{path}")
    } else {
        format!("{trimmed}/{path}")
    };
    url.set_path(&full_path);
    url
}

fn redact(message: &str, secret: &str) -> String {
    message.replace(secret, "[REDACTED]")
}

// The receiver remains valid for every accepted request, including connections
// spawned by axum after the listener task has stopped.
async fn wait_for_revocation(mut revoked: watch::Receiver<bool>) {
    loop {
        if *revoked.borrow_and_update() {
            return;
        }
        if revoked.changed().await.is_err() {
            return;
        }
    }
}

async fn wait_for_any_revocation(
    revoked: watch::Receiver<bool>,
    external: Option<watch::Receiver<bool>>,
) {
    match external {
        None => wait_for_revocation(revoked).await,
        Some(ext) => {
            tokio::select! {
                _ = wait_for_revocation(revoked) => {}
                _ = wait_for_revocation(ext) => {}
            }
        }
    }
}

fn bind_reasoning(
    payload: &mut Value,
    path: &str,
    effort: Option<&Value>,
) -> Result<(), &'static str> {
    let object = payload
        .as_object_mut()
        .ok_or("request body must be an object")?;
    if path == "messages" {
        if object.contains_key("reasoning") || object.contains_key("reasoning_effort") {
            return Err("reasoning field does not match request dialect");
        }
        return Ok(());
    }
    let (field, other_field) = if path == "responses" {
        ("reasoning", "reasoning_effort")
    } else {
        ("reasoning_effort", "reasoning")
    };
    if object.contains_key(other_field) {
        return Err("reasoning field does not match request dialect");
    }
    if path == "responses" {
        if let Some(reasoning) = object.get(field) {
            let reasoning = reasoning.as_object().ok_or("reasoning must be an object")?;
            if let Some(requested) = reasoning.get("effort")
                && Some(requested) != effort
            {
                return Err("request reasoning effort is not admitted");
            }
        }
        if let Some(effort) = effort {
            let reasoning = object.entry(field).or_insert_with(|| serde_json::json!({}));
            reasoning
                .as_object_mut()
                .expect("validated reasoning object")
                .insert("effort".to_owned(), effort.clone());
        }
    } else {
        if let Some(requested) = object.get(field)
            && Some(requested) != effort
        {
            return Err("request reasoning effort is not admitted");
        }
        if let Some(effort) = effort {
            object.insert(field.to_owned(), effort.clone());
        }
    }
    Ok(())
}

fn bind_output_limit(
    payload: &mut Value,
    path: &str,
    limit: Option<u64>,
) -> Result<(), &'static str> {
    let Some(limit) = limit else { return Ok(()) };
    let fields: &[&str] = if path == "responses" {
        &["max_output_tokens"]
    } else if path == "messages" {
        &["max_tokens"]
    } else {
        &["max_completion_tokens", "max_tokens"]
    };
    let object = payload
        .as_object_mut()
        .ok_or("request body must be an object")?;
    for field in ["max_output_tokens", "max_completion_tokens", "max_tokens"] {
        if let Some(value) = object.get(field) {
            if !fields.contains(&field) {
                return Err("output limit field does not match request dialect");
            }
            if !value
                .as_u64()
                .is_some_and(|value| value > 0 && value <= limit)
            {
                return Err("request output limit is not admitted");
            }
        }
    }
    if !fields.iter().any(|field| object.contains_key(*field)) {
        object.insert(fields[0].to_owned(), Value::from(limit));
    }
    Ok(())
}

fn redact_success_stream(
    stream: impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
    scanner: ResponseLeakScanner,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send {
    let redactor = scanner.streaming_redactor();
    futures_util::stream::unfold(
        (Box::pin(stream), redactor, false),
        move |(mut stream, mut redactor, finished)| async move {
            loop {
                if finished {
                    return None;
                }
                match stream.next().await {
                    Some(Ok(chunk)) => match redactor.push_chunk(&chunk) {
                        Ok(out) if !out.is_empty() => {
                            return Some((Ok(Bytes::from(out)), (stream, redactor, false)));
                        }
                        Ok(_) => continue,
                        Err(err) => {
                            return Some((
                                Err(std::io::Error::other(err.to_string())),
                                (stream, redactor, true),
                            ));
                        }
                    },
                    Some(Err(error)) => {
                        return Some((Err(error), (stream, redactor, true)));
                    }
                    None => match redactor.finish() {
                        Ok(out) if !out.is_empty() => {
                            return Some((Ok(Bytes::from(out)), (stream, redactor, true)));
                        }
                        Ok(_) => return None,
                        Err(err) => {
                            return Some((
                                Err(std::io::Error::other(err.to_string())),
                                (stream, redactor, true),
                            ));
                        }
                    },
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::types::{ModelApiDialect, ReasoningEffort};
    use crate::sentinel::{LeakAction, SURROGATE_TOKEN_PREFIX, SurrogateTokenBroker};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Mutex;

    struct TestUpstream {
        url: String,
        seen_auth: Arc<Mutex<Vec<String>>>,
        hits: Arc<AtomicUsize>,
        _stop: oneshot::Sender<()>,
    }

    fn test_selection(model: &str) -> ResolvedModelSelection {
        ResolvedModelSelection {
            provider_route: "openai_api".to_owned(),
            model_id: model.to_owned(),
            reasoning_effort: Some(ReasoningEffort::Medium),
            api_dialect: ModelApiDialect::OpenAiResponses,
            context_window: None,
            max_output_tokens: Some(128),
            capabilities: Default::default(),
            binding_revision: "sentinel-test-v1".to_owned(),
            binding_digest: "sha256:sentinel-test".to_owned(),
            metadata: Default::default(),
        }
    }

    fn anthropic_selection(model_id: &str) -> ResolvedModelSelection {
        ResolvedModelSelection {
            provider_route: "anthropic_api".to_owned(),
            model_id: model_id.to_owned(),
            reasoning_effort: Some(ReasoningEffort::High),
            api_dialect: ModelApiDialect::AnthropicMessages,
            context_window: None,
            max_output_tokens: Some(256),
            capabilities: Default::default(),
            binding_revision: "anthropic-test-v1".to_owned(),
            binding_digest: "sha256:anthropic-test".to_owned(),
            metadata: Default::default(),
        }
    }

    async fn spawn_upstream(reflect_in_body: bool, reflect_in_header: bool) -> TestUpstream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen_auth = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let auth_clone = Arc::clone(&seen_auth);
        let hits_clone = Arc::clone(&hits);
        let (stop_tx, mut stop_rx) = oneshot::channel();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    res = listener.accept() => {
                        let Ok((mut sock, _)) = res else { break };
                        let mut raw = Vec::new();
                        let mut buf = [0u8; 4096];
                        loop {
                            let n = sock.read(&mut buf).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            raw.extend_from_slice(&buf[..n]);
                            if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        let req = String::from_utf8_lossy(&raw).into_owned();
                        hits_clone.fetch_add(1, Ordering::SeqCst);
                        let auth = req
                            .lines()
                            .find_map(|l| {
                                l.split_once(':')
                                    .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                                    .map(|(_, v)| v.trim().to_owned())
                            })
                            .unwrap_or_default();
                        auth_clone.lock().await.push(auth.clone());
                        let secret_only = auth.strip_prefix("Bearer ").unwrap_or(&auth);
                        let body = if reflect_in_body {
                            serde_json::json!({
                                "id": "resp_1",
                                "echo": secret_only,
                                "header_echo": auth,
                            }).to_string()
                        } else {
                            serde_json::json!({"object": "list", "data": []}).to_string()
                        };
                        let ct = if reflect_in_header {
                            format!("application/json; leaked={secret_only}")
                        } else {
                            "application/json".to_owned()
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nX-Upstream-Leak: {secret_only}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    }
                }
            }
        });
        TestUpstream {
            url: format!("http://{addr}/v1"),
            seen_auth,
            hits,
            _stop: stop_tx,
        }
    }

    #[tokio::test]
    async fn facade_issues_surrogate_injects_jit_scrubs_body_and_headers_and_enforces_revocation() {
        let secret = "sk-live-facade-real-secret-11223344";
        let upstream = spawn_upstream(true, true).await;
        let (rev_tx, rev_rx) = watch::channel(false);

        let config =
            ProviderFacadeConfig::new(upstream.url.clone(), secret, test_selection("gpt-test"))
                .with_tenant_session("tenant-a", "sess-a")
                .with_revocation(rev_rx);
        let facade = ProviderFacade::start_with_config(config).await.unwrap();

        assert!(facade.route().token().starts_with(SURROGATE_TOKEN_PREFIX));
        assert!(!facade.route().token().contains(secret));

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{}/responses", facade.route().base_url()))
            .bearer_auth(facade.route().token())
            .json(&serde_json::json!({"model": "gpt-test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(!ct.contains(secret), "leaked secret in content-type: {ct}");
        assert!(ct.contains("[REDACTED]"));
        assert!(resp.headers().get("x-upstream-leak").is_none());

        let text = resp.text().await.unwrap();
        assert!(!text.contains(secret), "leaked secret in body: {text}");
        assert!(text.contains("[REDACTED]"));
        assert_eq!(
            upstream.seen_auth.lock().await.as_slice(),
            &[format!("Bearer {secret}")]
        );

        // Trip watch::Receiver<bool> revocation -> immediately rejects subsequent requests (401)!
        rev_tx.send_replace(true);
        let after_revoke = client
            .post(format!("{}/responses", facade.route().base_url()))
            .bearer_auth(facade.route().token())
            .json(&serde_json::json!({"model": "gpt-test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(after_revoke.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(upstream.hits.load(Ordering::SeqCst), 1);

        facade.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn facade_blocks_ssrf_targets_and_disallowed_loopback_with_403_without_injecting() {
        let secret = "sk-live-must-not-reach-ssrf-55667788";
        let upstream = spawn_upstream(false, false).await;

        // 1. Metadata & RFC1918 targets are ALWAYS blocked with 403 even on default config.
        for ssrf_url in ["http://169.254.169.254/v1", "http://10.0.0.1/v1"] {
            let facade = ProviderFacade::start(ssrf_url, secret, test_selection("gpt-test"))
                .await
                .unwrap();
            let resp = reqwest::Client::new()
                .get(format!("{}/models", facade.route().base_url()))
                .bearer_auth(facade.route().token())
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "expected 403 for {ssrf_url}"
            );
            facade.shutdown().await.unwrap();
        }

        // 2. Explicit SsrfGuard with loopback disallowed blocks 127.0.0.1 with 403 WITHOUT dialing!
        let strict_guard = SsrfGuard::new(vec!["127.0.0.1".into()]);
        let config = ProviderFacadeConfig::new(upstream.url, secret, test_selection("gpt-test"))
            .with_ssrf_guard(strict_guard);
        let facade = ProviderFacade::start_with_config(config).await.unwrap();
        let resp = reqwest::Client::new()
            .get(format!("{}/models", facade.route().base_url()))
            .bearer_auth(facade.route().token())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(upstream.hits.load(Ordering::SeqCst), 0);
        facade.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn facade_enforces_cross_tenant_surrogate_isolation_and_block_mode_leak_scanner() {
        let upstream = spawn_upstream(true, true).await;
        let shared_broker = SurrogateTokenBroker::new();
        let guard = SsrfGuard::new(vec!["127.0.0.1".into()]).with_loopback_allowed_for_tests(true);
        let shared_sentinel = Sentinel::new(shared_broker, guard, ResponseLeakScanner::new());

        let facade_a = ProviderFacade::start_with_config(
            ProviderFacadeConfig::new(
                upstream.url.clone(),
                "sk-live-tenant-a-key-1234567890",
                test_selection("gpt-test"),
            )
            .with_tenant_session("tenant-a", "sess-a")
            .with_sentinel(shared_sentinel.clone()),
        )
        .await
        .unwrap();

        let facade_b = ProviderFacade::start_with_config(
            ProviderFacadeConfig::new(
                upstream.url.clone(),
                "sk-live-tenant-b-key-0987654321",
                test_selection("gpt-test"),
            )
            .with_tenant_session("tenant-b", "sess-b")
            .with_sentinel(shared_sentinel),
        )
        .await
        .unwrap();

        // Presenting Tenant A's surrogate to Tenant B's facade must fail with 403 Forbidden!
        let cross_resp = reqwest::Client::new()
            .post(format!("{}/responses", facade_b.route().base_url()))
            .bearer_auth(facade_a.route().token())
            .json(&serde_json::json!({"model": "gpt-test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(cross_resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(upstream.hits.load(Ordering::SeqCst), 0);

        // Block-mode leak scanner rejects upstream responses that reflect secrets in headers.
        let blocking_facade = ProviderFacade::start_with_config(
            ProviderFacadeConfig::new(
                upstream.url,
                "sk-live-blocked-secret-99887766",
                test_selection("gpt-test"),
            )
            .with_leak_scanner(ResponseLeakScanner::new().with_action(LeakAction::Block)),
        )
        .await
        .unwrap();
        let blocked_resp = reqwest::Client::new()
            .post(format!("{}/responses", blocking_facade.route().base_url()))
            .bearer_auth(blocking_facade.route().token())
            .json(&serde_json::json!({"model": "gpt-test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(blocked_resp.status(), StatusCode::BAD_GATEWAY);
        let body = blocked_resp.text().await.unwrap();
        assert!(!body.contains("sk-live-blocked-secret-99887766"));

        facade_a.shutdown().await.unwrap();
        facade_b.shutdown().await.unwrap();
        blocking_facade.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn anthropic_messages_route_uses_x_api_key_and_anthropic_version_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            let mut buf = [0_u8; 4096];
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
                if let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&req[..end]);
                    let content_len = headers
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if req.len() >= end + 4 + content_len {
                        break;
                    }
                }
            }
            let body = json!({
                "id": "msg_anthropic_1",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "hello"}],
                "usage": {"input_tokens": 11, "output_tokens": 4}
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8(req).unwrap()
        });

        let facade = ProviderFacade::start(
            format!("http://{addr}"),
            "sk-ant-upstream-secret-canary",
            anthropic_selection("claude-sonnet-4-5"),
        )
        .await
        .unwrap();

        let client = reqwest::Client::new();
        // OpenAI routes must be rejected when upstream is Anthropic.
        let wrong_route = client
            .post(format!("{}/chat/completions", facade.route().base_url()))
            .header("x-api-key", facade.route().token())
            .json(&json!({"model": "claude-sonnet-4-5", "messages": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong_route.status(), reqwest::StatusCode::BAD_REQUEST);

        // Anthropic /v1/messages accepts x-api-key facade token and forwards Anthropic headers.
        let ok_resp = client
            .post(format!("{}/messages", facade.route().base_url()))
            .header("x-api-key", facade.route().token())
            .json(&json!({
                "model": "claude-sonnet-4-5",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(ok_resp.status(), reqwest::StatusCode::OK);

        let raw_upstream = upstream.await.unwrap();
        let (raw_headers, raw_body) = raw_upstream.split_once("\r\n\r\n").unwrap();
        let first_line = raw_headers.lines().next().unwrap();
        assert!(
            first_line.starts_with("POST /v1/messages "),
            "expected POST /v1/messages, got: {first_line}"
        );
        let lower_headers = raw_headers.to_ascii_lowercase();
        assert!(
            lower_headers.contains("x-api-key: sk-ant-upstream-secret-canary"),
            "missing x-api-key header: {raw_headers}"
        );
        assert!(
            lower_headers.contains("anthropic-version: 2023-06-01"),
            "missing anthropic-version header: {raw_headers}"
        );
        assert!(
            !lower_headers.contains("authorization: bearer"),
            "Anthropic request must not send Authorization: Bearer: {raw_headers}"
        );
        let parsed_body: Value = serde_json::from_str(raw_body).unwrap();
        assert_eq!(parsed_body["max_tokens"], 256);
        assert!(parsed_body.get("reasoning_effort").is_none());

        facade.shutdown().await.unwrap();
    }
}
