use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::super::acp::ACP_PROTOCOL_VERSION;
use super::super::harness::HarnessAdapterError;
use super::super::types::{sensitive_json_key, sensitive_json_text};

pub const SURROGATE_TOKEN_PREFIX: &str = "ohc_surr_";

const BLOCKED_EXTRA_ENV_KEYS: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "ANTHROPIC_API_KEY",
    "STRIPE_SECRET_KEY",
    "STRIPE_API_KEY",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SESSION_TOKEN",
    "DATABASE_URL",
    "TWILIO_AUTH_TOKEN",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "HOME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "OHC_SURROGATE_TOKEN",
    "OHC_SENTINEL_PROXY_URL",
    "OHC_TENANT_ID",
    "OHC_SANDBOX_TIER",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "NODE_OPTIONS",
    "BASH_ENV",
    "ENV",
    "PYTHONSTARTUP",
];

const RAW_SECRET_PREFIXES: &[&str] = &[
    "bearer ",
    "basic ",
    "sk-",
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "rk_test_",
    "pk_live_",
    "pk_test_",
    "whsec_",
    "ghp_",
    "gho_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "akia",
    "asia",
    "aiza",
    "-----begin",
];

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpSandboxTier {
    Tier1ToolWasm,
    Tier2LocalOs,
    Tier3CloudMicroVm,
}

