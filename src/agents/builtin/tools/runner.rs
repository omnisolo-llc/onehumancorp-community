#![allow(clippy::unnecessary_unwrap)]
use async_trait::async_trait;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Output;
use tokio::process::Command;

const SAFE_SANDBOX_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

const BLOCKED_ENV_KEYS: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "BASH_ENV",
    "ENV",
    "IFS",
    "PROMPT_COMMAND",
    "PYTHONPATH",
    "NODE_OPTIONS",
    "RUBYOPT",
    "PERL5OPT",
];

const SAFE_CONTAINER_CLI_ENVS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "TMPDIR",
    "DOCKER_HOST",
    "CONTAINER_HOST",
    "XDG_RUNTIME_DIR",
];

#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(
        &self,
        program: &str,
        args: &[&str],
        current_dir: Option<&Path>,
        envs: Vec<(String, String)>,
    ) -> io::Result<Output>;
}

pub struct SandboxedCommandRunner {
    pub sandbox_dir: Option<PathBuf>,
}

fn validate_env_pair(key: &str, value: &str) -> io::Result<()> {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "environment variable name must not be empty",
        ));
    };
    if !(first.is_ascii_alphabetic() || first == '_')
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid environment variable name: {key}"),
        ));
    }
    if BLOCKED_ENV_KEYS
        .iter()
        .any(|blocked| blocked.eq_ignore_ascii_case(key))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("blocked dangerous environment variable: {key}"),
        ));
    }
    if value.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("environment variable '{key}' contains NUL byte"),
        ));
    }
    Ok(())
}

fn validate_remote_identifier(label: &str, value: &str) -> io::Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('-')
        || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "invalid {label}: must be non-empty, not start with '-', and contain no whitespace/control chars"
            ),
        ));
    }
    Ok(trimmed.to_string())
}

