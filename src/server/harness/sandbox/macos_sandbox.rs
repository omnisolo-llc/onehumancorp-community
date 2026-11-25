use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;

use super::ast::ASTParser;
use super::manager::{SandboxAdapter, SandboxPolicy};
use super::permissions::PermissionEvaluator;
use super::tiers::{
    DeploymentMode, SandboxExecutionRequest, SandboxTier, WorkloadClass, enforce_tier_policy,
};
use crate::telemetry::ViolationStore;

const FORBIDDEN_WRITABLE_SYSTEM_ROOTS: &[&str] = &[
    "/",
    "/System",
    "/Library",
    "/Applications",
    "/Users",
    "/Volumes",
    "/usr",
    "/bin",
    "/sbin",
    "/etc",
    "/private/etc",
    "/var",
    "/private/var",
];

const DEFAULT_RO_SYSTEM_SUBPATHS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library",
    "/private/var/db",
];

/// Typed configuration for the Tier 2 macOS Seatbelt (`sandbox-exec`) local OS sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MacOsSandboxConfig {
    /// Explicitly allowed writable workspace subpaths.
    /// Empty by default -> strict default-deny `(deny file-write*)`.
    pub writable_workspace_paths: Vec<PathBuf>,
    /// Explicitly allowed ephemeral temp scratch subpaths (e.g., per-session temp directory).
    pub temp_scratch_paths: Vec<PathBuf>,
    /// Optional localhost Sentinel proxy TCP port. When `None`, ALL outbound network is denied.
    pub sentinel_proxy_port: Option<u16>,
    /// Optional Sentinel proxy Unix domain socket path.
    pub sentinel_uds_path: Option<PathBuf>,
    /// Active deployment mode (rejects Tier 2 execution in `CloudMultiTenant`).
    pub deployment_mode: DeploymentMode,
    /// Tenant identifier used for tier policy enforcement and violation logging.
    pub tenant_id: String,
}

impl Default for MacOsSandboxConfig {
    fn default() -> Self {
        Self {
            writable_workspace_paths: Vec::new(),
            temp_scratch_paths: Vec::new(),
            sentinel_proxy_port: None,
            sentinel_uds_path: None,
            deployment_mode: DeploymentMode::LocalSingleTenant,
            tenant_id: "local-owner".to_string(),
        }
    }
}

pub struct MacOsSandbox {
    evaluator: PermissionEvaluator,
    policy: SandboxPolicy,
    config: MacOsSandboxConfig,
    violation_store: Arc<ViolationStore>,
}

impl MacOsSandbox {
    pub fn new(pool: Option<PgPool>) -> Self {
        Self::with_config(pool, MacOsSandboxConfig::default())
    }

    pub fn with_config(pool: Option<PgPool>, config: MacOsSandboxConfig) -> Self {
        let violation_store = Arc::new(ViolationStore::new(pool));
        MacOsSandbox {
            evaluator: PermissionEvaluator::new(),
            policy: SandboxPolicy::default(),
            config,
            violation_store,
        }
    }

