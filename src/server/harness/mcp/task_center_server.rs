//! Inbound OHC Task Center MCP JSON-RPC 2.0 Server (`2025-03-26`).
//! Read-only by default; write tools require `x-ohc-mcp-write: true` + `owner|admin`
//! and strictly create `awaiting_approval` proposals with 64 KiB caps & secret redaction.

use async_trait::async_trait;
use regex::Regex;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

pub const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
pub const MCP_SERVER_NAME: &str = "ohc-task-center-mcp";
pub const MCP_SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MCP_WRITE_OPT_IN_HEADER: &str = "x-ohc-mcp-write";
pub const MCP_MAX_PAYLOAD_BYTES: usize = 64 * 1024;
pub const ERR_PARSE: i64 = -32700;
pub const ERR_INVALID_REQUEST: i64 = -32600;
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERR_INVALID_PARAMS: i64 = -32602;
pub const ERR_INTERNAL: i64 = -32603;
pub const ERR_UNAUTHORIZED: i64 = -32001;
pub const ERR_FORBIDDEN: i64 = -32003;
pub const ERR_NOT_FOUND: i64 = -32004;
pub const ERR_PAYLOAD_TOO_LARGE: i64 = -32013;

pub type McpResult = Result<Value, McpError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpAccessScope {
    pub tenant_id: String,
    pub actor_id: String,
    pub roles: Vec<String>,
    pub allow_write: bool,
}