fn sandbox_network_allowed() -> bool {
    std::env::var("OMNISOLO_AGENT_SANDBOX_ALLOW_NET").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

impl SandboxedCommandRunner {
    pub fn new(sandbox_dir: Option<PathBuf>) -> Self {
        Self { sandbox_dir }
    }

    fn execution_mode() -> String {
        std::env::var("OMNISOLO_AGENT_EXECUTION_MODE")
            .or_else(|_| std::env::var("OMNISOLO_EXECUTION_MODE"))
            .or_else(|_| std::env::var("OMNISOLO_SOURCE_MODE"))
            .unwrap_or_else(|_| "standalone".to_string())
            .trim()
            .to_ascii_lowercase()
    }

    pub(crate) fn should_use_container_backend(backend_env: &str, execution_mode: &str) -> bool {
        matches!(
            backend_env,
            "container"
                | "docker"
                | "podman"
                | "ssh"
                | "singularity"
                | "modal"
                | "daytona"
                | "vercal"
        ) || matches!(execution_mode, "cluster" | "cloud")
    }

    pub(crate) fn find_container_runtime(backend_env: &str) -> Option<String> {
        let candidates = match backend_env {
            "docker" => vec!["docker"],
            "podman" => vec!["podman"],
            "ssh" => vec!["ssh"],
            "singularity" => vec!["singularity"],
            "modal" => vec!["modal"],
            "daytona" => vec!["daytona"],
            "vercal" => vec!["vercal"],
            _ => vec!["docker", "podman"],
        };

        for candidate in candidates {
            let mut cmd = std::process::Command::new(candidate);
            let arg = if candidate == "ssh" {
                "-V"
            } else {
                "--version"
            };
            let available = cmd
                .arg(arg)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if available {
                return Some(candidate.to_string());
            }
        }
        None
    }

    fn is_bwrap_available() -> bool {
        std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn shell_command(program: &str, args: &[&str]) -> String {
        if (program == "bash" || program == "sh") && args.first() == Some(&"-c") {
            return args.get(1).copied().unwrap_or_default().to_string();
        }

        let mut parts = Vec::with_capacity(args.len() + 1);
        parts.push(shell_escape(program));
        parts.extend(args.iter().map(|arg| shell_escape(arg)));
        parts.join(" ")
    }

    pub(crate) fn build_bwrap_args(
        program: &str,
        args: &[&str],
        sandbox_dir: &Path,
        current_dir: Option<&Path>,
        allow_net: bool,
    ) -> io::Result<Vec<String>> {
        let canonical_sandbox = std::fs::canonicalize(sandbox_dir)?;
        let canonical_cwd = if let Some(dir) = current_dir {
            let resolved = std::fs::canonicalize(dir)?;
            if !resolved.starts_with(&canonical_sandbox) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "current_dir escapes sandbox_dir",
                ));
            }
            resolved
        } else {
            canonical_sandbox.clone()
        };

        let sandbox_str = canonical_sandbox.to_string_lossy().into_owned();
        let cwd_str = canonical_cwd.to_string_lossy().into_owned();

        let mut bwrap_args = vec![
            "--unshare-all".to_string(),
            "--die-with-parent".to_string(),
            "--new-session".to_string(),
            "--cap-drop".to_string(),
            "ALL".to_string(),
        ];
        if allow_net {
            bwrap_args.push("--share-net".to_string());
        } else {
            bwrap_args.push("--unshare-net".to_string());
        }

        bwrap_args.extend([
            "--proc".to_string(),
            "/proc".to_string(),
            "--dev".to_string(),
            "/dev".to_string(),
            "--tmpfs".to_string(),
            "/tmp".to_string(),
        ]);

        // Minimal read-only runtime mounts instead of exposing host `/` (no `/var/run/secrets`, `/home`, `/root`).
        for ro_mount in [
            "/usr",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/lib32",
            "/etc/alternatives",
            "/etc/ld.so.cache",
            "/etc/ld.so.conf",
            "/etc/ld.so.conf.d",
            "/etc/ssl/certs",
            "/etc/ca-certificates",
        ] {
            if Path::new(ro_mount).exists() {
                bwrap_args.push("--ro-bind".to_string());
                bwrap_args.push(ro_mount.to_string());
                bwrap_args.push(ro_mount.to_string());
            }
        }
        if allow_net && Path::new("/etc/resolv.conf").exists() {
            bwrap_args.push("--ro-bind".to_string());
            bwrap_args.push("/etc/resolv.conf".to_string());
            bwrap_args.push("/etc/resolv.conf".to_string());
        }

        bwrap_args.extend([
            "--bind".to_string(),
            sandbox_str.clone(),
            sandbox_str,
            "--chdir".to_string(),
            cwd_str,
            "--".to_string(),
            program.to_string(),
        ]);
        for arg in args {
            bwrap_args.push((*arg).to_string());
        }
        Ok(bwrap_args)
    }

    pub(crate) fn container_args(
        program: &str,
        args: &[&str],
        current_dir: Option<&Path>,
        sandbox_dir: Option<&Path>,
        envs: &[(String, String)],
        runtime: &str,
    ) -> io::Result<Vec<String>> {
        for (key, value) in envs {
            validate_env_pair(key, value)?;
        }
        let command = Self::shell_command(program, args);

        match runtime {
            "ssh" => {
                let raw_target = std::env::var("OMNISOLO_AGENT_SSH_TARGET")
                    .unwrap_or_else(|_| "localhost".to_string());
                let target = validate_remote_identifier("OMNISOLO_AGENT_SSH_TARGET", &raw_target)?;
                let mut ssh_args = vec!["--".to_string(), target];
                let mut env_prefix = String::new();
                for (key, value) in envs {
                    env_prefix.push_str(&format!("{}={} ", key, shell_escape(value)));
                }
                ssh_args.push(format!("{env_prefix}{command}"));
                Ok(ssh_args)
            }
            "singularity" => {
                let image = std::env::var("OMNISOLO_AGENT_SINGULARITY_IMAGE")
                    .unwrap_or_else(|_| "ubuntu.sif".to_string());
                let mut exec_args = vec!["exec".to_string(), "--containall".to_string()];
                for (key, value) in envs {
                    exec_args.push("--env".to_string());
                    exec_args.push(format!("{key}={value}"));
                }
                exec_args.push(image);
                exec_args.push("/bin/sh".to_string());
                exec_args.push("-c".to_string());
                exec_args.push(command);
                Ok(exec_args)
            }
            "modal" => {
                let mut exec_args = vec![
                    "run".to_string(),
                    "omnisolo_modal_worker".to_string(),
                    "--".to_string(),
                ];
                for (key, value) in envs {
                    exec_args.push("--env".to_string());
                    exec_args.push(format!("{key}={value}"));
                }
                exec_args.push(command);
                Ok(exec_args)
            }
            "daytona" => {
                let raw_workspace = std::env::var("OMNISOLO_AGENT_DAYTONA_WORKSPACE")
                    .unwrap_or_else(|_| "default".to_string());
                let workspace =
                    validate_remote_identifier("OMNISOLO_AGENT_DAYTONA_WORKSPACE", &raw_workspace)?;
                let mut exec_args = vec!["execute".to_string(), "--".to_string(), workspace];
                let mut env_prefix = String::new();
                for (key, value) in envs {
                    env_prefix.push_str(&format!("{}={} ", key, shell_escape(value)));
                }
                exec_args.push(format!("{env_prefix}{command}"));
                Ok(exec_args)
            }
            "vercal" => {
                let mut exec_args = vec!["sandbox".to_string(), "exec".to_string()];
                for (key, value) in envs {
                    exec_args.push("-e".to_string());
                    exec_args.push(format!("{key}={value}"));
                }
                exec_args.push("--".to_string());
                exec_args.push(command);
                Ok(exec_args)
            }
            _ => {
                let image = std::env::var("OMNISOLO_AGENT_CONTAINER_IMAGE")
                    .unwrap_or_else(|_| "alpine:3.20".to_string());
                let raw_sandbox = sandbox_dir.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "sandbox_dir is required for container execution",
                    )
                })?;
                let canonical_sandbox = std::fs::canonicalize(raw_sandbox).map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("failed to canonicalize container sandbox_dir: {e}"),
                    )
                })?;
                let container_workdir = if let Some(dir) = current_dir {
                    let canonical_dir = std::fs::canonicalize(dir).map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!("failed to canonicalize container current_dir: {e}"),
                        )
                    })?;
                    let rel = canonical_dir
                        .strip_prefix(&canonical_sandbox)
                        .map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "container current_dir escapes sandbox_dir",
                            )
                        })?;
                    if rel.as_os_str().is_empty() {
                        "/workspace".to_string()
                    } else {
                        format!("/workspace/{}", rel.to_string_lossy())
                    }
                } else {
                    "/workspace".to_string()
                };
                let container_user = std::env::var("OMNISOLO_AGENT_CONTAINER_USER")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
                    .unwrap_or_else(|| "65534:65534".to_string());
                let pids_limit = std::env::var("OMNISOLO_AGENT_CONTAINER_PIDS_LIMIT")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| v.chars().all(|c| c.is_ascii_digit()) && !v.is_empty())
                    .unwrap_or_else(|| "256".to_string());
                let memory_limit = std::env::var("OMNISOLO_AGENT_CONTAINER_MEMORY_LIMIT")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty() && !v.starts_with('-'))
                    .unwrap_or_else(|| "512m".to_string());

                let mut docker_args = vec![
                    "run".to_string(),
                    "--rm".to_string(),
                    "--cap-drop=ALL".to_string(),
                    "--security-opt=no-new-privileges".to_string(),
                    "--read-only".to_string(),
                    "--tmpfs".to_string(),
                    "/tmp:rw,noexec,nosuid,size=256m".to_string(),
                    "--pids-limit".to_string(),
                    pids_limit,
                    "--memory".to_string(),
                    memory_limit,
                    "--user".to_string(),
                    container_user,
                    "--network".to_string(),
                    std::env::var("OMNISOLO_AGENT_CONTAINER_NETWORK")
                        .unwrap_or_else(|_| "none".to_string()),
                    "-v".to_string(),
                    format!("{}:/workspace", canonical_sandbox.display()),
                    "-w".to_string(),
                    container_workdir,
                ];

                for (key, value) in envs {
                    docker_args.push("-e".to_string());
                    docker_args.push(format!("{key}={value}"));
                }

                docker_args.push(image);
                docker_args.push("/bin/sh".to_string());
                docker_args.push("-lc".to_string());
                docker_args.push(command);
                Ok(docker_args)
            }
        }
    }
}

