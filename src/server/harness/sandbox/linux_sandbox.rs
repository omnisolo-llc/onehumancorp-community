use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;

use ::server_telemetry::record_bubblewrap_violation;

use super::ast::ASTParser;
use super::manager::{SandboxAdapter, SandboxPolicy};
use super::permissions::PermissionEvaluator;
use super::tiers::{
    DeploymentMode, SandboxExecutionRequest, SandboxTier, WorkloadClass, enforce_tier_policy,
};
use crate::telemetry::ViolationStore;

const FORBIDDEN_WRITABLE_SYSTEM_ROOTS: &[&str] = &[
    "/", "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/boot", "/root", "/proc", "/sys",
    "/dev", "/var",
];

const DEFAULT_RO_SYSTEM_PATHS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"];

/// Typed configuration for the Tier 2 Linux Bubblewrap (`bwrap`) local OS sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinuxSandboxConfig {
    /// Executable name or absolute path for `bwrap`.
    pub bwrap_binary: PathBuf,
    /// Single workspace directory explicitly mounted read-write (`--bind`).
    /// When `None`, no persistent host directory is writable (strict default-deny read-only).
    pub writable_workspace_dir: Option<PathBuf>,
    /// System directories mounted read-only (`--ro-bind` / `--ro-bind-try`).
    /// Host `/` is never bound read-write; host `$HOME` and sibling directories are omitted.
    pub ro_system_paths: Vec<PathBuf>,
    /// Optional Sentinel proxy Unix domain socket path bound into the network-isolated sandbox.
    pub sentinel_uds_path: Option<PathBuf>,
    /// Optional localhost Sentinel proxy TCP port bridged into the sandbox via `socat`.
    pub sentinel_proxy_port: Option<u16>,
    /// Explicit local-dev opt-out when `bwrap` is unavailable.
    /// Defaults to `false` (fail-closed) and is NEVER honored in `CloudMultiTenant` mode.
    pub allow_insecure_dev_without_bwrap: bool,
    /// Active deployment mode (rejects Tier 2 execution in `CloudMultiTenant`).
    pub deployment_mode: DeploymentMode,
    /// Tenant identifier used for tier policy enforcement and violation audit logs.
    pub tenant_id: String,
}

impl Default for LinuxSandboxConfig {
    fn default() -> Self {
        Self {
            bwrap_binary: PathBuf::from("bwrap"),
            writable_workspace_dir: None,
            ro_system_paths: DEFAULT_RO_SYSTEM_PATHS.iter().map(PathBuf::from).collect(),
            sentinel_uds_path: None,
            sentinel_proxy_port: None,
            allow_insecure_dev_without_bwrap: false,
            deployment_mode: DeploymentMode::LocalSingleTenant,
            tenant_id: "local-owner".to_string(),
        }
    }
}

/// Output from a real `bwrap` execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxSandboxOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub struct LinuxSandbox {
    evaluator: PermissionEvaluator,
    policy: SandboxPolicy,
    config: LinuxSandboxConfig,
    violation_store: Arc<ViolationStore>,
}

impl LinuxSandbox {
    pub fn new(pool: Option<PgPool>) -> Self {
        Self::with_config(pool, LinuxSandboxConfig::default())
    }

    pub fn with_config(pool: Option<PgPool>, config: LinuxSandboxConfig) -> Self {
        let violation_store = Arc::new(ViolationStore::new(pool));
        LinuxSandbox {
            evaluator: PermissionEvaluator::new(),
            policy: SandboxPolicy::default(),
            config,
            violation_store,
        }
    }

    pub fn config(&self) -> &LinuxSandboxConfig {
        &self.config
    }

    pub fn set_config(&mut self, config: LinuxSandboxConfig) {
        self.config = config;
    }

    fn validate_absolute_clean_path(path: &Path, field: &str) -> Result<String, String> {
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
        if s.contains('\0') || s.contains('\n') || s.contains('\r') {
            return Err(format!("{} contains forbidden control characters", field));
        }
        Ok(s.to_string())
    }