    pub fn config(&self) -> &MacOsSandboxConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: MacOsSandboxConfig) {
        self.config = config;
    }

    fn enforce_tier_guard(&self) -> Result<(), String> {
        let req = SandboxExecutionRequest {
            tenant_id: self.config.tenant_id.clone(),
            deployment_mode: self.config.deployment_mode,
            workload: WorkloadClass::UntrustedShell,
            requested_tier: SandboxTier::Tier2LocalOs,
        };
        enforce_tier_policy(&req)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn validate_sbpl_path(
        path: &Path,
        field: &str,
        check_writable_root: bool,
    ) -> Result<String, String> {
        if !path.is_absolute() {
            return Err(format!(
                "{} must be an absolute path (fail-closed): {}",
                field,
                path.display()
            ));
        }
        for comp in path.components() {
            if matches!(comp, Component::ParentDir) {
                return Err(format!(
                    "{} must not contain '..' path traversal (fail-closed): {}",
                    field,
                    path.display()
                ));
            }
        }
        let s = path
            .to_str()
            .ok_or_else(|| format!("{} contains non-UTF-8 characters", field))?;
        if s.chars()
            .any(|c| matches!(c, '"' | '\\' | '(' | ')' | '\n' | '\r' | '\0'))
        {
            return Err(format!(
                "{} contains forbidden Seatbelt SBPL characters (fail-closed): {}",
                field, s
            ));
        }
        if check_writable_root {
            let trimmed = s.trim_end_matches('/');
            let normalized = if trimmed.is_empty() { "/" } else { trimmed };
            if FORBIDDEN_WRITABLE_SYSTEM_ROOTS.contains(&normalized) {
                return Err(format!(
                    "{} cannot grant write access to protected system path '{}' (fail-closed)",
                    field, normalized
                ));
            }
        }
        Ok(s.to_string())
    }

    fn effective_sentinel_port(&self) -> Option<u16> {
        self.config
            .sentinel_proxy_port
            .or(self.policy.socat_proxy_port)
            .filter(|port| *port > 0)
    }

    fn effective_sentinel_uds(&self) -> Option<PathBuf> {
        self.config
            .sentinel_uds_path
            .clone()
            .or_else(|| self.policy.socat_socket_path.as_ref().map(PathBuf::from))
    }

    /// Generate a strict default-deny Seatbelt SBPL profile:
    /// - `(deny default)` + `(deny file-write*)` + `(deny network-outbound)`
    /// - Writes allowed ONLY inside explicitly configured writable workspace subpaths + temp scratch.
    /// - Outbound network denied EXCEPT localhost Sentinel proxy port or UDS socket.
    pub fn generate_profile(&self) -> Result<String, String> {
        self.enforce_tier_guard()?;

        let mut profile = String::from("(version 1)\n");
        profile.push_str("(deny default)\n");
        profile.push_str("(deny file-write*)\n");
        profile.push_str("(deny network*)\n");
        profile.push_str("(deny network-inbound)\n");
        profile.push_str("(deny network-bind)\n");
        profile.push_str("(deny network-outbound)\n");

        // Minimal process execution primitives and read-only system runtime paths.
        profile.push_str("(allow process-exec)\n");
        profile.push_str("(allow process-fork)\n");
        profile.push_str("(allow sysctl-read)\n");
        profile.push_str("(allow mach-lookup)\n");
        profile.push_str("(allow file-read-metadata)\n");
        profile.push_str("(allow file-read* (literal \"/dev/null\") (literal \"/dev/urandom\") (literal \"/dev/random\"))\n");
        profile.push_str("(allow file-write-data (literal \"/dev/null\"))\n");

        for sys_path in DEFAULT_RO_SYSTEM_SUBPATHS {
            profile.push_str(&format!("(allow file-read* (subpath \"{}\"))\n", sys_path));
        }

        // Allow reads and writes ONLY inside explicitly configured writable workspace subpaths.
        for ws_path in &self.config.writable_workspace_paths {
            let safe = Self::validate_sbpl_path(ws_path, "writable_workspace_paths", true)?;
            profile.push_str(&format!("(allow file-read* (subpath \"{}\"))\n", safe));
            profile.push_str(&format!("(allow file-write* (subpath \"{}\"))\n", safe));
        }

        // Allow reads and writes inside explicitly configured ephemeral temp scratch subpaths.
        for scratch_path in &self.config.temp_scratch_paths {
            let safe = Self::validate_sbpl_path(scratch_path, "temp_scratch_paths", true)?;
            profile.push_str(&format!("(allow file-read* (subpath \"{}\"))\n", safe));
            profile.push_str(&format!("(allow file-write* (subpath \"{}\"))\n", safe));
        }

        // In Seatbelt SBPL, later matching rules take precedence: re-apply read_only_paths LAST
        // so protected subpaths (such as .git/hooks inside a workspace) remain non-writable.
        for ro_path in &self.policy.read_only_paths {
            let safe = Self::validate_sbpl_path(Path::new(ro_path), "read_only_paths", false)?;
            profile.push_str(&format!("(deny file-write* (subpath \"{}\"))\n", safe));
        }

        // Outbound network: strict default-deny EXCEPT explicitly configured localhost Sentinel proxy.
        if let Some(port) = self.effective_sentinel_port() {
            profile.push_str(&format!(
                "(allow network-outbound (remote tcp \"127.0.0.1:{}\"))\n",
                port
            ));
            profile.push_str(&format!(
                "(allow network-outbound (remote tcp \"localhost:{}\"))\n",
                port
            ));
        }

        if let Some(uds_path) = self.effective_sentinel_uds() {
            let safe = Self::validate_sbpl_path(&uds_path, "sentinel_uds_path", false)?;
            profile.push_str(&format!(
                "(allow network-outbound (remote unix-socket (path-literal \"{}\")))\n",
                safe
            ));
        }

        // Defense-in-depth explicit denies for any policy blocked domains.
        for domain in &self.policy.blocked_domains {
            if domain
                .chars()
                .any(|c| matches!(c, '"' | '\\' | '(' | ')' | '\n' | '\r' | '\0'))
            {
                return Err(format!(
                    "blocked_domains entry contains forbidden Seatbelt characters: {}",
                    domain
                ));
            }
            profile.push_str(&format!(
                "(deny network-outbound (remote ip \"{}\"))\n",
                domain
            ));
        }

        Ok(profile)
    }
}