fn shell_escape(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[async_trait]
impl CommandRunner for SandboxedCommandRunner {
    async fn run(
        &self,
        program: &str,
        args: &[&str],
        current_dir: Option<&Path>,
        envs: Vec<(String, String)>,
    ) -> io::Result<Output> {
        for (key, value) in &envs {
            validate_env_pair(key, value)?;
        }

        let backend_env = std::env::var("OMNISOLO_AGENT_COMMAND_BACKEND")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let execution_mode = Self::execution_mode();

        if Self::should_use_container_backend(&backend_env, &execution_mode) {
            let runtime = Self::find_container_runtime(&backend_env).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "required sandbox runtime for backend '{backend_env}' (mode '{execution_mode}') is unavailable; refusing unsandboxed host execution"
                    ),
                )
            })?;

            let container_args = Self::container_args(
                program,
                args,
                current_dir,
                self.sandbox_dir.as_deref(),
                &envs,
                &runtime,
            )?;
            let mut cmd = Command::new(&runtime);
            cmd.kill_on_drop(true);
            cmd.env_clear();
            cmd.env("PATH", SAFE_SANDBOX_PATH);
            for allowed_key in SAFE_CONTAINER_CLI_ENVS {
                if *allowed_key != "PATH"
                    && let Ok(value) = std::env::var(allowed_key)
                {
                    cmd.env(allowed_key, value);
                }
            }
            if runtime == "ssh"
                && let Ok(sock) = std::env::var("SSH_AUTH_SOCK")
            {
                cmd.env("SSH_AUTH_SOCK", sock);
            }
            cmd.args(&container_args);
            return cmd.output().await;
        }

        let Some(sandbox_dir) = self.sandbox_dir.as_deref() else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "sandbox_dir is required; refusing unsandboxed host execution",
            ));
        };

        if !Self::is_bwrap_available() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "bwrap sandbox backend is unavailable; refusing unsandboxed host execution",
            ));
        }

        let allow_net = sandbox_network_allowed();
        let bwrap_args =
            Self::build_bwrap_args(program, args, sandbox_dir, current_dir, allow_net)?;

        let mut bwrap_cmd = Command::new("bwrap");
        bwrap_cmd.kill_on_drop(true);
        bwrap_cmd.args(&bwrap_args);
        bwrap_cmd.env_clear();
        bwrap_cmd.env("PATH", SAFE_SANDBOX_PATH);
        bwrap_cmd.env("HOME", "/tmp");
        bwrap_cmd.env("TMPDIR", "/tmp");
        bwrap_cmd.env("LANG", "C.UTF-8");
        for (k, v) in envs {
            bwrap_cmd.env(k, v);
        }
        bwrap_cmd.output().await
    }
}

