// SPDX-License-Identifier: Apache-2.0
//! Own the packaged Node server for the actual Next application. No command or
//! path comes from a web page, and remote pages do not receive shell permissions.
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;
use tauri::Manager;

const DEFAULT_BACKEND_ORIGIN: &str = "http://127.0.0.1:18789";
const DEFAULT_NATIVE_WEB_PORT: u16 = 18790;
const FALLBACK_PORT_SPAN: u16 = 100;
const NATIVE_WEB_SESSION_KEY_ID: &str = "native-session-v1";
const STATE_DIR_NAME: &str = ".omnisolo";
const PORT_FILE_NAME: &str = "desktop-web-port";
const SESSION_SECRET_FILE_NAME: &str = "web-session.secret";

const BASE64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

#[derive(Default)]
pub struct NativeRuntime {
    child: Mutex<Option<Child>>,
    stopping: AtomicBool,
}

impl NativeRuntime {
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        if let Ok(mut state) = self.child.lock()
            && let Some(mut child) = state.take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn backend_origin() -> Result<String, String> {
    let value = std::env::var("BACKEND_URL").unwrap_or_else(|_| DEFAULT_BACKEND_ORIGIN.to_string());
    parse_backend_origin(&value)
}

fn parse_backend_origin(value: &str) -> Result<String, String> {
    let url = tauri::Url::parse(value).map_err(|_| "BACKEND_URL must be an absolute origin")?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
    {
        return Err("BACKEND_URL must be an HTTPS origin or local loopback HTTP origin".into());
    }
    Ok(url.origin().ascii_serialization())
}

fn resolve_state_dir() -> Result<PathBuf, String> {
    resolve_state_dir_from(
        std::env::var_os("OMNISOLO_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
    )
}

fn resolve_state_dir_from(
    omnisolo_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
    userprofile: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, String> {
    if let Some(dir) = omnisolo_home
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = home
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir).join(STATE_DIR_NAME));
    }
    if let Some(dir) = userprofile
        && !dir.is_empty()
    {
        return Ok(PathBuf::from(dir).join(STATE_DIR_NAME));
    }
    Err("Cannot determine local state directory: set OMNISOLO_HOME, HOME, or USERPROFILE".into())
}

fn ensure_state_dir(dir: &Path) -> Result<(), String> {
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(format!(
                    "Local state path {} must be a regular directory",
                    dir.display()
                ));
            }
            return Ok(());
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "Cannot inspect local state directory {}: {err}",
                dir.display()
            ));
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|err| {
                format!(
                    "Cannot create local state directory {}: {err}",
                    dir.display()
                )
            })?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir).map_err(|err| {
            format!(
                "Cannot create local state directory {}: {err}",
                dir.display()
            )
        })?;
    }
    Ok(())
}

fn create_private_file(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

fn write_private_file_atomic(dir: &Path, filename: &str, contents: &[u8]) -> Result<(), String> {
    ensure_state_dir(dir)?;
    let target = dir.join(filename);
    if let Ok(meta) = fs::symlink_metadata(&target)
        && meta.file_type().is_symlink()
    {
        return Err(format!(
            "Refusing to overwrite symlink {}",
            target.display()
        ));
    }
    let tmp = dir.join(format!(".{filename}.tmp.{}", uuid::Uuid::new_v4()));
    let write_result = (|| -> std::io::Result<()> {
        let mut file = create_private_file(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(format!("Cannot write {}: {err}", target.display()));
    }
    if let Err(err) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("Cannot persist {}: {err}", target.display()));
    }
    Ok(())
}

fn parse_port(raw: &str) -> Result<u16, String> {
    let port: u16 = raw
        .trim()
        .parse()
        .map_err(|_| format!("Invalid native web port '{raw}'"))?;
    if port < 1024 {
        return Err(format!(
            "Native web port must be between 1024 and 65535 (got {port})"
        ));
    }
    Ok(port)
}

fn read_persisted_port(state_dir: &Path) -> Result<Option<u16>, String> {
    let path = state_dir.join(PORT_FILE_NAME);
    match fs::symlink_metadata(&path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(format!(
                    "Persisted port file {} must be a regular file",
                    path.display()
                ));
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "Cannot inspect persisted port file {}: {err}",
                path.display()
            ));
        }
    }
    let raw = fs::read_to_string(&path)
        .map_err(|err| format!("Cannot read persisted port file {}: {err}", path.display()))?;
    Ok(Some(parse_port(&raw)?))
}