impl AcpSandboxTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tier1ToolWasm => "tier1_tool_wasm",
            Self::Tier2LocalOs => "tier2_local_os",
            Self::Tier3CloudMicroVm => "tier3_cloud_micro_vm",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcpMcpBridgeCommand {
    pub server_name: String,
    pub executable: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub allow_write: bool,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl AcpMcpBridgeCommand {
    /// Declarative descriptor wiring `scripts/ohc-mcp-stdio.mjs` into ACP
    /// `session/new.mcpServers` in fail-closed read-only mode by default.
    pub fn ohc_stdio_bridge(
        node_executable: impl Into<String>,
        script_path: impl Into<String>,
    ) -> Self {
        Self {
            server_name: "ohc".to_owned(),
            executable: node_executable.into(),
            args: vec![script_path.into()],
            allow_write: false,
            timeout_ms: None,
        }
    }

    pub fn with_allow_write(mut self, allow_write: bool) -> Self {
        self.allow_write = allow_write;
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    pub fn validate(&self) -> Result<(), HarnessAdapterError> {
        validate_identifier(&self.server_name, "MCP bridge server_name", 64)?;
        validate_command_token(&self.executable, "MCP bridge executable")?;
        for arg in &self.args {
            validate_command_token(arg, "MCP bridge argument")?;
        }
        if let Some(timeout_ms) = self.timeout_ms
            && !(100..=120_000).contains(&timeout_ms)
        {
            return Err(invalid_request(
                "MCP bridge timeout_ms must be between 100 and 120000",
            ));
        }
        Ok(())
    }

    /// Renders an ACP v1 `McpServerStdio` entry scoped strictly to the
    /// configured OHC base URL and opaque Tier 1 surrogate token.
    pub fn to_acp_mcp_server_entry(
        &self,
        ohc_base_url: &str,
        surrogate_token: &str,
    ) -> Result<Value, HarnessAdapterError> {
        self.validate()?;
        let base_url = validate_http_url(ohc_base_url, "OHC MCP base URL")?;
        let token = validate_opaque_surrogate_token(surrogate_token)?;

        let mut args = self.args.clone();
        args.push("--base-url".to_owned());
        args.push(base_url.clone());
        if self.allow_write {
            args.push("--allow-write".to_owned());
        }
        if let Some(timeout_ms) = self.timeout_ms {
            args.push("--timeout-ms".to_owned());
            args.push(timeout_ms.to_string());
        }

        let mut env = vec![
            json!({ "name": "OHC_BASE_URL", "value": base_url }),
            json!({ "name": "OHC_BEARER_TOKEN", "value": token }),
            json!({
                "name": "OHC_MCP_ALLOW_WRITE",
                "value": if self.allow_write { "true" } else { "false" }
            }),
        ];
        if let Some(timeout_ms) = self.timeout_ms {
            env.push(json!({
                "name": "OHC_MCP_TIMEOUT_MS",
                "value": timeout_ms.to_string()
            }));
        }

        Ok(json!({
            "name": self.server_name,
            "command": self.executable,
            "args": args,
            "env": env,
        }))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcpExternalHarnessSpec {
    pub harness_id: String,
    pub executable: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub protocol_version: u16,
    pub supported_sandbox_tiers: BTreeSet<AcpSandboxTier>,
    #[serde(default)]
    pub mcp_bridge_command: Option<AcpMcpBridgeCommand>,
}

impl AcpExternalHarnessSpec {
    pub fn new(
        harness_id: impl Into<String>,
        executable: impl Into<String>,
        args: Vec<String>,
        supported_sandbox_tiers: impl IntoIterator<Item = AcpSandboxTier>,
    ) -> Self {
        Self {
            harness_id: harness_id.into(),
            executable: executable.into(),
            args,
            protocol_version: ACP_PROTOCOL_VERSION,
            supported_sandbox_tiers: supported_sandbox_tiers.into_iter().collect(),
            mcp_bridge_command: None,
        }
    }

    pub fn with_mcp_bridge(mut self, bridge: AcpMcpBridgeCommand) -> Self {
        self.mcp_bridge_command = Some(bridge);
        self
    }

    pub fn supports_tier(&self, tier: AcpSandboxTier) -> bool {
        self.supported_sandbox_tiers.contains(&tier)
    }

    pub fn validate(&self) -> Result<(), HarnessAdapterError> {
        validate_identifier(&self.harness_id, "ACP harness_id", 128)?;
        validate_command_token(&self.executable, "ACP harness executable")?;
        for arg in &self.args {
            validate_command_token(arg, "ACP harness argument")?;
        }
        if self.protocol_version != ACP_PROTOCOL_VERSION {
            return Err(invalid_request(format!(
                "unsupported ACP protocol version {}; expected {}",
                self.protocol_version, ACP_PROTOCOL_VERSION
            )));
        }
        if self.supported_sandbox_tiers.is_empty() {
            return Err(invalid_request(
                "ACP harness spec must declare at least one supported sandbox tier",
            ));
        }
        if let Some(bridge) = &self.mcp_bridge_command {
            bridge.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct AcpHarnessRegistry {
    specs: BTreeMap<String, AcpExternalHarnessSpec>,
}

impl AcpHarnessRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a declarative registry of standard external ACP harnesses without
    /// hardcoding any vendor API endpoints or credentials.
    pub fn with_builtin_specs(mcp_bridge: Option<AcpMcpBridgeCommand>) -> Self {
        let mut registry = Self::new();
        let standard_tiers = [
            AcpSandboxTier::Tier2LocalOs,
            AcpSandboxTier::Tier3CloudMicroVm,
        ];
        for (harness_id, executable, args) in [
            ("codex", "codex-acp", Vec::<String>::new()),
            ("opencode", "opencode", vec!["acp".to_owned()]),
            ("goose", "goose", vec!["acp".to_owned()]),
        ] {
            let mut spec =
                AcpExternalHarnessSpec::new(harness_id, executable, args, standard_tiers);
            if let Some(bridge) = &mcp_bridge {
                spec.mcp_bridge_command = Some(bridge.clone());
            }
            registry
                .register(spec)
                .expect("built-in ACP harness specs are valid");
        }
        registry
    }

    pub fn from_json(raw: &str) -> Result<Self, HarnessAdapterError> {
        let specs: Vec<AcpExternalHarnessSpec> =
            serde_json::from_str(raw).map_err(HarnessAdapterError::Json)?;
        if specs.is_empty() {
            return Err(invalid_request(
                "ACP harness registry JSON must contain at least one harness spec",
            ));
        }
        let mut registry = Self::new();
        for spec in specs {
            if registry.specs.contains_key(&spec.harness_id) {
                return Err(invalid_request(format!(
                    "duplicate ACP harness_id in registry config: {}",
                    spec.harness_id
                )));
            }
            registry.register(spec)?;
        }
        Ok(registry)
    }

    pub fn register(&mut self, spec: AcpExternalHarnessSpec) -> Result<(), HarnessAdapterError> {
        spec.validate()?;
        self.specs.insert(spec.harness_id.clone(), spec);
        Ok(())
    }

    pub fn get(&self, harness_id: &str) -> Option<&AcpExternalHarnessSpec> {
        self.specs.get(harness_id)
    }

    pub fn resolve_for_tier(
        &self,
        harness_id: &str,
        tier: AcpSandboxTier,
    ) -> Result<&AcpExternalHarnessSpec, HarnessAdapterError> {
        let spec = self.specs.get(harness_id).ok_or_else(|| {
            invalid_request(format!("unknown ACP external harness: {harness_id}"))
        })?;
        if !spec.supports_tier(tier) {
            return Err(invalid_request(format!(
                "ACP harness '{harness_id}' is not permitted on sandbox tier {}",
                tier.as_str()
            )));
        }
        Ok(spec)
    }

    pub fn list(&self) -> Vec<&AcpExternalHarnessSpec> {
        self.specs.values().collect()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AcpSanitizedEnvInput {
    pub workspace_dir: PathBuf,
    pub surrogate_token: String,
    pub sentinel_proxy_url: String,
    pub tenant_id: String,
    pub sandbox_tier: AcpSandboxTier,
    pub extra_non_secret_env: BTreeMap<String, String>,
}

impl std::fmt::Debug for AcpSanitizedEnvInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AcpSanitizedEnvInput")
            .field("workspace_dir", &self.workspace_dir)
            .field("surrogate_token", &"<redacted>")
            .field("sentinel_proxy_url", &self.sentinel_proxy_url)
            .field("tenant_id", &self.tenant_id)
            .field("sandbox_tier", &self.sandbox_tier)
            .field(
                "extra_non_secret_env_keys",
                &self.extra_non_secret_env.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Validates that a token is an opaque Tier 1 surrogate (`ohc_surr_...`) or a
/// scoped UUID route token — never a raw provider API key or auth header.
pub fn validate_opaque_surrogate_token(token: &str) -> Result<&str, HarnessAdapterError> {
    if token.is_empty()
        || token.len() > 512
        || token != token.trim()
        || token.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(invalid_request(
            "ACP surrogate token must be a non-empty opaque token without whitespace or control characters",
        ));
    }
    let lowered = token.to_ascii_lowercase();
    if RAW_SECRET_PREFIXES
        .iter()
        .any(|prefix| lowered.starts_with(prefix))
    {
        return Err(invalid_request(
            "raw provider credentials must never be passed as an ACP surrogate token",
        ));
    }
    let valid_surrogate_prefix = token
        .strip_prefix(SURROGATE_TOKEN_PREFIX)
        .is_some_and(|suffix| {
            suffix.len() >= 8
                && suffix
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        });
    let valid_uuid_token = Uuid::parse_str(token).is_ok();
    if !valid_surrogate_prefix && !valid_uuid_token {
        return Err(invalid_request(
            "ACP surrogate token must use the opaque 'ohc_surr_<id>' format or a scoped route UUID",
        ));
    }
    Ok(token)
}

/// Builds the complete explicit child environment for an external ACP harness.
///
/// Paired with `process_env::apply_isolated_environment` (which executes
/// `Command::env_clear()`), this ensures ambient host secrets such as
/// `OPENAI_API_KEY` or `STRIPE_SECRET_KEY` never reach the child process.
pub fn build_sanitized_env(
    input: &AcpSanitizedEnvInput,
) -> Result<BTreeMap<String, String>, HarnessAdapterError> {
    validate_workspace_dir(&input.workspace_dir)?;
    let surrogate_token = validate_opaque_surrogate_token(&input.surrogate_token)?;
    let proxy_url = validate_http_url(&input.sentinel_proxy_url, "Sentinel proxy URL")?;
    validate_identifier(&input.tenant_id, "tenant_id", 255)?;

    let home_dir = input.workspace_dir.join(".ohc-home");
    let tmp_dir = input.workspace_dir.join(".ohc-tmp");
    let xdg_config = home_dir.join(".config");
    let xdg_data = home_dir.join(".local").join("share");
    let xdg_cache = home_dir.join(".cache");

    let mut env = BTreeMap::new();
    for key in ["PATH", "LANG", "LC_ALL"] {
        if let Ok(value) = std::env::var(key)
            && !value.is_empty()
            && !value.chars().any(char::is_control)
        {
            env.insert(key.to_owned(), value);
        }
    }

    let home_str = path_to_utf8(&home_dir, "workspace home directory")?;
    let tmp_str = path_to_utf8(&tmp_dir, "workspace temp directory")?;
    let config_str = path_to_utf8(&xdg_config, "workspace XDG config directory")?;
    let data_str = path_to_utf8(&xdg_data, "workspace XDG data directory")?;
    let cache_str = path_to_utf8(&xdg_cache, "workspace XDG cache directory")?;

    env.insert("HOME".to_owned(), home_str);
    env.insert("TMPDIR".to_owned(), tmp_str.clone());
    env.insert("TMP".to_owned(), tmp_str.clone());
    env.insert("TEMP".to_owned(), tmp_str);
    env.insert("XDG_CONFIG_HOME".to_owned(), config_str);
    env.insert("XDG_DATA_HOME".to_owned(), data_str);
    env.insert("XDG_CACHE_HOME".to_owned(), cache_str);

    // Inject ONLY the opaque Tier 1 surrogate token and Sentinel egress proxy URL.
    env.insert("OHC_SURROGATE_TOKEN".to_owned(), surrogate_token.to_owned());
    env.insert("OHC_SENTINEL_PROXY_URL".to_owned(), proxy_url.clone());
    env.insert("HTTP_PROXY".to_owned(), proxy_url.clone());
    env.insert("HTTPS_PROXY".to_owned(), proxy_url);
    env.insert("OHC_TENANT_ID".to_owned(), input.tenant_id.clone());
    env.insert(
        "OHC_SANDBOX_TIER".to_owned(),
        input.sandbox_tier.as_str().to_owned(),
    );

    for (key, value) in &input.extra_non_secret_env {
        validate_extra_env_entry(key, value)?;
        env.insert(key.clone(), value.clone());
    }

    Ok(env)
}

pub(crate) fn prepare_sanitized_workspace_dirs(
    workspace_dir: &Path,
) -> Result<(), HarnessAdapterError> {
    validate_workspace_dir(workspace_dir)?;
    let home_dir = workspace_dir.join(".ohc-home");
    let tmp_dir = workspace_dir.join(".ohc-tmp");
    for dir in [
        workspace_dir.to_path_buf(),
        home_dir.clone(),
        home_dir.join(".config"),
        home_dir.join(".local").join("share"),
        home_dir.join(".cache"),
        tmp_dir.clone(),
    ] {
        fs::create_dir_all(&dir).map_err(HarnessAdapterError::Io)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for private_dir in [&home_dir, &tmp_dir] {
            fs::set_permissions(private_dir, fs::Permissions::from_mode(0o700))
                .map_err(HarnessAdapterError::Io)?;
        }
    }
    Ok(())
}

pub(crate) fn validate_workspace_dir(workspace_dir: &Path) -> Result<String, HarnessAdapterError> {
    let as_str = path_to_utf8(workspace_dir, "ACP workspace directory")?;
    if as_str.trim().is_empty() || !workspace_dir.is_absolute() {
        return Err(invalid_request(
            "ACP workspace directory must be a non-empty absolute path",
        ));
    }
    if as_str.chars().any(char::is_control) {
        return Err(invalid_request(
            "ACP workspace directory must not contain control characters",
        ));
    }
    Ok(as_str)
}

fn validate_http_url(raw: &str, label: &str) -> Result<String, HarnessAdapterError> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty()
        || trimmed
            .chars()
            .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(invalid_request(format!(
            "{label} must be a non-empty HTTP(S) URL without whitespace or control characters"
        )));
    }
    let parsed = Url::parse(trimmed)
        .map_err(|_| invalid_request(format!("{label} is not a valid HTTP(S) URL")))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(invalid_request(format!(
            "{label} must use http/https and omit embedded credentials, query strings, and fragments"
        )));
    }
    Ok(trimmed.to_owned())
}

fn validate_extra_env_entry(key: &str, value: &str) -> Result<(), HarnessAdapterError> {
    if key.is_empty()
        || key.len() > 128
        || key.contains('=')
        || !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return Err(invalid_request(format!(
            "invalid ACP extra environment variable name: {key}"
        )));
    }
    let upper = key.to_ascii_uppercase();
    if BLOCKED_EXTRA_ENV_KEYS.contains(&upper.as_str()) || sensitive_json_key(key) {
        return Err(invalid_request(format!(
            "environment variable '{key}' is blocked by ACP environment sanitization"
        )));
    }
    if value.chars().any(char::is_control) || sensitive_json_text(value) {
        return Err(invalid_request(format!(
            "environment variable '{key}' contains disallowed control characters or raw secret material"
        )));
    }
    let lowered_value = value.trim().to_ascii_lowercase();
    if RAW_SECRET_PREFIXES
        .iter()
        .any(|prefix| lowered_value.starts_with(prefix))
    {
        return Err(invalid_request(format!(
            "environment variable '{key}' appears to contain a raw credential"
        )));
    }
    Ok(())
}

fn validate_identifier(
    value: &str,
    field: &str,
    max_len: usize,
) -> Result<(), HarnessAdapterError> {
    if value.trim().is_empty()
        || value != value.trim()
        || value.len() > max_len
        || value.chars().any(char::is_control)
    {
        return Err(invalid_request(format!(
            "{field} must be a non-empty trimmed identifier (<= {max_len} chars)"
        )));
    }
    Ok(())
}

fn validate_command_token(value: &str, field: &str) -> Result<(), HarnessAdapterError> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(invalid_request(format!(
            "{field} must be non-empty and contain no control characters"
        )));
    }
    Ok(())
}

fn path_to_utf8(path: &Path, label: &str) -> Result<String, HarnessAdapterError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid_request(format!("{label} must be valid UTF-8")))
}

fn invalid_request(message: impl Into<String>) -> HarnessAdapterError {
    HarnessAdapterError::InvalidRequest(message.into())
}