#[cfg(test)]
pub mod mock {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    pub type RecordedCommand = (String, Vec<String>);

    #[derive(Clone)]
    pub struct MockCommandRunner {
        pub last_command: Arc<Mutex<Option<RecordedCommand>>>,
        pub next_responses: Arc<Mutex<VecDeque<io::Result<Output>>>>,
    }

    impl Default for MockCommandRunner {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockCommandRunner {
        pub fn new() -> Self {
            Self {
                last_command: Arc::new(Mutex::new(None)),
                next_responses: Arc::new(Mutex::new(VecDeque::new())),
            }
        }

        pub fn push_response(&self, response: io::Result<Output>) {
            self.next_responses.lock().unwrap().push_back(response);
        }
    }

    #[async_trait]
    impl CommandRunner for MockCommandRunner {
        async fn run(
            &self,
            program: &str,
            args: &[&str],
            _current_dir: Option<&Path>,
            _envs: Vec<(String, String)>,
        ) -> io::Result<Output> {
            *self.last_command.lock().unwrap() = Some((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            self.next_responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Ok(Output {
                        status: mock_exit_status(0),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                    })
                })
        }
    }

    #[cfg(unix)]
    fn mock_exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn mock_exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }

    pub fn mock_output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: mock_exit_status(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_container_args_ssh_escapes_env_and_rejects_bad_keys() {
        let args = SandboxedCommandRunner::container_args(
            "echo",
            &["hello"],
            None,
            None,
            &[("FOO".to_string(), "bar; id".to_string())],
            "ssh",
        )
        .unwrap();
        assert_eq!(args[0], "--");
        assert_eq!(args[1], "localhost");
        assert!(args[2].contains("FOO='bar; id'"));
        assert!(args[2].contains("echo hello"));

        // Shell metacharacters in env keys must fail closed
        assert!(
            SandboxedCommandRunner::container_args(
                "echo",
                &["hello"],
                None,
                None,
                &[("BAD;id".to_string(), "x".to_string())],
                "ssh",
            )
            .is_err()
        );
        assert!(
            SandboxedCommandRunner::container_args(
                "echo",
                &["hello"],
                None,
                None,
                &[("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string())],
                "ssh",
            )
            .is_err()
        );
    }

    #[test]
    fn test_container_args_docker_includes_hardening_flags() {
        let dir = tempfile::tempdir().unwrap();
        let args = SandboxedCommandRunner::container_args(
            "echo",
            &["hello"],
            None,
            Some(dir.path()),
            &[("FOO".to_string(), "bar".to_string())],
            "docker",
        )
        .unwrap();
        assert!(args.contains(&"--cap-drop=ALL".to_string()));
        assert!(args.contains(&"--security-opt=no-new-privileges".to_string()));
        assert!(args.contains(&"--read-only".to_string()));
        assert!(args.windows(2).any(|w| w == ["--pids-limit", "256"]));
        assert!(args.windows(2).any(|w| w == ["--network", "none"]));
        assert!(args.windows(2).any(|w| w == ["--user", "65534:65534"]));
    }

    #[test]
    fn test_container_args_singularity() {
        let args = SandboxedCommandRunner::container_args(
            "echo",
            &["hello"],
            None,
            None,
            &[("FOO".to_string(), "bar".to_string())],
            "singularity",
        )
        .unwrap();
        assert_eq!(args[0], "exec");
        assert_eq!(args[1], "--containall");
        assert!(args.windows(2).any(|w| w == ["--env", "FOO=bar"]));
    }

    #[test]
    fn test_container_args_modal() {
        let args = SandboxedCommandRunner::container_args(
            "echo",
            &["hello"],
            None,
            None,
            &[("FOO".to_string(), "bar".to_string())],
            "modal",
        )
        .unwrap();
        assert_eq!(args[0], "run");
        assert!(args.windows(2).any(|w| w == ["--env", "FOO=bar"]));
    }

    #[test]
    fn test_container_args_daytona_escapes_env_values() {
        let args = SandboxedCommandRunner::container_args(
            "echo",
            &["hello"],
            None,
            None,
            &[("FOO".to_string(), "a'b;id".to_string())],
            "daytona",
        )
        .unwrap();
        assert_eq!(args[0], "execute");
        assert_eq!(args[1], "--");
        assert_eq!(args[2], "default");
        assert_eq!(args[3], "FOO='a'\\''b;id' echo hello");
    }

    #[test]
    fn test_container_args_vercal() {
        let args = SandboxedCommandRunner::container_args(
            "echo",
            &["hello"],
            None,
            None,
            &[("FOO".to_string(), "bar".to_string())],
            "vercal",
        )
        .unwrap();
        assert_eq!(args[0], "sandbox");
        assert_eq!(args[1], "exec");
        assert_eq!(args[2], "-e");
        assert_eq!(args[3], "FOO=bar");
        assert_eq!(args[4], "--");
        assert_eq!(args[5], "echo hello");
    }

    #[test]
    fn test_bwrap_args_use_minimal_mounts_and_full_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let args =
            SandboxedCommandRunner::build_bwrap_args("/bin/echo", &["hi"], dir.path(), None, false)
                .unwrap();
        assert!(args.contains(&"--unshare-all".to_string()));
        assert!(args.contains(&"--unshare-net".to_string()));
        assert!(args.contains(&"--die-with-parent".to_string()));
        assert!(args.contains(&"--new-session".to_string()));
        assert!(args.windows(2).any(|w| w == ["--cap-drop", "ALL"]));
        // Must NEVER mount host root `/`
        assert!(
            !args.windows(3).any(|w| w == ["--ro-bind", "/", "/"]),
            "bwrap must not bind host root /"
        );
        // Must NOT bind /etc/resolv.conf when network is disallowed
        assert!(!args.iter().any(|a| a == "/etc/resolv.conf"));
    }

    #[tokio::test]
    async fn runner_fails_closed_without_sandbox_dir_or_when_container_backend_missing() {
        let runner = SandboxedCommandRunner::new(None);
        let err = runner
            .run("/bin/echo", &["should-not-run"], None, vec![])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);

        let dir = tempfile::tempdir().unwrap();
        let container_runner = SandboxedCommandRunner::new(Some(dir.path().to_path_buf()));
        temp_env::async_with_vars(
            [
                ("OMNISOLO_AGENT_COMMAND_BACKEND", Some("daytona")),
                ("OMNISOLO_AGENT_EXECUTION_MODE", Some("standalone")),
            ],
            async {
                if SandboxedCommandRunner::find_container_runtime("daytona").is_none() {
                    let err = container_runner
                        .run("/bin/echo", &["should-not-run"], None, vec![])
                        .await
                        .unwrap_err();
                    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
                }
            },
        )
        .await;
    }

    #[tokio::test]
    async fn bwrap_execution_strips_parent_secrets_and_isolates_root() {
        if !SandboxedCommandRunner::is_bwrap_available() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let runner = SandboxedCommandRunner::new(Some(dir.path().to_path_buf()));
        temp_env::async_with_vars(
            [
                ("OPENAI_API_KEY", Some("sk-secret-host-canary")),
                ("OMNISOLO_AGENT_COMMAND_BACKEND", None::<&str>),
                ("OMNISOLO_AGENT_EXECUTION_MODE", Some("standalone")),
                ("OMNISOLO_EXECUTION_MODE", Some("standalone")),
                ("OMNISOLO_SOURCE_MODE", None::<&str>),
            ],
            async {
                let output = runner
                    .run(
                        "/bin/sh",
                        &["-c", "printf '%s' \"${OPENAI_API_KEY:-STRIPPED}\""],
                        None,
                        vec![],
                    )
                    .await
                    .unwrap();
                assert!(
                    output.status.success(),
                    "stderr={}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(String::from_utf8_lossy(&output.stdout), "STRIPPED");
            },
        )
        .await;
    }

    #[test]
    fn test_should_use_container_backend() {
        assert!(SandboxedCommandRunner::should_use_container_backend(
            "docker",
            "standalone"
        ));
        assert!(SandboxedCommandRunner::should_use_container_backend(
            "modal",
            "standalone"
        ));
        assert!(SandboxedCommandRunner::should_use_container_backend(
            "unknown", "cloud"
        ));
        assert!(SandboxedCommandRunner::should_use_container_backend(
            "unknown", "cluster"
        ));
        assert!(!SandboxedCommandRunner::should_use_container_backend(
            "unknown",
            "standalone"
        ));
        assert!(!SandboxedCommandRunner::should_use_container_backend(
            "", ""
        ));
    }

    #[test]
    fn test_find_container_runtime_candidates() {
        let _ = SandboxedCommandRunner::find_container_runtime("ssh");
        let _ = SandboxedCommandRunner::find_container_runtime("modal");
        let _ = SandboxedCommandRunner::find_container_runtime("unknown");
    }

    #[test]
    fn docker_backend_requires_sandbox_dir_and_confines_canonical_current_dir() {
        let sandbox = tempfile::tempdir().unwrap();
        let sub = sandbox.path().join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        let outside = tempfile::tempdir().unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), sandbox.path().join("escape_link")).unwrap();

        // sandbox_dir = None must fail closed with PermissionDenied even when current_dir is Some
        let no_sandbox = SandboxedCommandRunner::container_args(
            "echo",
            &["hi"],
            Some(outside.path()),
            None,
            &[],
            "docker",
        )
        .unwrap_err();
        assert_eq!(no_sandbox.kind(), io::ErrorKind::PermissionDenied);

        // current_dir outside sandbox_dir must fail closed with PermissionDenied
        let outside_err = SandboxedCommandRunner::container_args(
            "echo",
            &["hi"],
            Some(outside.path()),
            Some(sandbox.path()),
            &[],
            "docker",
        )
        .unwrap_err();
        assert_eq!(outside_err.kind(), io::ErrorKind::PermissionDenied);

        // Symlink escaping sandbox_dir must fail closed with PermissionDenied
        #[cfg(unix)]
        {
            let link_path = sandbox.path().join("escape_link");
            let sym_err = SandboxedCommandRunner::container_args(
                "echo",
                &["hi"],
                Some(link_path.as_path()),
                Some(sandbox.path()),
                &[],
                "docker",
            )
            .unwrap_err();
            assert_eq!(sym_err.kind(), io::ErrorKind::PermissionDenied);
        }

        // Valid subdirectory mounts canonical sandbox_dir and sets confined -w /workspace/subdir
        let args = SandboxedCommandRunner::container_args(
            "echo",
            &["hi"],
            Some(sub.as_path()),
            Some(sandbox.path()),
            &[],
            "docker",
        )
        .unwrap();
        let canonical_sandbox = std::fs::canonicalize(sandbox.path()).unwrap();
        assert!(
            args.windows(2)
                .any(|w| w == ["-v", &format!("{}:/workspace", canonical_sandbox.display())]),
            "expected canonical sandbox mount in {args:?}"
        );
        assert!(
            args.windows(2).any(|w| w == ["-w", "/workspace/subdir"]),
            "expected confined workdir in {args:?}"
        );
    }
}
