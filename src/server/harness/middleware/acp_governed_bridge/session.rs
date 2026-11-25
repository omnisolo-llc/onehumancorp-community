use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};
use uuid::Uuid;

use super::super::acp::{AcpServerInfo, AcpV1Codec};
use super::super::harness::{
    HarnessAdapterError, HarnessEvent, HarnessSessionRequest, NativeSession,
};
use super::super::json_rpc::{JsonRpcErrorObject, JsonRpcId, JsonRpcServerRequest};
use super::super::process_env::apply_isolated_environment;
use super::super::protocol::{
    AttemptOperation, HarnessProtocolCodec, NativeTurnState, ServerResponse, SessionOperation,
};
use super::super::types::sanitize_credential_value;
use super::policy::{
    AcpBudgetGuard, AcpPermissionDecision, AcpStandingPolicy, evaluate_permission_request,
};
use super::registry::{
    AcpExternalHarnessSpec, AcpSandboxTier, AcpSanitizedEnvInput, build_sanitized_env,
    prepare_sanitized_workspace_dirs, validate_workspace_dir,
};
use super::transport::{
    InboundFrame, cancelled_turn_event, emit_event, permission_decision_event, read_stdout_loop,
    write_frame,
};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CANCEL_GRACE_PERIOD: Duration = Duration::from_secs(2);
const INBOUND_CHANNEL_CAPACITY: usize = 512;

#[derive(Clone)]
pub struct AcpSessionLaunchConfig {
    pub workspace_dir: PathBuf,
    pub sandbox_tier: AcpSandboxTier,
    pub policy: AcpStandingPolicy,
    pub surrogate_token: String,
    pub sentinel_proxy_url: String,
    pub ohc_mcp_base_url: Option<String>,
    pub budget_guard: Option<AcpBudgetGuard>,
    pub request_timeout: Duration,
    pub cancel_grace_period: Duration,
    pub extra_non_secret_env: BTreeMap<String, String>,
}

impl std::fmt::Debug for AcpSessionLaunchConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcpSessionLaunchConfig")
            .field("workspace_dir", &self.workspace_dir)
            .field("sandbox_tier", &self.sandbox_tier)
            .field("policy", &self.policy)
            .field("surrogate_token", &"<redacted>")
            .field("sentinel_proxy_url", &self.sentinel_proxy_url)
            .field("ohc_mcp_base_url", &self.ohc_mcp_base_url)
            .field("budget_guard", &self.budget_guard)
            .field("request_timeout", &self.request_timeout)
            .field("cancel_grace_period", &self.cancel_grace_period)
            .finish()
    }
}

impl AcpSessionLaunchConfig {
    pub fn new(
        workspace_dir: impl Into<PathBuf>,
        sandbox_tier: AcpSandboxTier,
        policy: AcpStandingPolicy,
        surrogate_token: impl Into<String>,
        sentinel_proxy_url: impl Into<String>,
    ) -> Self {
        Self {
            workspace_dir: workspace_dir.into(),
            sandbox_tier,
            policy,
            surrogate_token: surrogate_token.into(),
            sentinel_proxy_url: sentinel_proxy_url.into(),
            ohc_mcp_base_url: None,
            budget_guard: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            cancel_grace_period: DEFAULT_CANCEL_GRACE_PERIOD,
            extra_non_secret_env: BTreeMap::new(),
        }
    }