fn persist_port(state_dir: &Path, port: u16) -> Result<(), String> {
    write_private_file_atomic(state_dir, PORT_FILE_NAME, format!("{port}\n").as_bytes())
}

fn bind_native_web_listener(
    state_dir: &Path,
    configured_port: Option<&str>,
    default_port: u16,
) -> Result<(TcpListener, u16), String> {
    if let Some(raw) = configured_port {
        let port = parse_port(raw)?;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .map_err(|err| format!("Cannot bind configured native web port {port}: {err}"))?;
        return Ok((listener, port));
    }

    ensure_state_dir(state_dir)?;
    let persisted = read_persisted_port(state_dir)?;
    let mut candidates = Vec::with_capacity(usize::from(FALLBACK_PORT_SPAN) + 2);
    if let Some(port) = persisted {
        candidates.push(port);
    }
    if default_port >= 1024 && !candidates.contains(&default_port) {
        candidates.push(default_port);
    }
    for offset in 1..=FALLBACK_PORT_SPAN {
        if let Some(port) = default_port.checked_add(offset)
            && !candidates.contains(&port)
        {
            candidates.push(port);
        }
    }

    for candidate in candidates {
        match TcpListener::bind(("127.0.0.1", candidate)) {
            Ok(listener) => {
                if persisted != Some(candidate) {
                    persist_port(state_dir, candidate)?;
                }
                return Ok((listener, candidate));
            }
            Err(err) if err.kind() == ErrorKind::AddrInUse => continue,
            Err(err) => {
                return Err(format!("Cannot bind local web port {candidate}: {err}"));
            }
        }
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|err| format!("Cannot bind fallback local web port: {err}"))?;
    let port = listener.local_addr().map_err(|err| err.to_string())?.port();
    persist_port(state_dir, port)?;
    Ok((listener, port))
}

fn is_structurally_weak_key(bytes: &[u8; 32]) -> bool {
    let mut seen = [false; 256];
    let mut distinct = 0usize;
    for &b in bytes {
        let idx = usize::from(b);
        if !seen[idx] {
            seen[idx] = true;
            distinct += 1;
        }
    }
    if distinct < 16 {
        return true;
    }
    for period in 1..=16 {
        if bytes
            .iter()
            .enumerate()
            .all(|(idx, &b)| b == bytes[idx % period])
        {
            return true;
        }
    }
    let ascending = bytes
        .windows(2)
        .all(|pair| pair[1] == pair[0].wrapping_add(1));
    let descending = bytes
        .windows(2)
        .all(|pair| pair[1] == pair[0].wrapping_sub(1));
    ascending || descending
}