#[async_trait]
impl SandboxAdapter for MacOsSandbox {
    async fn wrap_command(&self, cmd: &str) -> Result<String, String> {
        self.enforce_tier_guard()?;
        if cmd.trim().is_empty() {
            return Err("Command must not be empty (fail-closed)".to_string());
        }

        let mut ast_parser = ASTParser::new();
        if let Err(reason) = ast_parser.parse_for_security(cmd) {
            let details = json!({ "command": cmd, "reason": reason });
            let _ = self
                .violation_store
                .record_violation(
                    &self.config.tenant_id,
                    "macos_sandbox",
                    "local_session",
                    "ast_security_violation",
                    details,
                )
                .await;
            return Err(reason);
        }

        if !self.evaluator.evaluate(cmd) {
            let details = json!({ "command": cmd });
            let _ = self
                .violation_store
                .record_violation(
                    &self.config.tenant_id,
                    "macos_sandbox",
                    "local_session",
                    "command_execution",
                    details,
                )
                .await;
            return Err("Command execution denied by sandbox policy".to_string());
        }

        let profile = self.generate_profile()?;
        let escaped_profile = profile.replace('\'', "'\\''");
        let escaped_cmd = cmd.replace('\'', "'\\''");

        Ok(format!(
            "sandbox-exec -p '{}' bash -c '{}'",
            escaped_profile, escaped_cmd
        ))
    }

    async fn update_config(&mut self, policy_json: &str) -> Result<(), String> {
        let policy: SandboxPolicy =
            serde_json::from_str(policy_json).map_err(|e| format!("Invalid policy JSON: {}", e))?;

        self.evaluator.update_policy(policy.clone());
        self.policy = policy;
        Ok(())
    }

