//! Production MCP JSON-RPC 2.0 server for discovering, validating, persisting,
//! and generating CSP-safe embeds for OneHumanCorp viral growth widgets.

#[path = "viral_widgets_catalog.rs"]
mod catalog;
#[path = "viral_widgets_validation.rs"]
mod validation;

pub use catalog::EmbedKind;
pub use validation::{RECOMMENDED_HTML_EMBED_CSP, RECOMMENDED_SVG_EMBED_CSP};

use super::client::mcp::Tool;
use catalog::{WidgetSpec, build_widget_catalog};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::{Row, SqlitePool};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use validation::{
    build_embed_snippet, build_preview_url, compute_config_id, csp_for_embed_kind,
    param_to_json_schema, validate_plain_text, validate_tenant_id, validate_widget_params,
};

pub const MCP_PROTOCOL_VERSION: &str = "2025-03-26";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViralWidgetsServerConfig {
    app_origin: String,
}

impl ViralWidgetsServerConfig {
    /// Creates a validated server config. Plaintext HTTP is restricted strictly to loopback origins.
    pub fn new(app_origin: impl Into<String>) -> Result<Self, String> {
        let raw = app_origin.into();
        let trimmed = raw.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return Err("app_origin is required".to_string());
        }
        let parsed = reqwest::Url::parse(trimmed)
            .map_err(|_| "app_origin must be a valid absolute URL".to_string())?;
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || (parsed.path() != "/" && !parsed.path().is_empty())
        {
            return Err(
                "app_origin must not contain credentials, path, query, or fragment".to_string(),
            );
        }
        let host = parsed
            .host_str()
            .ok_or_else(|| "app_origin must include a host".to_string())?;
        match parsed.scheme() {
            "https" => {}
            "http" if matches!(host, "127.0.0.1" | "localhost" | "[::1]") => {}
            _ => {
                return Err(
                    "app_origin must use HTTPS (or loopback HTTP for local development)"
                        .to_string(),
                );
            }
        }
        Ok(Self {
            app_origin: trimmed.to_string(),
        })
    }

    pub fn app_origin(&self) -> &str {
        &self.app_origin
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SavedWidgetConfig {
    pub config_id: String,
    pub tenant_id: String,
    pub widget_id: String,
    pub status: String,
    pub owner_confirmed: bool,
    pub validated_params: Map<String, Value>,
    pub preview_url: String,
    pub embed_html: String,
    pub updated_at: String,
}

enum ConfigStorage {
    Memory(RwLock<BTreeMap<(String, String), SavedWidgetConfig>>),
    Sqlite(SqlitePool),
}

pub struct ViralWidgetsMcpServer {
    config: ViralWidgetsServerConfig,
    catalog: Vec<WidgetSpec>,
    storage: Arc<ConfigStorage>,
}

impl ViralWidgetsMcpServer {
    pub fn new(config: ViralWidgetsServerConfig) -> Self {
        Self {
            config,
            catalog: build_widget_catalog(),
            storage: Arc::new(ConfigStorage::Memory(RwLock::new(BTreeMap::new()))),
        }
    }

    pub async fn with_sqlite(
        config: ViralWidgetsServerConfig,
        pool: SqlitePool,
    ) -> Result<Self, String> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS viral_widget_configs (
                tenant_id TEXT NOT NULL,
                widget_id TEXT NOT NULL,
                config_id TEXT NOT NULL,
                status TEXT NOT NULL,
                owner_confirmed INTEGER NOT NULL,
                validated_params_json TEXT NOT NULL,
                preview_url TEXT NOT NULL,
                embed_html TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (tenant_id, widget_id)
            )",
        )
        .execute(&pool)
        .await
        .map_err(|e| format!("failed to initialize viral_widget_configs schema: {e}"))?;

        Ok(Self {
            config,
            catalog: build_widget_catalog(),
            storage: Arc::new(ConfigStorage::Sqlite(pool)),
        })
    }

    pub fn tools(&self) -> Vec<Tool> {
        vec![
            Tool {
                name: "viral_widgets.list".to_string(),
                description: "List supported OneHumanCorp viral growth widgets with routes, embed endpoints, and parameter summaries.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "category": { "type": "string", "enum": ["referral", "conversion", "social_proof", "gamification", "booking"] },
                        "embed_kind": { "type": "string", "enum": ["iframe", "script", "image"] },
                        "query": { "type": "string", "maxLength": 64 }
                    }
                }),
            },
            Tool {
                name: "viral_widgets.get_schema".to_string(),
                description: "Return strict JSON Schema, defaults, validation bounds, and presets for a specific viral widget ID.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["widget_id"],
                    "properties": {
                        "widget_id": { "type": "string", "minLength": 1, "maxLength": 64 }
                    }
                }),
            },
            Tool {
                name: "viral_widgets.configure".to_string(),
                description: "Validate fail-closed widget parameters, persist tenant-scoped widget config, and emit a preview URL plus CSP-safe embed snippet. Defaults to draft status unless owner_confirmed is true.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["tenant_id", "widget_id", "params"],
                    "properties": {
                        "tenant_id": { "type": "string", "minLength": 1, "maxLength": 64 },
                        "widget_id": { "type": "string", "minLength": 1, "maxLength": 64 },
                        "params": { "type": "object" },
                        "owner_confirmed": { "type": "boolean", "default": false }
                    }
                }),
            },
            Tool {
                name: "viral_widgets.generate_embed".to_string(),
                description: "Generate a CSP-safe, HTML-escaped embed snippet for a viral widget using either inline validated params or the tenant's persisted widget config.".to_string(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["tenant_id", "widget_id"],
                    "properties": {
                        "tenant_id": { "type": "string", "minLength": 1, "maxLength": 64 },
                        "widget_id": { "type": "string", "minLength": 1, "maxLength": 64 },
                        "params": { "type": "object" },
                        "width": { "type": "integer", "minimum": 200, "maximum": 1600 },
                        "height": { "type": "integer", "minimum": 120, "maximum": 1200 },
                        "title": { "type": "string", "maxLength": 100 }
                    }
                }),
            },
        ]
    }

    pub async fn handle_json_rpc(&self, raw_request: &str) -> Value {
        self.handle_json_rpc_for_tenant(raw_request, None).await
    }

    pub async fn handle_json_rpc_for_tenant(
        &self,
        raw_request: &str,
        verified_tenant: Option<&str>,
    ) -> Value {
        let req: Value = match serde_json::from_str(raw_request) {
            Ok(val) => val,
            Err(_) => return jsonrpc_error(Value::Null, -32700, "Parse error: invalid JSON"),
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        if req.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return jsonrpc_error(id, -32600, "Invalid Request: jsonrpc must be '2.0'");
        }
        let Some(method) = req.get("method").and_then(Value::as_str) else {
            return jsonrpc_error(id, -32600, "Invalid Request: method is required");
        };
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

        match method {
            "initialize" => jsonrpc_ok(
                id,
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "ohc-viral-widgets-mcp",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            ),
            "notifications/initialized" | "ping" => jsonrpc_ok(id, json!({})),
            "tools/list" | "listTools" => jsonrpc_ok(id, json!({ "tools": self.tools() })),
            "tools/call" | "callTool" => {
                let Some(tool_name) = params.get("name").and_then(Value::as_str) else {
                    return jsonrpc_error(id, -32602, "Invalid params: 'name' is required");
                };
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match self.call_tool(tool_name, args, verified_tenant).await {
                    Ok(payload) => jsonrpc_ok(
                        id,
                        json!({
                            "content": [{
                                "type": "text",
                                "text": serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string())
                            }],
                            "structuredContent": payload,
                            "isError": false
                        }),
                    ),
                    Err((code, msg)) => jsonrpc_error(id, code, &msg),
                }
            }
            _ => jsonrpc_error(id, -32601, &format!("Method not found: {method}")),
        }
    }

    pub async fn call_tool(
        &self,
        name: &str,
        args: Value,
        verified_tenant: Option<&str>,
    ) -> Result<Value, (i32, String)> {
        let obj = args
            .as_object()
            .ok_or_else(|| (-32602, "Tool arguments must be a JSON object".to_string()))?;

        match name {
            "viral_widgets.list" => self.tool_list(obj),
            "viral_widgets.get_schema" => self.tool_get_schema(obj),
            "viral_widgets.configure" => self.tool_configure(obj, verified_tenant).await,
            "viral_widgets.generate_embed" => self.tool_generate_embed(obj, verified_tenant).await,
            _ => Err((-32601, format!("Unknown tool: {name}"))),
        }
    }

    fn tool_list(&self, args: &Map<String, Value>) -> Result<Value, (i32, String)> {
        for key in args.keys() {
            if !matches!(key.as_str(), "category" | "embed_kind" | "query") {
                return Err((-32602, format!("Unknown parameter '{key}'")));
            }
        }
        let category = optional_str(args, "category")?;
        let embed_kind = optional_str(args, "embed_kind")?;
        let query = optional_str(args, "query")?.map(|q| q.to_ascii_lowercase());

        let widgets: Vec<Value> = self
            .catalog
            .iter()
            .filter(|w| category.is_none_or(|c| w.category == c))
            .filter(|w| {
                embed_kind.is_none_or(|k| {
                    serde_json::to_value(w.embed_kind)
                        .ok()
                        .and_then(|v| v.as_str().map(|s| s == k))
                        .unwrap_or(false)
                })
            })
            .filter(|w| {
                query.as_ref().is_none_or(|q| {
                    w.id.to_ascii_lowercase().contains(q)
                        || w.name.to_ascii_lowercase().contains(q)
                        || w.description.to_ascii_lowercase().contains(q)
                })
            })
            .map(|w| {
                json!({
                    "id": w.id,
                    "name": w.name,
                    "category": w.category,
                    "description": w.description,
                    "embed_kind": w.embed_kind,
                    "builder_route": w.builder_route,
                    "embed_endpoint": w.embed_endpoint,
                    "default_dimensions": { "width": w.default_width, "height": w.default_height },
                    "requires_owner_confirmation": true,
                    "standing_authority_note": w.standing_authority_note,
                    "parameter_names": w.params.iter().map(|p| p.name).collect::<Vec<_>>()
                })
            })
            .collect();

        Ok(json!({
            "app_origin": self.config.app_origin(),
            "count": widgets.len(),
            "widgets": widgets
        }))
    }

    fn tool_get_schema(&self, args: &Map<String, Value>) -> Result<Value, (i32, String)> {
        let widget_id = required_str(args, "widget_id")?;
        let spec = self.find_widget(widget_id)?;
        let mut properties = Map::new();
        let mut required = Vec::new();
        let mut defaults = Map::new();

        for param in &spec.params {
            properties.insert(param.name.to_string(), param_to_json_schema(param));
            defaults.insert(param.name.to_string(), param.default.clone());
            if param.required {
                required.push(Value::String(param.name.to_string()));
            }
        }

        Ok(json!({
            "widget_id": spec.id,
            "name": spec.name,
            "category": spec.category,
            "embed_kind": spec.embed_kind,
            "builder_route": spec.builder_route,
            "embed_endpoint": spec.embed_endpoint,
            "standing_authority_note": spec.standing_authority_note,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "required": required,
                "properties": properties
            },
            "defaults": defaults,
            "presets": spec.presets
        }))
    }

    async fn tool_configure(
        &self,
        args: &Map<String, Value>,
        verified_tenant: Option<&str>,
    ) -> Result<Value, (i32, String)> {
        for key in args.keys() {
            if !matches!(
                key.as_str(),
                "tenant_id" | "widget_id" | "params" | "owner_confirmed"
            ) {
                return Err((-32602, format!("Unknown parameter '{key}'")));
            }
        }
        let tenant_id = validate_tenant_id(required_str(args, "tenant_id")?, verified_tenant)?;
        let widget_id = required_str(args, "widget_id")?;
        let raw_params = args
            .get("params")
            .and_then(Value::as_object)
            .ok_or_else(|| (-32602, "'params' must be a JSON object".to_string()))?;
        let owner_confirmed = match args.get("owner_confirmed") {
            None => false,
            Some(Value::Bool(v)) => *v,
            Some(_) => return Err((-32602, "'owner_confirmed' must be a boolean".to_string())),
        };

        let spec = self.find_widget(widget_id)?;
        let validated = validate_widget_params(spec, raw_params)?;
        let preview_url = build_preview_url(self.config.app_origin(), &tenant_id, spec, &validated);
        let embed_html = build_embed_snippet(
            spec,
            &preview_url,
            spec.default_width,
            spec.default_height,
            spec.name,
        );
        let status = if owner_confirmed {
            "configured_ready_to_embed"
        } else {
            "draft_awaiting_owner_confirmation"
        };
        let saved = SavedWidgetConfig {
            config_id: compute_config_id(&tenant_id, spec.id, &validated),
            tenant_id,
            widget_id: spec.id.to_string(),
            status: status.to_string(),
            owner_confirmed,
            validated_params: validated,
            preview_url,
            embed_html,
            updated_at: Utc::now().to_rfc3339(),
        };

        self.save_config(&saved)
            .await
            .map_err(|e| (-32603, format!("Failed to persist widget config: {e}")))?;

        Ok(json!({
            "config_id": saved.config_id,
            "tenant_id": saved.tenant_id,
            "widget_id": saved.widget_id,
            "status": saved.status,
            "owner_confirmed": saved.owner_confirmed,
            "requires_owner_confirmation": !saved.owner_confirmed,
            "standing_authority_note": spec.standing_authority_note,
            "validated_params": saved.validated_params,
            "preview_url": saved.preview_url,
            "embed_html": saved.embed_html,
            "recommended_csp": csp_for_embed_kind(spec.embed_kind),
            "updated_at": saved.updated_at
        }))
    }

    async fn tool_generate_embed(
        &self,
        args: &Map<String, Value>,
        verified_tenant: Option<&str>,
    ) -> Result<Value, (i32, String)> {
        for key in args.keys() {
            if !matches!(
                key.as_str(),
                "tenant_id" | "widget_id" | "params" | "width" | "height" | "title"
            ) {
                return Err((-32602, format!("Unknown parameter '{key}'")));
            }
        }
        let tenant_id = validate_tenant_id(required_str(args, "tenant_id")?, verified_tenant)?;
        let widget_id = required_str(args, "widget_id")?;
        let spec = self.find_widget(widget_id)?;

        let width = parse_dimension(args.get("width"), 200, 1600, spec.default_width, "width")?;
        let height = parse_dimension(args.get("height"), 120, 1200, spec.default_height, "height")?;
        let title = match args.get("title") {
            None => spec.name.to_string(),
            Some(Value::String(s)) => validate_plain_text("title", s, 100)?,
            Some(_) => return Err((-32602, "'title' must be a string".to_string())),
        };

        let (validated_params, config_status, from_saved) = match args.get("params") {
            Some(Value::Object(raw)) => (
                validate_widget_params(spec, raw)?,
                "ephemeral_unpersisted".to_string(),
                false,
            ),
            Some(_) => return Err((-32602, "'params' must be a JSON object".to_string())),
            None => {
                let saved = self
                    .load_config(&tenant_id, spec.id)
                    .await
                    .map_err(|e| (-32603, format!("Failed to load saved widget config: {e}")))?
                    .ok_or_else(|| {
                        (
                            -32602,
                            format!(
                                "No saved configuration found for tenant '{}' and widget '{}'; call viral_widgets.configure first or pass 'params'",
                                tenant_id, spec.id
                            ),
                        )
                    })?;
                (saved.validated_params, saved.status, true)
            }
        };

        let preview_url = build_preview_url(
            self.config.app_origin(),
            &tenant_id,
            spec,
            &validated_params,
        );
        let embed_html = build_embed_snippet(spec, &preview_url, width, height, &title);

        Ok(json!({
            "tenant_id": tenant_id,
            "widget_id": spec.id,
            "embed_kind": spec.embed_kind,
            "config_status": config_status,
            "from_saved_config": from_saved,
            "preview_url": preview_url,
            "embed_html": embed_html,
            "recommended_csp": csp_for_embed_kind(spec.embed_kind),
            "dimensions": { "width": width, "height": height }
        }))
    }

    fn find_widget(&self, widget_id: &str) -> Result<&WidgetSpec, (i32, String)> {
        self.catalog
            .iter()
            .find(|w| w.id == widget_id)
            .ok_or_else(|| (-32602, format!("Unknown widget_id: '{widget_id}'")))
    }

    async fn save_config(&self, config: &SavedWidgetConfig) -> Result<(), String> {
        match self.storage.as_ref() {
            ConfigStorage::Memory(map) => {
                map.write().await.insert(
                    (config.tenant_id.clone(), config.widget_id.clone()),
                    config.clone(),
                );
                Ok(())
            }
            ConfigStorage::Sqlite(pool) => {
                let params_json =
                    serde_json::to_string(&config.validated_params).map_err(|e| e.to_string())?;
                sqlx::query(
                    "INSERT INTO viral_widget_configs (
                        tenant_id, widget_id, config_id, status, owner_confirmed,
                        validated_params_json, preview_url, embed_html, updated_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                    ON CONFLICT(tenant_id, widget_id) DO UPDATE SET
                        config_id = excluded.config_id,
                        status = excluded.status,
                        owner_confirmed = excluded.owner_confirmed,
                        validated_params_json = excluded.validated_params_json,
                        preview_url = excluded.preview_url,
                        embed_html = excluded.embed_html,
                        updated_at = excluded.updated_at",
                )
                .bind(&config.tenant_id)
                .bind(&config.widget_id)
                .bind(&config.config_id)
                .bind(&config.status)
                .bind(if config.owner_confirmed { 1_i64 } else { 0_i64 })
                .bind(params_json)
                .bind(&config.preview_url)
                .bind(&config.embed_html)
                .bind(&config.updated_at)
                .execute(pool)
                .await
                .map_err(|e| e.to_string())?;
                Ok(())
            }
        }
    }

    pub async fn load_config(
        &self,
        tenant_id: &str,
        widget_id: &str,
    ) -> Result<Option<SavedWidgetConfig>, String> {
        match self.storage.as_ref() {
            ConfigStorage::Memory(map) => Ok(map
                .read()
                .await
                .get(&(tenant_id.to_string(), widget_id.to_string()))
                .cloned()),
            ConfigStorage::Sqlite(pool) => {
                let row = sqlx::query(
                    "SELECT config_id, tenant_id, widget_id, status, owner_confirmed,
                            validated_params_json, preview_url, embed_html, updated_at
                     FROM viral_widget_configs
                     WHERE tenant_id = ?1 AND widget_id = ?2",
                )
                .bind(tenant_id)
                .bind(widget_id)
                .fetch_optional(pool)
                .await
                .map_err(|e| e.to_string())?;
                let Some(row) = row else {
                    return Ok(None);
                };
                let params_str: String = row.get("validated_params_json");
                let validated_params: Map<String, Value> =
                    serde_json::from_str(&params_str).map_err(|e| e.to_string())?;
                Ok(Some(SavedWidgetConfig {
                    config_id: row.get("config_id"),
                    tenant_id: row.get("tenant_id"),
                    widget_id: row.get("widget_id"),
                    status: row.get("status"),
                    owner_confirmed: row.get::<i64, _>("owner_confirmed") != 0,
                    validated_params,
                    preview_url: row.get("preview_url"),
                    embed_html: row.get("embed_html"),
                    updated_at: row.get("updated_at"),
                }))
            }
        }
    }
}

fn jsonrpc_ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn jsonrpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn required_str<'a>(obj: &'a Map<String, Value>, key: &str) -> Result<&'a str, (i32, String)> {
    obj.get(key).and_then(Value::as_str).ok_or_else(|| {
        (
            -32602,
            format!("Missing or non-string required field '{key}'"),
        )
    })
}

fn optional_str<'a>(
    obj: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>, (i32, String)> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err((-32602, format!("Field '{key}' must be a string"))),
    }
}

fn parse_dimension(
    value: Option<&Value>,
    min: u16,
    max: u16,
    default: u16,
    field: &str,
) -> Result<u16, (i32, String)> {
    match value {
        None => Ok(default),
        Some(Value::Number(n)) => {
            let parsed = n
                .as_u64()
                .and_then(|v| u16::try_from(v).ok())
                .ok_or_else(|| (-32602, format!("'{field}' must be an integer")))?;
            if parsed < min || parsed > max {
                return Err((-32602, format!("'{field}' must be between {min} and {max}")));
            }
            Ok(parsed)
        }
        Some(_) => Err((-32602, format!("'{field}' must be an integer"))),
    }
}

#[cfg(test)]
#[path = "viral_widgets_server_test.rs"]
mod tests;