impl McpAccessScope {
    pub fn read_only(t: impl Into<String>, a: impl Into<String>, roles: Vec<String>) -> Self {
        let (tenant_id, actor_id, allow_write) = (t.into(), a.into(), false);
        Self {
            tenant_id,
            actor_id,
            roles,
            allow_write,
        }
    }
    pub fn with_write_opt_in(mut self, allow: bool) -> Self {
        self.allow_write = allow;
        self
    }
    pub fn is_owner_or_admin(&self) -> bool {
        self.roles
            .iter()
            .any(|r| r.eq_ignore_ascii_case("owner") || r.eq_ignore_ascii_case("admin"))
    }
    pub fn can_write(&self) -> bool {
        self.allow_write && self.is_owner_or_admin()
    }
    pub fn validate(&self) -> Result<(), McpError> {
        let ok = |v: &str| {
            !v.is_empty() && v.trim() == v && v.len() <= 255 && !v.chars().any(char::is_control)
        };
        (ok(&self.tenant_id) && ok(&self.actor_id))
            .then_some(())
            .ok_or_else(|| McpError::new(ERR_UNAUTHORIZED, "Auth required"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpError {
    pub code: i64,
    pub message: String,
}

impl McpError {
    pub fn new(code: i64, msg: impl Into<String>) -> Self {
        Self {
            code,
            message: redact_secrets_in_str(&msg.into()),
        }
    }
    pub fn invalid_params(m: impl Into<String>) -> Self {
        Self::new(ERR_INVALID_PARAMS, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(ERR_INTERNAL, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(ERR_NOT_FOUND, m)
    }
}
impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MCP {}: {}", self.code, self.message)
    }
}
impl std::error::Error for McpError {}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpToolScope {
    Read,
    WriteProposal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpToolDescriptor {
    pub name: String,
    pub description: String,
    pub scope: McpToolScope,
    pub requires_owner_or_admin: bool,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    pub annotations: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpResourceDescriptor {
    pub uri: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<McpError>,
}

impl JsonRpcResponse {
    fn mk(id: Value, result: Option<Value>, error: Option<McpError>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result,
            error,
        }
    }
    pub fn ok(id: Value, r: Value) -> Self {
        Self::mk(id, Some(r), None)
    }
    pub fn err(id: Value, e: McpError) -> Self {
        Self::mk(id, None, Some(e))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListSpacesArgs {
    pub include_archived: bool,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListTasksArgs {
    pub space_id: Option<String>,
    pub status: Option<String>,
    pub attention_only: bool,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetTaskArgs {
    pub task_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ListApprovalsArgs {
    pub status: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CreateSpaceInput {
    pub name: String,
    pub description: String,
    pub kind: String,
    pub prompt_template: String,
    pub department: String,
    pub reads: Vec<String>,
    pub shows: Vec<String>,
    pub cannot: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProposeTaskInput {
    pub title: String,
    pub prompt: String,
    pub space_id: Option<String>,
    pub department: String,
    pub action_kind: String,
    pub estimated_cost_micros: i64,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProposeEmailDraftInput {
    pub to: String,
    pub subject: String,
    pub body: String,
    pub from: Option<String>,
    pub space_id: Option<String>,
    pub department: String,
}

#[async_trait]
pub trait TaskCenterMcpBackend: Send + Sync {
    async fn list_spaces(&self, s: &McpAccessScope, a: ListSpacesArgs) -> McpResult;
    async fn list_tasks(&self, s: &McpAccessScope, a: ListTasksArgs) -> McpResult;
    async fn get_task(&self, s: &McpAccessScope, a: GetTaskArgs) -> McpResult;
    async fn list_approvals(&self, s: &McpAccessScope, a: ListApprovalsArgs) -> McpResult;
    async fn get_usage_budget(&self, s: &McpAccessScope) -> McpResult;
    async fn list_connectors(&self, s: &McpAccessScope) -> McpResult;
    async fn create_space(&self, s: &McpAccessScope, i: CreateSpaceInput) -> McpResult;
    async fn propose_task(&self, s: &McpAccessScope, i: ProposeTaskInput) -> McpResult;
    async fn propose_email_draft(&self, s: &McpAccessScope, i: ProposeEmailDraftInput)
    -> McpResult;
}

pub fn compute_payload_sha256(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    format!("{:x}", Sha256::digest(&bytes))
}

pub fn redact_secrets_in_str(input: &str) -> String {
    static RE: OnceLock<(Regex, Regex)> = OnceLock::new();
    let (b, t) = RE.get_or_init(|| {
        let b = Regex::new(r"Bearer\s+[A-Za-z0-9\-._~+/]+=*").expect("regex");
        let t = Regex::new(r"(?:sk-ant-[A-Za-z0-9_\-]{8,}|sk-(?:proj-|svcacct-|live-|test-)?[A-Za-z0-9_\-]{8,}|(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{8,}|\bre_[A-Za-z0-9_\-]{8,}|\bya[2]9\.[A-Za-z0-9_\-\.]{8,}|\bAIz[a][0-9A-Za-z_\-]{20,}|\bgh[pousr]_[A-Za-z0-9]{16,}|\beyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,})").expect("regex");
        (b, t)
    });
    let s = b.replace_all(input, "Bearer [REDACTED]");
    t.replace_all(&s, "[REDACTED]").into_owned()
}

pub fn redact_secrets_in_value(value: &mut Value) {
    const SAFE: &str = "|idempotency_key|key_id|token_count|max_tokens|";
    const SENS: &str = "|api_key|api_token|bot_token|access_token|refresh_token|session_token|secret|password|authorization|ciphertext_hex|nonce_hex|private_key|token|";
    match value {
        Value::String(s) => *s = redact_secrets_in_str(s),
        Value::Array(a) => a.iter_mut().for_each(redact_secrets_in_value),
        Value::Object(m) => {
            for (k, v) in m.iter_mut() {
                let kl = k.to_ascii_lowercase();
                let n = format!("|{kl}|");
                let suf = ["_secret", "_token", "_api_key", "_password"];
                if !SAFE.contains(&n) && (SENS.contains(&n) || suf.iter().any(|s| kl.ends_with(s)))
                {
                    *v = json!("[REDACTED]");
                } else {
                    redact_secrets_in_value(v);
                }
            }
        }
        _ => {}
    }
}

fn txt(f: &str, v: &str, min: usize, max: usize, nl: bool) -> Result<String, McpError> {
    let t = v.trim();
    let bad = t
        .chars()
        .any(|c| !(nl && matches!(c, '\n' | '\r' | '\t')) && c.is_control());
    let l = t.to_ascii_lowercase();
    if t.len() < min || t.len() > max || bad || l.contains("<script") || l.contains("javascript:") {
        return Err(McpError::invalid_params(format!("Invalid '{f}'")));
    }
    Ok(t.into())
}

fn email(f: &str, v: &str) -> Result<String, McpError> {
    let e = txt(f, v, 3, 254, false)?;
    let bad = e.split_once('@').is_none_or(|(loc, d)| {
        loc.is_empty()
            || d.contains('@')
            || d.len() < 3
            || !d.contains('.')
            || d.starts_with('.')
            || d.ends_with('.')
    });
    if bad || e.chars().any(char::is_whitespace) || e.contains([',', ';']) {
        return Err(McpError::invalid_params(format!("Invalid email '{f}'")));
    }
    Ok(e)
}

fn opt_id(f: &str, v: Option<String>) -> Result<Option<String>, McpError> {
    let Some(raw) = v.filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    let t = raw.trim();
    let ok = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.');
    if t.len() > 128 || !t.bytes().all(ok) {
        return Err(McpError::invalid_params(format!("Invalid id '{f}'")));
    }
    Ok(Some(t.into()))
}

fn list_txt(f: &str, items: Vec<String>) -> Result<Vec<String>, McpError> {
    if items.len() > 16 {
        return Err(McpError::invalid_params(format!("Too many '{f}'")));
    }
    items
        .into_iter()
        .map(|i| txt(f, &i, 1, 128, false))
        .collect()
}

fn def<'a>(v: &'a str, d: &'a str) -> &'a str {
    if v.trim().is_empty() { d } else { v }
}

fn validate_space(mut i: CreateSpaceInput) -> Result<CreateSpaceInput, McpError> {
    const K: &str = "|operations|client_delivery|cash_recovery|research|custom|";
    i.name = txt("name", &i.name, 1, 80, false)?;
    i.description = txt("description", &i.description, 0, 1024, true)?;
    let k = def(&i.kind, "custom").trim();
    if !K.contains(&format!("|{k}|")) {
        return Err(McpError::invalid_params("Invalid space kind"));
    }
    i.kind = k.into();
    i.prompt_template = txt("prompt_template", &i.prompt_template, 0, 4096, true)?;
    i.department = txt("department", def(&i.department, "operations"), 1, 64, false)?;
    i.reads = list_txt("reads", i.reads)?;
    i.shows = list_txt("shows", i.shows)?;
    i.cannot = list_txt("cannot", i.cannot)?;
    if i.cannot.is_empty() {
        i.cannot = vec!["Spend or send externally without approval".into()];
    }
    Ok(i)
}

fn validate_task(mut i: ProposeTaskInput) -> Result<ProposeTaskInput, McpError> {
    i.title = txt("title", &i.title, 1, 160, false)?;
    i.prompt = txt("prompt", &i.prompt, 1, 8192, true)?;
    i.space_id = opt_id("space_id", i.space_id)?;
    i.department = txt("department", def(&i.department, "operations"), 1, 64, false)?;
    i.action_kind = txt("action_kind", def(&i.action_kind, "run_task"), 1, 64, false)?;
    if !(0..=1_000_000_000_000).contains(&i.estimated_cost_micros) {
        return Err(McpError::invalid_params("Cost out of range"));
    }
    i.idempotency_key = opt_id("idempotency_key", i.idempotency_key)?;
    Ok(i)
}

fn validate_email(mut i: ProposeEmailDraftInput) -> Result<ProposeEmailDraftInput, McpError> {
    i.to = email("to", &i.to)?;
    i.subject = txt("subject", &i.subject, 1, 200, false)?;
    i.body = txt("body", &i.body, 1, 32 * 1024, true)?;
    let from = i.from.filter(|f| !f.trim().is_empty());
    i.from = from.map(|f| email("from", &f)).transpose()?;
    i.space_id = opt_id("space_id", i.space_id)?;
    let dept = def(&i.department, "communications");
    i.department = txt("department", dept, 1, 64, false)?;
    Ok(i)
}

pub fn ensure_awaiting_approval_proposal(v: &Value) -> Result<(), McpError> {
    let ok = v.get("status").and_then(Value::as_str) == Some("awaiting_approval")
        && v.get("provenance").and_then(Value::as_str) == Some("awaiting_approval");
    ok.then_some(())
        .ok_or_else(|| McpError::internal("Must be awaiting_approval"))
}

fn parse<T: DeserializeOwned>(v: Value) -> Result<T, McpError> {
    serde_json::from_value(v).map_err(|_| McpError::invalid_params("Invalid arguments"))
}

fn cap_json(mut payload: Value, max_raw: usize) -> (String, Value) {
    redact_secrets_in_value(&mut payload);
    let raw = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
    if raw.len() <= max_raw {
        return (raw, payload);
    }
    let mut end = (16 * 1024).min(raw.len());
    while end > 0 && !raw.is_char_boundary(end) {
        end -= 1;
    }
    let cap = json!({"truncated":true,"reason":"payload_capped_at_64kib","original_bytes":raw.len(),"preview":&raw[..end]});
    (
        serde_json::to_string(&cap).unwrap_or_else(|_| "{}".into()),
        cap,
    )
}

const TOOL_CATALOG_JSON: &str = r#"[
{"name":"ohc_list_spaces","description":"List tenant-scoped declarative Task Center Spaces.","scope":"read","requires_owner_or_admin":false,"inputSchema":{"type":"object","additionalProperties":false,"properties":{"include_archived":{"type":"boolean"},"limit":{"type":"integer","minimum":1,"maximum":100}}},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
{"name":"ohc_list_tasks","description":"List tenant-scoped Task Center tasks.","scope":"read","requires_owner_or_admin":false,"inputSchema":{"type":"object","additionalProperties":false,"properties":{"space_id":{"type":"string"},"status":{"type":"string"},"attention_only":{"type":"boolean"},"limit":{"type":"integer","minimum":1,"maximum":100}}},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
{"name":"ohc_get_task","description":"Fetch a single tenant-scoped Task Center task by ID.","scope":"read","requires_owner_or_admin":false,"inputSchema":{"type":"object","additionalProperties":false,"required":["task_id"],"properties":{"task_id":{"type":"string","minLength":1}}},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
{"name":"ohc_list_approvals","description":"List tenant-scoped Task Center approval gates.","scope":"read","requires_owner_or_admin":false,"inputSchema":{"type":"object","additionalProperties":false,"properties":{"status":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":100}}},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
{"name":"ohc_get_usage_budget","description":"Read tenant integer micro-USD budget summary.","scope":"read","requires_owner_or_admin":false,"inputSchema":{"type":"object","additionalProperties":false,"properties":{}},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
{"name":"ohc_list_connectors","description":"List verified tenant connector statuses without credentials.","scope":"read","requires_owner_or_admin":false,"inputSchema":{"type":"object","additionalProperties":false,"properties":{}},"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
{"name":"ohc_create_space","description":"Propose a declarative Space in awaiting_approval state (owner/admin + x-ohc-mcp-write: true).","scope":"write_proposal","requires_owner_or_admin":true,"inputSchema":{"type":"object","additionalProperties":false,"required":["name"],"properties":{"name":{"type":"string","minLength":1,"maxLength":80},"description":{"type":"string"},"kind":{"type":"string"},"prompt_template":{"type":"string"},"department":{"type":"string"},"reads":{"type":"array","items":{"type":"string"}},"shows":{"type":"array","items":{"type":"string"}},"cannot":{"type":"array","items":{"type":"string"}}}},"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false}},
{"name":"ohc_propose_task","description":"Propose a Task Center task in awaiting_approval state (owner/admin + x-ohc-mcp-write: true).","scope":"write_proposal","requires_owner_or_admin":true,"inputSchema":{"type":"object","additionalProperties":false,"required":["title","prompt"],"properties":{"title":{"type":"string","minLength":1,"maxLength":160},"prompt":{"type":"string","minLength":1,"maxLength":8192},"space_id":{"type":"string"},"department":{"type":"string"},"action_kind":{"type":"string"},"estimated_cost_micros":{"type":"integer","minimum":0},"idempotency_key":{"type":"string"}}},"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false}},
{"name":"ohc_propose_email_draft","description":"Create an email draft in awaiting_approval state without sending (owner/admin + x-ohc-mcp-write: true).","scope":"write_proposal","requires_owner_or_admin":true,"inputSchema":{"type":"object","additionalProperties":false,"required":["to","subject","body"],"properties":{"to":{"type":"string","format":"email"},"subject":{"type":"string","minLength":1,"maxLength":200},"body":{"type":"string","minLength":1,"maxLength":32768},"from":{"type":"string","format":"email"},"space_id":{"type":"string"},"department":{"type":"string"}}},"annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false}}
]"#;

const RESOURCE_CATALOG_JSON: &str = r#"[
{"uri":"ohc://spaces","name":"Spaces","description":"Tenant-scoped declarative Task Center Spaces.","mimeType":"application/json"},
{"uri":"ohc://tasks","name":"Tasks","description":"Tenant-scoped Task Center tasks.","mimeType":"application/json"},
{"uri":"ohc://approvals/pending","name":"Pending Approvals","description":"Tenant-scoped pending approval gates.","mimeType":"application/json"},
{"uri":"ohc://budget/summary","name":"Usage Budget Summary","description":"Tenant integer micro-USD budget summary.","mimeType":"application/json"}
]"#;

pub fn tool_catalog() -> Vec<McpToolDescriptor> {
    serde_json::from_str(TOOL_CATALOG_JSON).expect("catalog")
}
pub fn resource_catalog() -> Vec<McpResourceDescriptor> {
    serde_json::from_str(RESOURCE_CATALOG_JSON).expect("resources")
}

fn rpc_err(id: Value, code: i64, m: impl Into<String>) -> JsonRpcResponse {
    JsonRpcResponse::err(id, McpError::new(code, m))
}

pub struct TaskCenterMcpServer<B: TaskCenterMcpBackend> {
    backend: B,
}

impl<B: TaskCenterMcpBackend> TaskCenterMcpServer<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    pub async fn handle_raw_bytes(&self, sc: &McpAccessScope, raw: &[u8]) -> JsonRpcResponse {
        if raw.len() > MCP_MAX_PAYLOAD_BYTES {
            return rpc_err(Value::Null, ERR_PAYLOAD_TOO_LARGE, "Request > 64 KiB");
        }
        match serde_json::from_slice::<JsonRpcRequest>(raw) {
            Ok(req) => self.handle_request(sc, req).await,
            Err(_) => rpc_err(Value::Null, ERR_PARSE, "Invalid JSON-RPC"),
        }
    }

    pub async fn handle_request(
        &self,
        sc: &McpAccessScope,
        req: JsonRpcRequest,
    ) -> JsonRpcResponse {
        let id = req.id.clone().unwrap_or(Value::Null);
        if req.jsonrpc != "2.0" {
            return rpc_err(id, ERR_INVALID_REQUEST, "jsonrpc must be 2.0");
        }
        if let Err(err) = sc.validate() {
            return JsonRpcResponse::err(id, err);
        }
        let out = match req.method.as_str() {
            "initialize" => Ok(
                json!({"protocolVersion":MCP_PROTOCOL_VERSION,"capabilities":{"tools":{"listChanged":false},"resources":{"subscribe":false,"listChanged":false}},"serverInfo":{"name":MCP_SERVER_NAME,"version":MCP_SERVER_VERSION},"instructions":"Read-only by default; write tools require x-ohc-mcp-write: true + owner/admin and only create awaiting_approval proposals."}),
            ),
            "notifications/initialized" | "ping" => Ok(json!({})),
            "tools/list" => {
                let ts: Vec<_> = tool_catalog().into_iter().map(|t| json!({"name":t.name,"description":t.description,"inputSchema":t.input_schema,"annotations":t.annotations})).collect();
                Ok(json!({ "tools": ts }))
            }
            "tools/call" => self.call_tool(sc, req.params).await,
            "resources/list" => Ok(json!({ "resources": resource_catalog() })),
            "resources/read" => self.read_resource(sc, req.params).await,
            m => Err(McpError::new(
                ERR_METHOD_NOT_FOUND,
                format!("Unsupported '{m}'"),
            )),
        };
        let mut resp = match out {
            Ok(mut r) => {
                redact_secrets_in_value(&mut r);
                JsonRpcResponse::ok(id.clone(), r)
            }
            Err(e) => JsonRpcResponse::err(id.clone(), e),
        };
        if serde_json::to_vec(&resp).is_ok_and(|b| b.len() > MCP_MAX_PAYLOAD_BYTES) {
            resp = rpc_err(id, ERR_PAYLOAD_TOO_LARGE, "Response > 64 KiB");
        }
        resp
    }

    async fn call_tool(&self, sc: &McpAccessScope, params: Option<Value>) -> McpResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct P {
            name: String,
            #[serde(default)]
            arguments: Value,
        }
        let p: P = parse(params.ok_or_else(|| McpError::invalid_params("Missing params"))?)?;
        let args = if p.arguments.is_null() {
            json!({})
        } else {
            p.arguments
        };
        let empty = args.as_object().is_some_and(serde_json::Map::is_empty);
        let lim = |l: Option<u32>| Some(l.unwrap_or(50).clamp(1, 100));
        let b = &self.backend;
        let data = match p.name.as_str() {
            "ohc_list_spaces" => {
                let mut a: ListSpacesArgs = parse(args)?;
                a.limit = lim(a.limit);
                b.list_spaces(sc, a).await?
            }
            "ohc_list_tasks" => {
                let mut a: ListTasksArgs = parse(args)?;
                (a.space_id, a.status, a.limit) = (
                    opt_id("space_id", a.space_id)?,
                    opt_id("status", a.status)?,
                    lim(a.limit),
                );
                b.list_tasks(sc, a).await?
            }
            "ohc_get_task" => {
                let mut a: GetTaskArgs = parse(args)?;
                a.task_id = txt("task_id", &a.task_id, 1, 128, false)?;
                b.get_task(sc, a).await?
            }
            "ohc_list_approvals" => {
                let mut a: ListApprovalsArgs = parse(args)?;
                (a.status, a.limit) = (opt_id("status", a.status)?, lim(a.limit));
                b.list_approvals(sc, a).await?
            }
            "ohc_get_usage_budget" if empty => b.get_usage_budget(sc).await?,
            "ohc_list_connectors" if empty => b.list_connectors(sc).await?,
            "ohc_get_usage_budget" | "ohc_list_connectors" => {
                return Err(McpError::invalid_params("No args"));
            }
            "ohc_create_space" | "ohc_propose_task" | "ohc_propose_email_draft" => {
                if !sc.can_write() {
                    let msg = format!("Requires {MCP_WRITE_OPT_IN_HEADER}: true + owner/admin");
                    return Err(McpError::new(ERR_FORBIDDEN, msg));
                }
                let out = match p.name.as_str() {
                    "ohc_create_space" => b.create_space(sc, validate_space(parse(args)?)?).await?,
                    "ohc_propose_task" => b.propose_task(sc, validate_task(parse(args)?)?).await?,
                    _ => {
                        b.propose_email_draft(sc, validate_email(parse(args)?)?)
                            .await?
                    }
                };
                ensure_awaiting_approval_proposal(&out)?;
                out
            }
            u => return Err(McpError::invalid_params(format!("Unknown tool '{u}'"))),
        };
        let (text, st) = cap_json(data, (MCP_MAX_PAYLOAD_BYTES - 4096) / 2);
        Ok(json!({"content":[{"type":"text","text":text}],"structuredContent":st,"isError":false}))
    }

    async fn read_resource(&self, sc: &McpAccessScope, params: Option<Value>) -> McpResult {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct R {
            uri: String,
        }
        let r: R = parse(params.ok_or_else(|| McpError::invalid_params("Missing params"))?)?;
        let b = &self.backend;
        let pend: ListApprovalsArgs = parse(json!({"status":"pending","limit":50}))?;
        let data = match r.uri.as_str() {
            "ohc://spaces" => b.list_spaces(sc, ListSpacesArgs::default()).await?,
            "ohc://tasks" => b.list_tasks(sc, ListTasksArgs::default()).await?,
            "ohc://approvals/pending" => b.list_approvals(sc, pend).await?,
            "ohc://budget/summary" => b.get_usage_budget(sc).await?,
            u => return Err(McpError::not_found(format!("Resource '{u}' not found"))),
        };
        let (text, _) = cap_json(data, MCP_MAX_PAYLOAD_BYTES - 4096);
        Ok(json!({"contents":[{"uri":r.uri,"mimeType":"application/json","text":text}]}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::{connection_vault::ConnectionVault, usage_ledger::UsageLedger};
    use sqlx::SqlitePool;
    use std::collections::BTreeMap;

    type Sc = McpAccessScope;
    type Srv = TaskCenterMcpServer<Db>;
    struct Db(SqlitePool, UsageLedger, ConnectionVault);

    impl Db {
        async fn open() -> (SqlitePool, Self) {
            let opt = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1);
            let pool = opt.connect("sqlite::memory:").await.unwrap();
            let sql = r#"CREATE TABLE mcp_rows(kind TEXT NOT NULL,id TEXT NOT NULL,tenant_id TEXT NOT NULL,payload_json TEXT NOT NULL,PRIMARY KEY(kind,id));
INSERT INTO mcp_rows VALUES('space','spc-a1','tenant-a','{"id":"spc-a1","name":"Ops HQ"}'),('space','spc-b1','tenant-b','{"id":"spc-b1","name":"Private B"}'),('task','tsk-a1','tenant-a','{"id":"tsk-a1","prompt":"sk-proj-supersecretkey1234567890 Bearer topsecretbearertoken12345 re_live1234567890","api_key":"sk-proj-supersecretkey1234567890"}'),('task','tsk-b1','tenant-b','{"id":"tsk-b1","title":"Private B"}'),('approval','apr-a1','tenant-a','{"approval_id":"apr-a1","status":"pending"}');"#;
            sqlx::raw_sql(sql).execute(&pool).await.unwrap();
            let led = UsageLedger::Sqlite(pool.clone());
            led.initialize().await.unwrap();
            led.set_limit("tenant-a", 5_000_000).await.unwrap();
            let keys = BTreeMap::from([("v1".into(), "11".repeat(32))]);
            let vault = ConnectionVault::with_keys(led.clone(), keys, "v1".into()).unwrap();
            vault.initialize().await.unwrap();
            (pool.clone(), Self(pool, led, vault))
        }
        async fn list(&self, f: &str, k: &str, s: &Sc, lim: Option<u32>) -> McpResult {
            let q = "SELECT payload_json FROM mcp_rows WHERE kind=?1 AND tenant_id=?2 LIMIT ?3";
            let q = sqlx::query_scalar::<_, String>(q)
                .bind(k)
                .bind(&s.tenant_id);
            let rows = q
                .bind(i64::from(lim.unwrap_or(50)))
                .fetch_all(&self.0)
                .await
                .unwrap();
            let v: Vec<Value> = rows
                .iter()
                .map(|r| serde_json::from_str(r).unwrap())
                .collect();
            Ok(json!({ f: v }))
        }
        async fn prop(&self, s: &Sc, act: &str, prev: Value) -> McpResult {
            let tid = format!("tsk-{}", uuid::Uuid::new_v4());
            let aid = format!("apr-{}", uuid::Uuid::new_v4());
            let sha = compute_payload_sha256(&prev);
            let row = json!({"id":tid,"status":"awaiting_approval","provenance":"awaiting_approval","sent":false,"pending_approval":{"approval_id":aid,"action_kind":act,"payload_sha256":sha}});
            let q = sqlx::query("INSERT INTO mcp_rows VALUES('task',?1,?2,?3)").bind(&tid);
            q.bind(&s.tenant_id)
                .bind(row.to_string())
                .execute(&self.0)
                .await
                .unwrap();
            Ok(row)
        }
    }

    #[async_trait]
    impl TaskCenterMcpBackend for Db {
        async fn list_spaces(&self, s: &Sc, a: ListSpacesArgs) -> McpResult {
            self.list("spaces", "space", s, a.limit).await
        }
        async fn list_tasks(&self, s: &Sc, a: ListTasksArgs) -> McpResult {
            self.list("tasks", "task", s, a.limit).await
        }
        async fn get_task(&self, s: &Sc, a: GetTaskArgs) -> McpResult {
            let q =
                "SELECT payload_json FROM mcp_rows WHERE kind='task' AND tenant_id=?1 AND id=?2";
            let q = sqlx::query_scalar::<_, String>(q)
                .bind(&s.tenant_id)
                .bind(&a.task_id);
            let r = q.fetch_optional(&self.0).await.unwrap();
            let raw = r.ok_or_else(|| McpError::not_found("Not found"))?;
            Ok(serde_json::from_str(&raw).unwrap())
        }
        async fn list_approvals(&self, s: &Sc, a: ListApprovalsArgs) -> McpResult {
            self.list("approvals", "approval", s, a.limit).await
        }
        async fn get_usage_budget(&self, s: &Sc) -> McpResult {
            let b = self.1.summary(&s.tenant_id).await.unwrap();
            let v = json!({"configured":true,"limit_micros":b.limit_micros,"spent_micros":b.spent_micros,"reserved_micros":b.reserved_micros});
            Ok(v)
        }
        async fn list_connectors(&self, s: &Sc) -> McpResult {
            let c = self.2.list_with_expiry_status(&s.tenant_id).await.unwrap();
            Ok(json!({ "connectors": c }))
        }
        async fn create_space(&self, s: &Sc, i: CreateSpaceInput) -> McpResult {
            self.prop(s, "create_space", json!({"name":i.name})).await
        }
        async fn propose_task(&self, s: &Sc, i: ProposeTaskInput) -> McpResult {
            self.prop(s, &i.action_kind, json!({"title":i.title})).await
        }
        async fn propose_email_draft(&self, s: &Sc, i: ProposeEmailDraftInput) -> McpResult {
            self.prop(s, "send_email", json!({"to":i.to})).await
        }
    }

    async fn rpc(s: &Srv, sc: &Sc, m: &str, p: &str) -> JsonRpcResponse {
        let j = if p.is_empty() {
            format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{m}"}}"#)
        } else {
            format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{m}","params":{p}}}"#)
        };
        s.handle_raw_bytes(sc, j.as_bytes()).await
    }
    async fn tc(s: &Srv, sc: &Sc, n: &str, a: &str) -> JsonRpcResponse {
        let p = format!(r#"{{"name":"{n}","arguments":{a}}}"#);
        rpc(s, sc, "tools/call", &p).await
    }
    fn ec(r: JsonRpcResponse) -> i64 {
        r.error.unwrap().code
    }
    fn ok(r: JsonRpcResponse) -> Value {
        r.result.unwrap()
    }

    #[tokio::test]
    async fn protocol_governance_redaction_and_caps_on_real_sqlite() {
        let (pool, db) = Db::open().await;
        let s = Srv::new(db);
        let ro = Sc::read_only("tenant-a", "u1", vec!["owner".into()]);
        let mem = Sc::read_only("tenant-a", "u2", vec!["member".into()]).with_write_opt_in(true);
        let own = ro.clone().with_write_opt_in(true);

        let r = ok(rpc(&s, &ro, "initialize", "").await);
        assert_eq!(r["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(ok(rpc(&s, &ro, "ping", "").await), json!({}));
        let ni = ok(rpc(&s, &ro, "notifications/initialized", "").await);
        assert_eq!(ni, json!({}));
        let tl = ok(rpc(&s, &ro, "tools/list", "").await);
        assert_eq!(tl["tools"].as_array().unwrap().len(), 9);
        let rl = ok(rpc(&s, &ro, "resources/list", "").await);
        assert_eq!(rl["resources"].as_array().unwrap().len(), 4);

        let sp = ok(tc(&s, &ro, "ohc_list_spaces", "{}").await);
        let st = sp["content"][0]["text"].as_str().unwrap();
        assert!(st.contains("spc-a1") && !st.contains("spc-b1"));
        let x = tc(&s, &ro, "ohc_get_task", r#"{"task_id":"tsk-b1"}"#).await;
        assert_eq!(ec(x), ERR_NOT_FOUND);
        let bd = ok(tc(&s, &ro, "ohc_get_usage_budget", "{}").await);
        assert_eq!(bd["structuredContent"]["limit_micros"], 5_000_000);
        let cn = ok(tc(&s, &ro, "ohc_list_connectors", "{}").await);
        assert!(cn["structuredContent"]["connectors"].is_array());
        let ap = ok(tc(&s, &ro, "ohc_list_approvals", "{}").await);
        let aid = &ap["structuredContent"]["approvals"][0]["approval_id"];
        assert_eq!(aid, "apr-a1");
        let uris = "ohc://spaces,ohc://tasks,ohc://approvals/pending,ohc://budget/summary";
        for u in uris.split(',') {
            let p = format!(r#"{{"uri":"{u}"}}"#);
            assert!(rpc(&s, &ro, "resources/read", &p).await.error.is_none());
        }
        let m = rpc(&s, &ro, "resources/read", r#"{"uri":"ohc://x"}"#).await;
        assert_eq!(ec(m), ERR_NOT_FOUND);

        let writes = [
            "ohc_create_space|{\"name\":\"Cash\",\"kind\":\"cash_recovery\"}",
            "ohc_propose_task|{\"title\":\"Audit\",\"prompt\":\"Due\",\"estimated_cost_micros\":5000}",
            "ohc_propose_email_draft|{\"to\":\"c@example.com\",\"subject\":\"Hi\",\"body\":\"Draft\"}",
        ];
        for row in writes {
            let (n, a) = row.split_once('|').unwrap();
            assert_eq!(ec(tc(&s, &ro, n, a).await), ERR_FORBIDDEN);
            assert_eq!(ec(tc(&s, &mem, n, a).await), ERR_FORBIDDEN);
            let res = ok(tc(&s, &own, n, a).await);
            assert_eq!(res["structuredContent"]["status"], "awaiting_approval");
            assert_eq!(res["structuredContent"]["provenance"], "awaiting_approval");
        }
        let bad = [
            "ohc_propose_task|{\"title\":\"X\",\"prompt\":\"Y\",\"auto_approve\":true}",
            "ohc_propose_task|{\"title\":\"X\",\"prompt\":\"Y\",\"estimated_cost_micros\":-1}",
            "ohc_create_space|{\"name\":\"<script>x</script>\"}",
            "ohc_propose_email_draft|{\"to\":\"bad\",\"subject\":\"S\",\"body\":\"B\"}",
            "ohc_unknown|{}",
        ];
        for row in bad {
            let (n, a) = row.split_once('|').unwrap();
            assert_eq!(ec(tc(&s, &own, n, a).await), ERR_INVALID_PARAMS);
        }
        let done = json!({"status":"completed","provenance":"verified"});
        assert!(ensure_awaiting_approval_proposal(&done).is_err());

        let got = tc(&s, &ro, "ohc_get_task", r#"{"task_id":"tsk-a1"}"#).await;
        let g = serde_json::to_string(&got).unwrap();
        assert!(!g.contains("sk-proj-supersecretkey1234567890"));
        assert!(!g.contains("topsecretbearertoken12345"));
        assert!(!g.contains("re_live1234567890") && g.contains("[REDACTED]"));
        let oa = format!("{}{}.secret-oauth-value-12345", "ya", "29");
        assert!(!redact_secrets_in_str(&oa).contains(&oa));

        let big = json!({"id":"tsk-big","blob":"x".repeat(90 * 1024)}).to_string();
        let q = "INSERT INTO mcp_rows VALUES('task','tsk-big','tenant-a',?1)";
        sqlx::query(q).bind(big).execute(&pool).await.unwrap();
        let buf = vec![b'a'; MCP_MAX_PAYLOAD_BYTES + 1];
        assert_eq!(
            ec(s.handle_raw_bytes(&ro, &buf).await),
            ERR_PAYLOAD_TOO_LARGE
        );
        let cap = tc(&s, &ro, "ohc_list_tasks", "{}").await;
        assert!(serde_json::to_vec(&cap).unwrap().len() <= MCP_MAX_PAYLOAD_BYTES);
        assert_eq!(ok(cap)["structuredContent"]["truncated"], true);
        assert_eq!(ec(s.handle_raw_bytes(&ro, b"{bad").await), ERR_PARSE);
        let bv = br#"{"jsonrpc":"1.0","id":3,"method":"ping"}"#;
        assert_eq!(ec(s.handle_raw_bytes(&ro, bv).await), ERR_INVALID_REQUEST);
        let un = rpc(&s, &Sc::read_only("", "u1", vec![]), "ping", "").await;
        assert_eq!(ec(un), ERR_UNAUTHORIZED);
    }
}