fn encode_base64url_32(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(43);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = if chunk.len() > 1 {
            u32::from(chunk[1])
        } else {
            0
        };
        let b2 = if chunk.len() > 2 {
            u32::from(chunk[2])
        } else {
            0
        };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(BASE64URL_ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(BASE64URL_ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(BASE64URL_ALPHABET[((triple >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(BASE64URL_ALPHABET[(triple & 0x3f) as usize] as char);
        }
    }
    out
}

fn decode_base64url_sextet(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

fn decode_validate_session_secret(encoded: &str) -> Result<[u8; 32], String> {
    let raw = encoded.as_bytes();
    if raw.len() != 43 {
        return Err("OMNISOLO_WEB_SESSION_SECRET must be 43 base64url characters".into());
    }
    let mut bytes = [0u8; 32];
    let mut out_idx = 0usize;
    let mut in_idx = 0usize;
    while in_idx < 40 {
        let s0 = decode_base64url_sextet(raw[in_idx]).ok_or("Invalid base64url character")?;
        let s1 = decode_base64url_sextet(raw[in_idx + 1]).ok_or("Invalid base64url character")?;
        let s2 = decode_base64url_sextet(raw[in_idx + 2]).ok_or("Invalid base64url character")?;
        let s3 = decode_base64url_sextet(raw[in_idx + 3]).ok_or("Invalid base64url character")?;
        bytes[out_idx] = (s0 << 2) | (s1 >> 4);
        bytes[out_idx + 1] = (s1 << 4) | (s2 >> 2);
        bytes[out_idx + 2] = (s2 << 6) | s3;
        out_idx += 3;
        in_idx += 4;
    }
    let s0 = decode_base64url_sextet(raw[40]).ok_or("Invalid base64url character")?;
    let s1 = decode_base64url_sextet(raw[41]).ok_or("Invalid base64url character")?;
    let s2 = decode_base64url_sextet(raw[42]).ok_or("Invalid base64url character")?;
    if (s2 & 0x03) != 0 {
        return Err("OMNISOLO_WEB_SESSION_SECRET has non-canonical trailing bits".into());
    }
    bytes[30] = (s0 << 2) | (s1 >> 4);
    bytes[31] = (s1 << 4) | (s2 >> 2);

    if encode_base64url_32(&bytes) != encoded || is_structurally_weak_key(&bytes) {
        return Err(
            "OMNISOLO_WEB_SESSION_SECRET must be canonical base64url for acceptable 32-byte key material"
                .into(),
        );
    }
    Ok(bytes)
}

fn fill_os_random_32() -> Result<[u8; 32], String> {
    #[cfg(unix)]
    {
        let mut bytes = [0u8; 32];
        let mut urandom = File::open("/dev/urandom")
            .map_err(|err| format!("Cannot open /dev/urandom for session key entropy: {err}"))?;
        urandom
            .read_exact(&mut bytes)
            .map_err(|err| format!("Cannot read session key entropy: {err}"))?;
        Ok(bytes)
    }
    #[cfg(not(unix))]
    {
        // Extract only the 14 completely unbiased CSPRNG bytes from each UUIDv4
        // (skipping RFC 4122 version byte 6 and variant byte 8). Three UUIDv4s
        // provide 42 unbiased OS-CSPRNG bytes; take the first 32.
        const UNBIASED_INDICES: [usize; 14] = [0, 1, 2, 3, 4, 5, 7, 9, 10, 11, 12, 13, 14, 15];
        let mut bytes = [0u8; 32];
        let mut written = 0usize;
        while written < bytes.len() {
            let uuid_bytes = uuid::Uuid::new_v4().into_bytes();
            for idx in UNBIASED_INDICES {
                if written == bytes.len() {
                    break;
                }
                bytes[written] = uuid_bytes[idx];
                written += 1;
            }
        }
        Ok(bytes)
    }
}

fn generate_session_secret() -> Result<String, String> {
    for _ in 0..16 {
        let bytes = fill_os_random_32()?;
        if !is_structurally_weak_key(&bytes) {
            return Ok(encode_base64url_32(&bytes));
        }
    }
    Err("CSPRNG failed to generate acceptable session key material".into())
}

fn read_verified_session_secret(path: &Path) -> Result<String, String> {
    let meta = fs::symlink_metadata(path)
        .map_err(|err| format!("Cannot inspect session secret {}: {err}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(format!(
            "Session secret path {} must be a regular file",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(format!(
                "Session secret {} has insecure permissions {:04o}; expected 0600",
                path.display(),
                mode
            ));
        }
    }
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("Cannot read session secret {}: {err}", path.display()))?;
    let secret = raw.trim();
    let _ = decode_validate_session_secret(secret)?;
    Ok(secret.to_string())
}

fn load_or_create_session_secret(state_dir: &Path) -> Result<String, String> {
    ensure_state_dir(state_dir)?;
    let path = state_dir.join(SESSION_SECRET_FILE_NAME);
    match fs::symlink_metadata(&path) {
        Ok(_) => return read_verified_session_secret(&path),
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            return Err(format!(
                "Cannot inspect session secret {}: {err}",
                path.display()
            ));
        }
    }

    let secret = generate_session_secret()?;
    let tmp = state_dir.join(format!(
        ".{SESSION_SECRET_FILE_NAME}.tmp.{}",
        uuid::Uuid::new_v4()
    ));
    let write_result = (|| -> std::io::Result<()> {
        let mut file = create_private_file(&tmp)?;
        file.write_all(format!("{secret}\n").as_bytes())?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(format!(
            "Cannot write session secret {}: {err}",
            path.display()
        ));
    }

    match fs::hard_link(&tmp, &path) {
        Ok(()) => {
            let _ = fs::remove_file(&tmp);
            Ok(secret)
        }
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&tmp);
            read_verified_session_secret(&path)
        }
        Err(_) => {
            // Filesystems without hard-link support fall back to exclusive 0600 create.
            let _ = fs::remove_file(&tmp);
            match create_private_file(&path) {
                Ok(mut file) => {
                    if let Err(err) = file
                        .write_all(format!("{secret}\n").as_bytes())
                        .and_then(|()| file.sync_all())
                    {
                        let _ = fs::remove_file(&path);
                        return Err(format!(
                            "Cannot persist session secret {}: {err}",
                            path.display()
                        ));
                    }
                    Ok(secret)
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    read_verified_session_secret(&path)
                }
                Err(err) => Err(format!(
                    "Cannot create session secret {}: {err}",
                    path.display()
                )),
            }
        }
    }
}

/// Drain a line without allocating beyond the handshake bound. Oversized log
/// lines are discarded rather than filling the child's pipe after readiness.
fn bounded_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> std::io::Result<bool> {
    line.clear();
    let mut observed = false;
    let mut oversized = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(observed);
        }
        observed = true;
        let end = buffer
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|at| at + 1);
        let amount = end.unwrap_or(buffer.len());
        if !oversized {
            if line.len().saturating_add(amount) <= 16_384 {
                line.extend_from_slice(&buffer[..amount]);
            } else {
                line.clear();
                oversized = true;
            }
        }
        reader.consume(amount);
        if end.is_some() {
            return Ok(true);
        }
    }
}