    pub fn with_ohc_mcp_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.ohc_mcp_base_url = Some(base_url.into());
        self
    }

    pub fn with_budget_guard(mut self, guard: AcpBudgetGuard) -> Self {
        self.budget_guard = Some(guard);
        self
    }

    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    pub fn with_cancel_grace_period(mut self, cancel_grace_period: Duration) -> Self {
        self.cancel_grace_period = cancel_grace_period;
        self
    }

    pub fn with_extra_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_non_secret_env.insert(key.into(), value.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AcpTurnResult {
    pub session_id: String,
    pub stop_reason: String,
    pub final_text: Option<String>,
    pub usage: Option<Value>,
    pub events: Vec<HarnessEvent>,
    pub permission_decisions: Vec<AcpPermissionDecision>,
    pub cancelled: bool,
    pub force_killed: bool,
}

#[derive(Clone)]
pub struct AcpCancelHandle {
    cancel_tx: watch::Sender<bool>,
}

impl AcpCancelHandle {
    /// Signals the active ACP turn to emit `session/cancel` and enforce the
    /// configured kill fallback if the child harness does not abort cleanly.
    pub fn cancel(&self) {
        self.cancel_tx.send_replace(true);
    }
}

pub struct AcpGovernedSession {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Arc<Mutex<Child>>,
    inbound_rx: mpsc::Receiver<InboundFrame>,
    reader_task: Option<JoinHandle<()>>,
    next_id: AtomicU64,
    codec: AcpV1Codec,
    workspace_cwd: String,
    mcp_servers: Vec<Value>,
    policy: AcpStandingPolicy,
    budget_guard: Option<AcpBudgetGuard>,
    session_request: HarnessSessionRequest,
    request_timeout: Duration,
    cancel_grace_period: Duration,
    cancel_tx: watch::Sender<bool>,
    cancel_rx: watch::Receiver<bool>,
    server_info: Option<AcpServerInfo>,
    native_session_id: Option<String>,
    turn_state: Option<NativeTurnState>,
}

impl AcpGovernedSession {
    pub fn spawn(
        spec: &AcpExternalHarnessSpec,
        config: AcpSessionLaunchConfig,
    ) -> Result<Self, HarnessAdapterError> {
        spec.validate()?;
        if !spec.supports_tier(config.sandbox_tier) {
            return Err(HarnessAdapterError::InvalidRequest(format!(
                "ACP harness '{}' does not support sandbox tier {}",
                spec.harness_id,
                config.sandbox_tier.as_str()
            )));
        }
        config.policy.validate()?;
        if config.request_timeout.is_zero() || config.cancel_grace_period.is_zero() {
            return Err(HarnessAdapterError::InvalidRequest(
                "ACP request_timeout and cancel_grace_period must be greater than zero".to_owned(),
            ));
        }
        if let Some(guard) = &config.budget_guard
            && guard.scope.tenant_id != config.policy.tenant_id
        {
            return Err(HarnessAdapterError::InvalidRequest(
                "AcpBudgetGuard tenant_id does not match AcpStandingPolicy tenant_id".to_owned(),
            ));
        }

        let workspace_cwd = validate_workspace_dir(&config.workspace_dir)?;
        prepare_sanitized_workspace_dirs(&config.workspace_dir)?;

        let sanitized_env = build_sanitized_env(&AcpSanitizedEnvInput {
            workspace_dir: config.workspace_dir.clone(),
            surrogate_token: config.surrogate_token.clone(),
            sentinel_proxy_url: config.sentinel_proxy_url.clone(),
            tenant_id: config.policy.tenant_id.clone(),
            sandbox_tier: config.sandbox_tier,
            extra_non_secret_env: config.extra_non_secret_env.clone(),
        })?;

        let mcp_servers = match &spec.mcp_bridge_command {
            Some(bridge) => {
                let base_url = config.ohc_mcp_base_url.as_deref().ok_or_else(|| {
                    HarnessAdapterError::InvalidRequest(
                        "ohc_mcp_base_url is required when the ACP harness spec configures an MCP bridge"
                            .to_owned(),
                    )
                })?;
                vec![bridge.to_acp_mcp_server_entry(base_url, &config.surrogate_token)?]
            }
            None => Vec::new(),
        };

        let session_request =
            HarnessSessionRequest::new(&config.policy.tenant_id, Uuid::new_v4(), Uuid::new_v4());
        let codec = AcpV1Codec::for_kimi(&workspace_cwd, &session_request)?;

        let mut command = Command::new(&spec.executable);
        command.args(&spec.args);
        command.current_dir(&config.workspace_dir);
        apply_isolated_environment(&mut command, &sanitized_env);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(HarnessAdapterError::Spawn)?;
        let stdin = child.stdin.take().ok_or_else(|| {
            HarnessAdapterError::Io(std::io::Error::other("ACP child stdin was not piped"))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            HarnessAdapterError::Io(std::io::Error::other("ACP child stdout was not piped"))
        })?;

        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
        let reader_task = tokio::spawn(read_stdout_loop(stdout, inbound_tx));
        let (cancel_tx, cancel_rx) = watch::channel(false);

        Ok(Self {
            stdin: Arc::new(Mutex::new(stdin)),
            child: Arc::new(Mutex::new(child)),
            inbound_rx,
            reader_task: Some(reader_task),
            next_id: AtomicU64::new(1),
            codec,
            workspace_cwd,
            mcp_servers,
            policy: config.policy,
            budget_guard: config.budget_guard,
            session_request,
            request_timeout: config.request_timeout,
            cancel_grace_period: config.cancel_grace_period,
            cancel_tx,
            cancel_rx,
            server_info: None,
            native_session_id: None,
            turn_state: None,
        })
    }

    pub fn cancel_handle(&self) -> AcpCancelHandle {
        AcpCancelHandle {
            cancel_tx: self.cancel_tx.clone(),
        }
    }

    pub async fn process_id(&self) -> Option<u32> {
        self.child.lock().await.id()
    }

    pub fn server_info(&self) -> Option<&AcpServerInfo> {
        self.server_info.as_ref()
    }

    pub fn native_session_id(&self) -> Option<&str> {
        self.native_session_id.as_deref()
    }

    pub async fn initialize(&mut self) -> Result<AcpServerInfo, HarnessAdapterError> {
        let req = self.codec.initialize_request();
        let result = self.send_simple_request(&req.method, req.params).await?;
        let info = self.codec.decode_initialize_result(result)?;
        self.server_info = Some(info.clone());
        Ok(info)
    }

    pub async fn new_session(&mut self) -> Result<NativeSession, HarnessAdapterError> {
        if self.server_info.is_none() {
            return Err(HarnessAdapterError::InvalidRequest(
                "ACP session must be initialized before session/new".to_owned(),
            ));
        }
        let params = json!({
            "cwd": self.workspace_cwd,
            "mcpServers": self.mcp_servers,
        });
        let result = self.send_simple_request("session/new", params).await?;
        let native = self
            .codec
            .decode_session_result(SessionOperation::Create, result)?;
        self.native_session_id = Some(native.native_session_id.clone());
        self.turn_state = Some(NativeTurnState::new(native.native_session_id.clone()));
        Ok(native)
    }

    pub async fn start_session(
        &mut self,
    ) -> Result<(AcpServerInfo, NativeSession), HarnessAdapterError> {
        let info = self.initialize().await?;
        let session = self.new_session().await?;
        Ok((info, session))
    }

    pub async fn prompt(&mut self, prompt: &str) -> Result<AcpTurnResult, HarnessAdapterError> {
        self.prompt_with_stream(prompt, None).await
    }

    pub async fn prompt_with_stream(
        &mut self,
        prompt: &str,
        stream_tx: Option<mpsc::Sender<HarnessEvent>>,
    ) -> Result<AcpTurnResult, HarnessAdapterError> {
        let session_id = self.native_session_id.clone().ok_or_else(|| {
            HarnessAdapterError::InvalidRequest(
                "ACP session/new must succeed before session/prompt".to_owned(),
            )
        })?;
        let spec = self.codec.attempt_request(
            AttemptOperation::Execute,
            &self.session_request,
            prompt,
            Some(&session_id),
            None,
        )?;

        let prompt_id = self.next_request_id();
        self.write_request(&prompt_id, &spec.method, spec.params)
            .await?;

        let mut events = Vec::new();
        let mut permission_decisions = Vec::new();
        let mut streamed_text = String::new();
        let mut cancel_sent = false;
        let mut cancel_deadline: Option<Instant> = None;
        let turn_deadline = Instant::now() + self.request_timeout;

        if *self.cancel_rx.borrow() {
            self.send_cancel_notification(&session_id).await;
            cancel_sent = true;
            cancel_deadline = Some(Instant::now() + self.cancel_grace_period);
        }

        loop {
            let active_deadline = cancel_deadline.map_or(turn_deadline, |d| d.min(turn_deadline));
            tokio::select! {
                biased;

                _ = self.cancel_rx.changed(), if !cancel_sent => {
                    if *self.cancel_rx.borrow() {
                        self.send_cancel_notification(&session_id).await;
                        cancel_sent = true;
                        cancel_deadline = Some(Instant::now() + self.cancel_grace_period);
                    }
                }

                frame = self.inbound_rx.recv() => {
                    match frame {
                        Some(InboundFrame::Notification(notification)) => {
                            let state = self.turn_state.as_mut().expect("turn state initialized with session");
                            let decoded = self.codec.decode_notification(&notification, state)?;
                            if decoded.event.event_type == "assistant.text_chunk"
                                && let Some(chunk) = decoded.event.payload.get("content").and_then(Value::as_str)
                            {
                                streamed_text.push_str(chunk);
                            }
                            emit_event(&mut events, stream_tx.as_ref(), decoded.event).await;
                        }
                        Some(InboundFrame::ServerRequest(server_req)) => {
                            self.handle_inbound_server_request(
                                &session_id,
                                &server_req,
                                &mut events,
                                &mut permission_decisions,
                                stream_tx.as_ref(),
                            )
                            .await?;
                        }
                        Some(InboundFrame::Response { id, result }) if id == prompt_id => {
                            let value = result.map_err(|err| {
                                HarnessAdapterError::Remote(format!("{}: {}", err.code, err.message))
                            })?;
                            let decoded = self.codec.decode_prompt_result(&session_id, value)?;
                            let stop_reason = decoded
                                .event
                                .payload
                                .get("stop_reason")
                                .and_then(Value::as_str)
                                .unwrap_or("end_turn")
                                .to_owned();
                            let cancelled = stop_reason == "cancelled" || cancel_sent;
                            let usage = decoded.usage.clone();
                            emit_event(&mut events, stream_tx.as_ref(), decoded.event).await;
                            return Ok(AcpTurnResult {
                                session_id,
                                stop_reason,
                                final_text: (!streamed_text.is_empty()).then_some(streamed_text),
                                usage,
                                events,
                                permission_decisions,
                                cancelled,
                                force_killed: false,
                            });
                        }
                        Some(InboundFrame::Response { .. }) => {
                            self.kill_and_reap().await;
                            return Err(HarnessAdapterError::InvalidResponse(
                                "received unexpected JSON-RPC response id during session/prompt".to_owned(),
                            ));
                        }
                        Some(InboundFrame::TransportError(message)) => {
                            self.kill_and_reap().await;
                            return Err(HarnessAdapterError::InvalidResponse(message));
                        }
                        None => {
                            self.kill_and_reap().await;
                            if cancel_sent {
                                emit_event(&mut events, stream_tx.as_ref(), cancelled_turn_event(&session_id, false)).await;
                                return Ok(AcpTurnResult {
                                    session_id,
                                    stop_reason: "cancelled".to_owned(),
                                    final_text: (!streamed_text.is_empty()).then_some(streamed_text),
                                    usage: None,
                                    events,
                                    permission_decisions,
                                    cancelled: true,
                                    force_killed: false,
                                });
                            }
                            return Err(HarnessAdapterError::ProcessExited);
                        }
                    }
                }

                _ = sleep_until(active_deadline) => {
                    if cancel_sent {
                        self.kill_and_reap().await;
                        emit_event(&mut events, stream_tx.as_ref(), cancelled_turn_event(&session_id, true)).await;
                        return Ok(AcpTurnResult {
                            session_id,
                            stop_reason: "cancelled".to_owned(),
                            final_text: (!streamed_text.is_empty()).then_some(streamed_text),
                            usage: None,
                            events,
                            permission_decisions,
                            cancelled: true,
                            force_killed: true,
                        });
                    }
                    self.send_cancel_notification(&session_id).await;
                    self.kill_and_reap().await;
                    return Err(HarnessAdapterError::Timeout);
                }
            }
        }
    }

    pub async fn cancel(&mut self) -> Result<(), HarnessAdapterError> {
        self.cancel_tx.send_replace(true);
        if let Some(session_id) = self.native_session_id.clone() {
            let notification = self.codec.cancel_notification(&session_id)?;
            self.write_notification(&notification.method, notification.params)
                .await?;
        }
        Ok(())
    }

    pub async fn shutdown(mut self) -> Result<(), HarnessAdapterError> {
        if let Some(session_id) = self.native_session_id.clone() {
            self.send_cancel_notification(&session_id).await;
        }
        self.kill_and_reap().await;
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
        Ok(())
    }

    async fn handle_inbound_server_request(
        &self,
        session_id: &str,
        server_req: &JsonRpcServerRequest,
        events: &mut Vec<HarnessEvent>,
        permission_decisions: &mut Vec<AcpPermissionDecision>,
        stream_tx: Option<&mpsc::Sender<HarnessEvent>>,
    ) -> Result<(), HarnessAdapterError> {
        if server_req.method == "session/request_permission" {
            self.codec.server_request_event(server_req)?;
            let decision = evaluate_permission_request(
                &self.policy,
                self.budget_guard.as_ref(),
                session_id,
                server_req,
            )
            .await?;
            let wire = self.codec.encode_server_response(
                "session/request_permission",
                &json!({ "result": decision.acp_response_result }),
            )?;
            match wire {
                ServerResponse::Result(result) => {
                    self.write_server_result(&server_req.id, result).await?;
                }
                ServerResponse::Error(err) => {
                    self.write_server_error(&server_req.id, err).await?;
                }
            }
            let audit_event = permission_decision_event(session_id, &decision);
            permission_decisions.push(decision);
            emit_event(events, stream_tx, audit_event).await;
        } else {
            self.write_server_error(
                &server_req.id,
                JsonRpcErrorObject {
                    code: -32601,
                    message: format!(
                        "ACP server-to-client method '{}' is disabled by OHC governance",
                        server_req.method
                    ),
                    data: None,
                },
            )
            .await?;
            emit_event(
                events,
                stream_tx,
                HarnessEvent {
                    event_type: "governance.method_denied".to_owned(),
                    durable: true,
                    payload: sanitize_credential_value(&json!({
                        "session_id": session_id,
                        "method": server_req.method,
                    })),
                    native_cursor: None,
                },
            )
            .await;
        }
        Ok(())
    }

    async fn send_simple_request(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, HarnessAdapterError> {
        let id = self.next_request_id();
        self.write_request(&id, method, params).await?;
        let deadline = Instant::now() + self.request_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.kill_and_reap().await;
                return Err(HarnessAdapterError::Timeout);
            }
            match timeout(remaining, self.inbound_rx.recv()).await {
                Err(_) => {
                    self.kill_and_reap().await;
                    return Err(HarnessAdapterError::Timeout);
                }
                Ok(None) => {
                    self.kill_and_reap().await;
                    return Err(HarnessAdapterError::ProcessExited);
                }
                Ok(Some(InboundFrame::TransportError(msg))) => {
                    self.kill_and_reap().await;
                    return Err(HarnessAdapterError::InvalidResponse(msg));
                }
                Ok(Some(InboundFrame::Response {
                    id: response_id,
                    result,
                })) if response_id == id => {
                    return result.map_err(|err| {
                        HarnessAdapterError::Remote(format!("{}: {}", err.code, err.message))
                    });
                }
                Ok(Some(InboundFrame::Notification(_))) => continue,
                Ok(Some(InboundFrame::ServerRequest(req))) => {
                    self.write_server_error(
                        &req.id,
                        JsonRpcErrorObject {
                            code: -32601,
                            message:
                                "server-to-client requests are not permitted before session/prompt"
                                    .to_owned(),
                            data: None,
                        },
                    )
                    .await?;
                }
                Ok(Some(InboundFrame::Response { .. })) => {
                    self.kill_and_reap().await;
                    return Err(HarnessAdapterError::InvalidResponse(format!(
                        "received unexpected response id while awaiting {method}"
                    )));
                }
            }
        }
    }

    async fn send_cancel_notification(&self, session_id: &str) {
        if let Ok(notification) = self.codec.cancel_notification(session_id) {
            let _ = self
                .write_notification(&notification.method, notification.params)
                .await;
        }
    }

    async fn kill_and_reap(&self) {
        let mut child = self.child.lock().await;
        if let Ok(None) = child.try_wait() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    fn next_request_id(&self) -> JsonRpcId {
        JsonRpcId::from_number(self.next_id.fetch_add(1, Ordering::AcqRel))
    }

    async fn write_request(
        &self,
        id: &JsonRpcId,
        method: &str,
        params: Value,
    ) -> Result<(), HarnessAdapterError> {
        write_frame(
            &self.stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": id.as_value(),
                "method": method,
                "params": params,
            }),
        )
        .await
    }

    async fn write_notification(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(), HarnessAdapterError> {
        write_frame(
            &self.stdin,
            &json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params,
            }),
        )
        .await
    }

    async fn write_server_result(
        &self,
        id: &JsonRpcId,
        result: Value,
    ) -> Result<(), HarnessAdapterError> {
        write_frame(
            &self.stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": id.as_value(),
                "result": result,
            }),
        )
        .await
    }

    async fn write_server_error(
        &self,
        id: &JsonRpcId,
        error: JsonRpcErrorObject,
    ) -> Result<(), HarnessAdapterError> {
        write_frame(
            &self.stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": id.as_value(),
                "error": {
                    "code": error.code,
                    "message": error.message,
                    "data": error.data,
                },
            }),
        )
        .await
    }
}

impl Drop for AcpGovernedSession {
    fn drop(&mut self) {
        if let Some(task) = self.reader_task.take() {
            task.abort();
        }
    }
}