    fn validate_writable_workspace(path: &Path) -> Result<String, String> {
        let rendered = Self::validate_absolute_clean_path(path, "writable_workspace_dir")?;
        let trimmed = rendered.trim_end_matches('/');
        let normalized = if trimmed.is_empty() { "/" } else { trimmed };
        if FORBIDDEN_WRITABLE_SYSTEM_ROOTS.contains(&normalized) {
            return Err(format!(
                "writable_workspace_dir cannot target protected system path '{}' (fail-closed)",
                normalized
            ));
        }
        Ok(rendered)
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

    /// Resolve and verify that the configured `bwrap` binary exists and is executable.
    pub fn check_bwrap_available(&self) -> Result<PathBuf, String> {
        resolve_executable(&self.config.bwrap_binary).ok_or_else(|| {
            format!(
                "Bubblewrap binary '{}' is unavailable; failing closed instead of running unsandboxed",
                self.config.bwrap_binary.display()
            )
        })
    }

    fn effective_sentinel_uds(&self) -> Option<PathBuf> {
        self.config
            .sentinel_uds_path
            .clone()
            .or_else(|| self.policy.socat_socket_path.as_ref().map(PathBuf::from))
    }

    fn effective_sentinel_port(&self) -> Option<u16> {
        self.config
            .sentinel_proxy_port
            .or(self.policy.socat_proxy_port)
            .filter(|port| *port > 0)
    }

    /// Build hardened `bwrap` arguments enforcing default-deny filesystem, PID/net isolation,
    /// capability dropping, and optional workspace/Sentinel UDS bindings.
    pub fn generate_bwrap_args(&self) -> Result<Vec<String>, String> {
        self.enforce_tier_guard()?;

        if self.config.ro_system_paths.is_empty() {
            return Err("ro_system_paths must not be empty (fail-closed)".to_string());
        }

        // Mandatory namespace, lifecycle, privilege, and minimal virtual-fs isolation:
        // - --unshare-net detaches the network namespace so traffic can only exit via Sentinel proxy.
        // - --unshare-pid / --unshare-ipc / --unshare-uts isolate process and IPC visibility.
        // - --die-with-parent prevents orphan background processes after turn cancel/exit.
        let mut args: Vec<String> = [
            "--unshare-net",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        // Default-deny filesystem: read-only bind mounts ONLY for configured system paths.
        // Optional paths (like /lib64 or /sbin on non-standard arches) use --ro-bind-try so
        // missing optional directories do not break startup, while /usr and /etc are strict --ro-bind.
        for path in &self.config.ro_system_paths {
            let rendered = Self::validate_absolute_clean_path(path, "ro_system_paths")?;
            let is_optional = matches!(rendered.as_str(), "/lib64" | "/sbin");
            args.push(if is_optional {
                "--ro-bind-try".to_string()
            } else {
                "--ro-bind".to_string()
            });
            args.push(rendered.clone());
            args.push(rendered);
        }

        // Explicitly bind ONLY the configured workspace directory read-write.
        if let Some(workspace) = &self.config.writable_workspace_dir {
            let rendered = Self::validate_writable_workspace(workspace)?;
            args.push("--bind".to_string());
            args.push(rendered.clone());
            args.push(rendered.clone());
            args.push("--chdir".to_string());
            args.push(rendered);
        }

        // Bind Sentinel UDS socket into the network-isolated sandbox when configured.
        if let Some(uds_path) = self.effective_sentinel_uds() {
            let rendered = Self::validate_absolute_clean_path(&uds_path, "sentinel_uds_path")?;
            args.push("--bind-try".to_string());
            args.push(rendered.clone());
            args.push(rendered);
        }

        // Apply policy read_only_paths LAST so read-only subpaths inside a workspace override --bind.
        for path in &self.policy.read_only_paths {
            let rendered = Self::validate_absolute_clean_path(Path::new(path), "read_only_paths")?;
            args.push("--ro-bind-try".to_string());
            args.push(rendered.clone());
            args.push(rendered);
        }

        if let Some(fd) = self.policy.seccomp_fd {
            if fd < 0 {
                return Err(format!("seccomp_fd must be non-negative, got {}", fd));
            }
            args.push("--seccomp".to_string());
            args.push(fd.to_string());
        }

        Ok(args)
    }

    async fn validate_command(&self, cmd: &str) -> Result<(), String> {
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
                    "linux_sandbox",
                    "local_session",
                    "ast_security_violation",
                    details,
                )
                .await;
            record_bubblewrap_violation("linux_sandbox", "local_session", "ast_security_violation");
            return Err(reason);
        }

        if !self.evaluator.evaluate(cmd) {
            let details = json!({ "command": cmd });
            let _ = self
                .violation_store
                .record_violation(
                    &self.config.tenant_id,
                    "linux_sandbox",
                    "local_session",
                    "command_execution",
                    details,
                )
                .await;
            record_bubblewrap_violation(
                "linux_sandbox",
                "local_session",
                "command_execution_denied",
            );
            return Err("Command execution denied by sandbox policy".to_string());
        }

        Ok(())
    }