fn resource_root(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    // Debug builds are not installed bundles. This compile-time path is created
    // by our packaging script; web content cannot override it.
    #[cfg(debug_assertions)]
    {
        let prepared = Path::new(env!("CARGO_MANIFEST_DIR")).join("native-resources");
        if prepared.join("web/build-manifest.json").is_file() {
            return Ok(prepared);
        }
    }
    Ok(app
        .path()
        .resource_dir()
        .map_err(|error| error.to_string())?
        .join("native-runtime"))
}

fn start(app: &tauri::AppHandle) -> Result<tauri::Url, String> {
    let resources = resource_root(app)?;
    let node = resources
        .join("bin")
        .join(if cfg!(windows) { "node.exe" } else { "node" });
    let entry = resources.join("web/src/ui/next/native-entry.cjs");
    if !node.is_file() || !entry.is_file() {
        return Err("Native web resources are missing. Rebuild with npm run desktop:build.".into());
    }
    let state_dir = resolve_state_dir()?;
    let configured_port = std::env::var("OMNISOLO_NATIVE_WEB_PORT")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let (listener, port) = bind_native_web_listener(
        &state_dir,
        configured_port.as_deref(),
        DEFAULT_NATIVE_WEB_PORT,
    )?;
    let session_secret = load_or_create_session_secret(&state_dir)?;
    let launch = uuid::Uuid::new_v4().to_string();
    let expected = format!("OMNISOLO_NATIVE_READY {launch} {port}");
    let mut command = Command::new(node);
    command
        .arg(entry)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for key in [
        "PATH",
        "HOME",
        "USERPROFILE",
        "SYSTEMROOT",
        "WINDIR",
        "TMP",
        "TEMP",
        "TMPDIR",
        "LANG",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
        .env("BACKEND_URL", backend_origin()?)
        .env("OMNISOLO_NATIVE_WEB_PORT", port.to_string())
        .env("OMNISOLO_NATIVE_LAUNCH_ID", &launch)
        .env("OMNISOLO_WEB_SESSION_KEY_ID", NATIVE_WEB_SESSION_KEY_ID)
        .env("OMNISOLO_WEB_SESSION_SECRET", &session_secret);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    // The child does not retry an occupied port. Its private stdout readiness
    // handshake prevents a different local process being mistaken for our app.
    drop(listener);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Cannot start packaged Node: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or("Native runtime stdout unavailable")?;
    let state = app.state::<NativeRuntime>();
    {
        let mut slot = state
            .child
            .lock()
            .map_err(|_| "Native runtime state unavailable")?;
        if state.stopping.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Application is shutting down".into());
        }
        *slot = Some(child);
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        loop {
            if !matches!(bounded_line(&mut reader, &mut line), Ok(true)) {
                break;
            }
            if String::from_utf8_lossy(&line).trim() == expected {
                let _ = sender.try_send(());
            }
        }
    });
    if receiver.recv_timeout(Duration::from_secs(45)).is_err() {
        state.stop();
        return Err("The local workspace did not start. Check the installed runtime and backend configuration.".into());
    }
    let mut slot = state
        .child
        .lock()
        .map_err(|_| "Native runtime state unavailable")?;
    if state.stopping.load(Ordering::SeqCst)
        || slot
            .as_mut()
            .ok_or("Native runtime stopped")?
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
    {
        return Err("The local workspace exited during startup".into());
    }
    tauri::Url::parse(&format!("http://127.0.0.1:{port}/")).map_err(|error| error.to_string())
}