    fn annotate_error(&self, err: String, stdout: String) -> String {
        format!("SANDBOX_FAILURE: {}\nSTDOUT:\n{}", err, stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_seatbelt_profile_is_strict_default_deny_for_writes_and_network() {
        let sandbox = MacOsSandbox::new(None);
        let profile = sandbox.generate_profile().unwrap();

        assert!(profile.contains("(version 1)"));
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(deny network-outbound)"));
        assert!(profile.contains("(deny network-bind)"));

        // Must NEVER contain blanket default-allow file-write or outbound network rules!
        assert!(!profile.contains("(allow file-write*)\n"));
        assert!(!profile.contains("(allow network-outbound)\n"));
        assert!(!profile.contains("(allow network-bind)\n"));
        assert!(!profile.contains("(allow file-read*)\n"));
    }

    #[test]
    fn seatbelt_profile_scopes_writes_to_workspace_and_temp_and_network_to_sentinel() {
        let cfg = MacOsSandboxConfig {
            writable_workspace_paths: vec![PathBuf::from("/private/tmp/ohc-ws-project")],
            temp_scratch_paths: vec![PathBuf::from("/private/tmp/ohc-scratch-123")],
            sentinel_proxy_port: Some(18080),
            sentinel_uds_path: Some(PathBuf::from("/private/tmp/ohc-sentinel.sock")),
            ..Default::default()
        };

        let sandbox = MacOsSandbox::with_config(None, cfg);
        let profile = sandbox.generate_profile().unwrap();

        assert!(profile.contains("(deny file-write*)"));
        assert!(profile.contains("(allow file-write* (subpath \"/private/tmp/ohc-ws-project\"))"));
        assert!(profile.contains("(allow file-write* (subpath \"/private/tmp/ohc-scratch-123\"))"));
        assert!(profile.contains("(deny network-outbound)"));
        assert!(profile.contains("(allow network-outbound (remote tcp \"127.0.0.1:18080\"))"));
        assert!(profile.contains("(allow network-outbound (remote tcp \"localhost:18080\"))"));
        assert!(profile.contains(
            "(allow network-outbound (remote unix-socket (path-literal \"/private/tmp/ohc-sentinel.sock\")))"
        ));
        assert!(!profile.contains("(allow file-write*)\n"));
        assert!(!profile.contains("(allow network-outbound)\n"));
    }

    #[test]
    fn seatbelt_rejects_system_roots_traversal_sbpl_injection_and_cloud_mode() {
        for bad in [
            "/",
            "/System",
            "/Users",
            "/tmp/../etc",
            "/tmp/evil\") (allow file-write*) (\"",
        ] {
            let cfg = MacOsSandboxConfig {
                writable_workspace_paths: vec![PathBuf::from(bad)],
                ..Default::default()
            };
            let sandbox = MacOsSandbox::with_config(None, cfg);
            assert!(
                sandbox.generate_profile().is_err(),
                "must fail closed on dangerous path '{}'",
                bad
            );
        }

        let cloud_cfg = MacOsSandboxConfig {
            deployment_mode: DeploymentMode::CloudMultiTenant,
            ..Default::default()
        };
        let cloud_sandbox = MacOsSandbox::with_config(None, cloud_cfg);
        let err = cloud_sandbox
            .generate_profile()
            .expect_err("CloudMultiTenant must reject Tier2 MacOsSandbox");
        assert!(err.contains("Tier3CloudMicroVm"));
    }

    #[tokio::test]
    async fn update_config_and_wrap_command_enforces_readonly_overrides_and_ast() {
        let cfg = MacOsSandboxConfig {
            writable_workspace_paths: vec![PathBuf::from("/private/tmp/ohc-ws")],
            ..Default::default()
        };
        let mut sandbox = MacOsSandbox::with_config(None, cfg);

        let policy_json = r#"{
            "read_only_paths": ["/private/tmp/ohc-ws/.git/hooks", "/etc"],
            "blocked_domains": ["169.254.169.254"],
            "socat_proxy_port": 19090
        }"#;

        sandbox.update_config(policy_json).await.unwrap();
        let profile = sandbox.generate_profile().unwrap();

        // Deny rule for read_only_paths must appear AFTER the workspace allow rule.
        let ws_allow_pos = profile
            .find("(allow file-write* (subpath \"/private/tmp/ohc-ws\"))")
            .unwrap();
        let hooks_deny_pos = profile
            .find("(deny file-write* (subpath \"/private/tmp/ohc-ws/.git/hooks\"))")
            .unwrap();
        assert!(hooks_deny_pos > ws_allow_pos);
        assert!(profile.contains("(allow network-outbound (remote tcp \"127.0.0.1:19090\"))"));
        assert!(profile.contains("(deny network-outbound (remote ip \"169.254.169.254\"))"));

        let wrapped = sandbox.wrap_command("echo 'hello world'").await.unwrap();
        assert!(wrapped.starts_with("sandbox-exec -p '"));
        assert!(wrapped.contains("(deny file-write*)"));
        assert!(wrapped.contains("bash -c 'echo '\\''hello world'\\'''"));

        let ast_err = sandbox
            .wrap_command("echo test > >(cat)")
            .await
            .unwrap_err();
        assert_eq!(
            ast_err,
            "Dangerous pattern detected: >() process substitution"
        );

        let eval_err = sandbox.wrap_command("rm -rf /").await.unwrap_err();
        assert_eq!(eval_err, "Command execution denied by sandbox policy");
    }
}