    fn build_inner_shell_script(&self, cmd: &str) -> String {
        let mut prefix = String::from("set -e; umask 077; ");
        if !self.policy.blocked_domains.is_empty() {
            prefix.push_str(&format!(
                "export BLOCKED_DOMAINS={}; ",
                shell_single_quote(&self.policy.blocked_domains.join(","))
            ));
        }
        if let Some(uds_path) = self.effective_sentinel_uds() {
            prefix.push_str(&format!(
                "export OHC_SENTINEL_UDS={}; ",
                shell_single_quote(&uds_path.to_string_lossy())
            ));
        }
        format!("{}{}", prefix, cmd)
    }

    /// Execute a command directly inside a real `bwrap` subprocess (no shell-quoting ambiguity).
    pub async fn execute_command(
        &self,
        cmd: &str,
        timeout: Duration,
    ) -> Result<LinuxSandboxOutput, String> {
        self.enforce_tier_guard()?;
        self.validate_command(cmd).await?;

        let bwrap_path = self.check_bwrap_available()?;
        let mut argv = self.generate_bwrap_args()?;
        argv.push("--".to_string());
        argv.push("/bin/bash".to_string());
        argv.push("-c".to_string());
        argv.push(self.build_inner_shell_script(cmd));

        let mut command = tokio::process::Command::new(&bwrap_path);
        command
            .args(&argv)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("HOME", "/tmp")
            .env("TMPDIR", "/tmp")
            .kill_on_drop(true);

        let output = tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| format!("Linux sandbox command timed out after {:?}", timeout))?
            .map_err(|e| format!("Failed to spawn bwrap '{}': {}", bwrap_path.display(), e))?;