pub fn start_window(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let result = start(&app);
        if let Some(window) = app.get_webview_window("main") {
            match result {
                Ok(url) => {
                    if window.navigate(url).is_err() {
                        app.state::<NativeRuntime>().stop();
                    }
                }
                Err(message) => {
                    let literal = serde_json::to_string(&message)
                        .unwrap_or_else(|_| "\"Workspace startup failed\"".into());
                    let _ = window.eval(format!(
                        "document.getElementById('status').textContent={literal};"
                    ));
                }
            }
        } else {
            app.state::<NativeRuntime>().stop();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempStateDir {
        path: PathBuf,
    }

    impl TempStateDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("ohc-tauri-test-{}", uuid::Uuid::new_v4()));
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempStateDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn find_two_adjacent_free_ports() -> u16 {
        use std::sync::atomic::AtomicU16;
        static NEXT_BASE: AtomicU16 = AtomicU16::new(29_100);
        loop {
            let base = NEXT_BASE.fetch_add(10, Ordering::Relaxed);
            assert!(
                base < 60_000,
                "Could not find two adjacent free loopback ports"
            );
            if let (Ok(l1), Ok(l2)) = (
                TcpListener::bind(("127.0.0.1", base)),
                TcpListener::bind(("127.0.0.1", base + 1)),
            ) {
                drop(l2);
                drop(l1);
                return base;
            }
        }
    }

    #[test]
    fn backend_origin_preserves_https_and_loopback_origins() {
        for (value, expected) in [
            ("https://api.example.com/", "https://api.example.com"),
            (
                "https://api.example.com:8443",
                "https://api.example.com:8443",
            ),
            ("http://127.0.0.1:18789", "http://127.0.0.1:18789"),
            ("http://[::1]:18789/", "http://[::1]:18789"),
            ("http://localhost:18789/", "http://localhost:18789"),
        ] {
            assert_eq!(parse_backend_origin(value).unwrap(), expected, "{value}");
        }
    }

    #[test]
    fn backend_origin_rejects_credentials_non_origins_and_non_loopback_http() {
        for value in [
            "https://user@example.com",
            "https://user:password@example.com",
            "https://:password@example.com",
            "https://example.com/path",
            "https://example.com/?query=1",
            "https://example.com/#fragment",
            "http://example.com",
            "http://localhost.example.com",
            "http://192.168.1.1",
            "http://[::2]",
            "ftp://localhost",
            "file:///tmp/app",
            "example.com",
            "https://[invalid]",
            "",
        ] {
            assert!(parse_backend_origin(value).is_err(), "{value}");
        }
    }

    #[test]
    fn state_dir_resolution_prefers_omnisolo_home_then_home_and_fails_closed() {
        use std::ffi::OsStr;
        assert_eq!(
            resolve_state_dir_from(
                Some(OsStr::new("/var/lib/ohc")),
                Some(OsStr::new("/home/owner")),
                None
            )
            .unwrap(),
            PathBuf::from("/var/lib/ohc")
        );
        assert_eq!(
            resolve_state_dir_from(None, Some(OsStr::new("/home/owner")), None).unwrap(),
            PathBuf::from("/home/owner/.omnisolo")
        );
        assert_eq!(
            resolve_state_dir_from(None, None, Some(OsStr::new("C:\\Users\\owner"))).unwrap(),
            PathBuf::from("C:\\Users\\owner").join(".omnisolo")
        );
        assert!(resolve_state_dir_from(None, None, None).is_err());
    }

    #[test]
    fn default_port_persists_and_is_reused_across_restarts() {
        let dir = TempStateDir::new();
        let base_port = find_two_adjacent_free_ports();

        let (listener1, port1) = bind_native_web_listener(dir.path(), None, base_port).unwrap();
        assert_eq!(port1, base_port);
        assert_eq!(read_persisted_port(dir.path()).unwrap(), Some(base_port));
        drop(listener1);

        let (listener2, port2) = bind_native_web_listener(dir.path(), None, base_port).unwrap();
        assert_eq!(port2, base_port);
        drop(listener2);
    }

    #[test]
    fn occupied_default_port_falls_back_and_persists_stable_origin() {
        let dir = TempStateDir::new();
        let base_port = find_two_adjacent_free_ports();

        let blocker = TcpListener::bind(("127.0.0.1", base_port)).unwrap();
        let (listener1, port1) = bind_native_web_listener(dir.path(), None, base_port).unwrap();
        assert_eq!(port1, base_port + 1);
        assert_eq!(
            read_persisted_port(dir.path()).unwrap(),
            Some(base_port + 1)
        );
        drop(listener1);
        drop(blocker);

        // Even after the default port frees up, restarts stay on the persisted
        // fallback port so localStorage/IndexedDB are never orphaned.
        let (listener2, port2) = bind_native_web_listener(dir.path(), None, base_port).unwrap();
        assert_eq!(port2, base_port + 1);
        drop(listener2);
    }

    #[test]
    fn explicit_configured_port_binds_exact_port_and_fails_closed_when_occupied_or_invalid() {
        let dir = TempStateDir::new();
        let base_port = find_two_adjacent_free_ports();

        assert!(bind_native_web_listener(dir.path(), Some("80"), base_port).is_err());
        assert!(bind_native_web_listener(dir.path(), Some("not-a-port"), base_port).is_err());

        let configured = base_port.to_string();
        let (listener, port) =
            bind_native_web_listener(dir.path(), Some(&configured), base_port + 1).unwrap();
        assert_eq!(port, base_port);
        // While still occupied, requesting the same explicit port fails closed.
        assert!(bind_native_web_listener(dir.path(), Some(&configured), base_port + 1).is_err());
        drop(listener);
    }

    #[test]
    fn session_secret_persists_across_restarts_and_validates_as_canonical_32_byte_base64url() {
        let dir = TempStateDir::new();

        let first = load_or_create_session_secret(dir.path()).unwrap();
        assert_eq!(first.len(), 43);
        let decoded = decode_validate_session_secret(&first).unwrap();
        assert_eq!(encode_base64url_32(&decoded), first);

        let second = load_or_create_session_secret(dir.path()).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn session_secret_rejects_weak_and_malformed_material() {
        assert!(decode_validate_session_secret("short").is_err());
        let all_zeros = encode_base64url_32(&[0u8; 32]);
        assert!(decode_validate_session_secret(&all_zeros).is_err());
        let ascending: [u8; 32] = std::array::from_fn(|i| i as u8);
        assert!(decode_validate_session_secret(&encode_base64url_32(&ascending)).is_err());

        // Non-zero trailing padding bits in the 43rd base64url character must be rejected.
        let valid = generate_session_secret().unwrap();
        let mut mutated = valid.into_bytes();
        mutated[42] = b'B'; // value 1 has low bit set -> non-canonical for 32 bytes
        let invalid = String::from_utf8(mutated).unwrap();
        assert!(decode_validate_session_secret(&invalid).is_err());

        let dir = TempStateDir::new();
        ensure_state_dir(dir.path()).unwrap();
        write_private_file_atomic(dir.path(), SESSION_SECRET_FILE_NAME, b"corrupt-secret\n")
            .unwrap();
        assert!(load_or_create_session_secret(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn state_dir_port_and_session_secret_enforce_unix_permissions_and_reject_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = TempStateDir::new();
        let base_port = find_two_adjacent_free_ports();

        let (listener, _) = bind_native_web_listener(dir.path(), None, base_port).unwrap();
        drop(listener);
        let _secret = load_or_create_session_secret(dir.path()).unwrap();

        let dir_mode = fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);

        let port_mode = fs::metadata(dir.path().join(PORT_FILE_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(port_mode, 0o600);

        let secret_path = dir.path().join(SESSION_SECRET_FILE_NAME);
        let secret_mode = fs::metadata(&secret_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(secret_mode, 0o600);

        // Loosening secret permissions must fail closed on reload.
        fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_or_create_session_secret(dir.path()).is_err());

        // Symlinked secret file must be rejected.
        let link_dir = TempStateDir::new();
        ensure_state_dir(link_dir.path()).unwrap();
        symlink(&secret_path, link_dir.path().join(SESSION_SECRET_FILE_NAME)).unwrap();
        assert!(load_or_create_session_secret(link_dir.path()).is_err());
    }

    #[test]
    fn oversized_lines_are_bounded_and_following_readiness_survives() {
        let mut data = vec![b'x'; 100_000];
        data.extend_from_slice(b"\nOMNISOLO_NATIVE_READY test 1234\n");
        let mut reader = BufReader::with_capacity(257, data.as_slice());
        let mut line = Vec::new();
        assert!(bounded_line(&mut reader, &mut line).unwrap());
        assert!(line.is_empty());
        assert!(line.capacity() <= 32_768);
        assert!(bounded_line(&mut reader, &mut line).unwrap());
        assert_eq!(line, b"OMNISOLO_NATIVE_READY test 1234\n");
        assert!(!bounded_line(&mut reader, &mut line).unwrap());
    }

    #[test]
    fn unterminated_final_line_and_empty_input_are_handled() {
        let mut line = vec![];
        let mut reader = std::io::Cursor::new(b"final".as_slice());
        assert!(bounded_line(&mut reader, &mut line).unwrap());
        assert_eq!(line, b"final");
        assert!(!bounded_line(&mut reader, &mut line).unwrap());
    }
}