        Ok(LinuxSandboxOutput {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[async_trait]
impl SandboxAdapter for LinuxSandbox {
    async fn wrap_command(&self, cmd: &str) -> Result<String, String> {
        self.enforce_tier_guard()?;
        self.validate_command(cmd).await?;

        let inner_script = self.build_inner_shell_script(cmd);
        let bwrap_binary = match self.check_bwrap_available() {
            Ok(path) => path,
            Err(err) => {
                if self.config.allow_insecure_dev_without_bwrap
                    && self.config.deployment_mode == DeploymentMode::LocalSingleTenant
                {
                    tracing::warn!(
                        "LinuxSandbox running with explicit local dev opt-out without bwrap: {}",
                        err
                    );
                    return Ok(format!(
                        "OHC_DEV_UNSANDBOXED=1 bash -c {}",
                        shell_single_quote(&inner_script)
                    ));
                }
                return Err(err);
            }
        };

        let bwrap_args = self.generate_bwrap_args()?;
        let rendered_args = bwrap_args
            .iter()
            .map(|arg| shell_arg(arg))
            .collect::<Vec<_>>()
            .join(" ");

        let bwrap_cmd = format!(
            "{} {} -- bash -c {}",
            shell_arg(&bwrap_binary.to_string_lossy()),
            rendered_args,
            shell_single_quote(&inner_script)
        );

        if let (Some(uds_path), Some(port)) = (
            self.effective_sentinel_uds(),
            self.effective_sentinel_port(),
        ) {
            let uds_str = Self::validate_absolute_clean_path(&uds_path, "sentinel_uds_path")?;
            let quoted_uds = shell_single_quote(&uds_str);
            let proxy_script = format!(
                "socat UNIX-LISTEN:{},fork TCP:127.0.0.1:{} &\n\
                 SOCAT_PID=$!\n\
                 trap 'kill $SOCAT_PID 2>/dev/null || true' EXIT\n\
                 while [ ! -S {} ]; do sleep 0.05; done\n\
                 {}",
                quoted_uds, port, quoted_uds, bwrap_cmd
            );
            Ok(format!("bash -c {}", shell_single_quote(&proxy_script)))
        } else {
            Ok(bwrap_cmd)
        }
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

fn shell_single_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

fn shell_arg(arg: &str) -> String {
    if !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '/' | '.' | ':' | '=' | ',')
        })
    {
        arg.to_string()
    } else {
        shell_single_quote(arg)
    }
}

fn resolve_executable(bin: &Path) -> Option<PathBuf> {
    if bin.components().count() > 1 || bin.is_absolute() {
        return is_executable_file(bin).then(|| bin.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bwrap_args_enforce_default_deny_fs_and_net_isolation() {
        let sandbox = LinuxSandbox::new(None);
        let args = sandbox.generate_bwrap_args().unwrap();

        assert!(args.contains(&"--unshare-net".to_string()));
        assert!(args.contains(&"--unshare-pid".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
        assert!(args.contains(&"--cap-drop".to_string()));
        assert!(args.contains(&"ALL".to_string()));
        assert!(args.contains(&"--tmpfs".to_string()));
        assert!(args.contains(&"/tmp".to_string()));
        assert!(args.contains(&"--ro-bind".to_string()));
        assert!(args.contains(&"/usr".to_string()));
        assert!(!args.contains(&"--share-net".to_string()));
        // Default config has no writable workspace dir bound.
        assert!(!args.contains(&"--bind".to_string()));
    }

    #[test]
    fn workspace_dir_is_only_writable_bind_and_system_roots_are_rejected() {
        let cfg = LinuxSandboxConfig {
            writable_workspace_dir: Some(PathBuf::from("/tmp/ohc-test-ws")),
            ..Default::default()
        };
        let sandbox = LinuxSandbox::with_config(None, cfg);
        let args = sandbox.generate_bwrap_args().unwrap();

        let bind_idx = args.iter().position(|a| a == "--bind").unwrap();
        assert_eq!(args[bind_idx + 1], "/tmp/ohc-test-ws");
        assert_eq!(args[bind_idx + 2], "/tmp/ohc-test-ws");

        for forbidden in ["/", "/etc", "/usr", "/tmp/../etc"] {
            let bad_cfg = LinuxSandboxConfig {
                writable_workspace_dir: Some(PathBuf::from(forbidden)),
                ..Default::default()
            };
            let bad_sandbox = LinuxSandbox::with_config(None, bad_cfg);
            assert!(
                bad_sandbox.generate_bwrap_args().is_err(),
                "must reject dangerous writable workspace '{}'",
                forbidden
            );
        }
    }

    #[tokio::test]
    async fn fails_closed_when_bwrap_missing_unless_explicit_local_dev_opt_out() {
        let mut cfg = LinuxSandboxConfig {
            bwrap_binary: PathBuf::from("/nonexistent/ohc-missing-bwrap"),
            ..Default::default()
        };
        let strict = LinuxSandbox::with_config(None, cfg.clone());
        let err = strict
            .wrap_command("echo hello")
            .await
            .expect_err("must fail closed when bwrap is missing");
        assert!(err.contains("failing closed"));

        cfg.allow_insecure_dev_without_bwrap = true;
        let dev_opt_out = LinuxSandbox::with_config(None, cfg.clone());
        let wrapped = dev_opt_out.wrap_command("echo hello").await.unwrap();
        assert!(wrapped.starts_with("OHC_DEV_UNSANDBOXED=1 bash -c "));

        // Even with dev opt-out set, CloudMultiTenant MUST ALWAYS fail closed!
        cfg.deployment_mode = DeploymentMode::CloudMultiTenant;
        let cloud_attempt = LinuxSandbox::with_config(None, cfg);
        let cloud_err = cloud_attempt
            .wrap_command("echo hello")
            .await
            .expect_err("CloudMultiTenant must reject Tier2 LinuxSandbox even with dev opt-out");
        assert!(cloud_err.contains("Tier3CloudMicroVm"));
    }

    #[tokio::test]
    async fn wrap_command_blocks_ast_and_policy_violations_and_wires_sentinel_proxy() {
        let mut sandbox = LinuxSandbox::new(None);
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

        let policy_json = r#"{
            "read_only_paths": ["/etc", "/var/log"],
            "blocked_domains": ["169.254.169.254"],
            "seccomp_fd": 11,
            "socat_socket_path": "/tmp/ohc-sentinel.sock",
            "socat_proxy_port": 18080
        }"#;
        sandbox.update_config(policy_json).await.unwrap();
        let args = sandbox.generate_bwrap_args().unwrap();
        assert!(args.contains(&"--seccomp".to_string()));
        assert!(args.contains(&"11".to_string()));
        assert!(args.contains(&"/tmp/ohc-sentinel.sock".to_string()));

        if sandbox.check_bwrap_available().is_ok() {
            let wrapped = sandbox.wrap_command("echo 'hello world'").await.unwrap();
            assert!(wrapped.contains("socat UNIX-LISTEN:"));
            assert!(wrapped.contains("127.0.0.1:18080"));
            assert!(wrapped.contains("--unshare-net"));
        }
    }

    #[tokio::test]
    async fn real_bwrap_execution_enforces_fs_and_net_isolation_when_installed() {
        let base_dir =
            std::env::temp_dir().join(format!("ohc-bwrap-test-{}", uuid::Uuid::new_v4()));
        let workspace_dir = base_dir.join("workspace");
        let hidden_sibling = base_dir.join("secret-sibling");
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::create_dir_all(&hidden_sibling).unwrap();
        std::fs::write(hidden_sibling.join("secret.txt"), "top-secret").unwrap();

        let cfg = LinuxSandboxConfig {
            writable_workspace_dir: Some(workspace_dir.clone()),
            ..Default::default()
        };
        let sandbox = LinuxSandbox::with_config(None, cfg);

        if let Err(reason) = sandbox.check_bwrap_available() {
            eprintln!(
                "INFO: skipping live bwrap execution assertions on host without bwrap: {}",
                reason
            );
            let _ = std::fs::remove_dir_all(&base_dir);
            return;
        }

        // 1. Allowed write inside explicit workspace succeeds; /etc is read-only; hidden sibling is invisible.
        let cmd = format!(
            "echo sandbox_ok > {ws}/ok.txt && cat {ws}/ok.txt && (! touch /etc/ohc_ro_probe 2>/dev/null) && (! cat {sib}/secret.txt 2>/dev/null)",
            ws = shell_single_quote(&workspace_dir.to_string_lossy()),
            sib = shell_single_quote(&hidden_sibling.to_string_lossy()),
        );
        let out = sandbox
            .execute_command(&cmd, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0, "stderr: {}", out.stderr);
        assert_eq!(out.stdout.trim(), "sandbox_ok");
        assert_eq!(
            std::fs::read_to_string(workspace_dir.join("ok.txt"))
                .unwrap()
                .trim(),
            "sandbox_ok"
        );

        // 2. Network namespace isolation (--unshare-net): host loopback listener MUST NOT be reachable.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let net_probe = format!(
            "python3 -c 'import socket; s=socket.create_connection((\"127.0.0.1\", {}), timeout=1); s.close()'",
            port
        );
        let net_out = sandbox
            .execute_command(&net_probe, Duration::from_secs(10))
            .await
            .unwrap();
        assert_ne!(
            net_out.exit_code, 0,
            "bwrap --unshare-net must prevent connecting to host 127.0.0.1:{}",
            port
        );

        let _ = std::fs::remove_dir_all(&base_dir);
    }
}
