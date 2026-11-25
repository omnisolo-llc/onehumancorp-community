use ::server_telemetry::record_bubblewrap_violation;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, watch};
use zeroize::Zeroizing;

use crate::sentinel::{
    LeakAction, ResponseLeakScanner, Sentinel, SentinelError, SsrfGuard, SurrogateTokenBroker,
    extract_surrogate_tokens,
};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BUFFERED_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
struct ProxyRuntime {
    ssrf_guard: SsrfGuard,
    broker: Option<SurrogateTokenBroker>,
    leak_scanner: ResponseLeakScanner,
    tenant_id: Option<String>,
    session_id: Option<String>,
    agent_id: String,
    task_id: String,
}

pub struct NetworkProxy {
    ssrf_guard: SsrfGuard,
    broker: Option<SurrogateTokenBroker>,
    leak_scanner: ResponseLeakScanner,
    tenant_id: Option<String>,
    session_id: Option<String>,
    agent_id: String,
    task_id: String,
}

impl NetworkProxy {
    pub fn new(allowed_domains: Vec<String>, agent_id: String, task_id: String) -> Self {
        Self {
            ssrf_guard: SsrfGuard::new(allowed_domains),
            broker: None,
            leak_scanner: ResponseLeakScanner::new(),
            tenant_id: None,
            session_id: None,
            agent_id,
            task_id,
        }
    }

    pub fn with_sentinel(
        mut self,
        tenant_id: impl Into<String>,
        session_id: impl Into<String>,
        sentinel: Sentinel,
    ) -> Self {
        self.ssrf_guard = sentinel.ssrf_guard;
        self.broker = Some(sentinel.broker);
        self.leak_scanner = sentinel.leak_scanner;
        self.tenant_id = Some(tenant_id.into());
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_ssrf_guard(mut self, ssrf_guard: SsrfGuard) -> Self {
        self.ssrf_guard = ssrf_guard;
        self
    }

    pub fn with_surrogate_broker(
        mut self,
        broker: SurrogateTokenBroker,
        tenant_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Self {
        self.broker = Some(broker);
        self.tenant_id = Some(tenant_id.into());
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_leak_scanner(mut self, leak_scanner: ResponseLeakScanner) -> Self {
        self.leak_scanner = leak_scanner;
        self
    }

    pub async fn start(&self, port: u16) -> Result<(u16, oneshot::Sender<()>), std::io::Error> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let actual_port = listener.local_addr()?.port();
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        let runtime = Arc::new(ProxyRuntime {
            ssrf_guard: self.ssrf_guard.clone(),
            broker: self.broker.clone(),
            leak_scanner: self.leak_scanner.clone(),
            tenant_id: self.tenant_id.clone(),
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            task_id: self.task_id.clone(),
        });

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    accept_res = listener.accept() => {
                        match accept_res {
                            Ok((stream, _)) => {
                                let rt = Arc::clone(&runtime);
                                tokio::spawn(async move {
                                    if let Err(e) = handle_connection(stream, rt).await {
                                        tracing::debug!("Proxy connection error: {}", e);
                                    }
                                });
                            }
                            Err(e) => tracing::error!("Failed to accept connection: {}", e),
                        }
                    }
                    _ = &mut shutdown_rx => {
                        tracing::debug!("Shutting down network proxy on port {}", actual_port);
                        break;
                    }
                }
            }
        });

        Ok((actual_port, shutdown_tx))
    }
}

struct ParsedRequestHead {
    method: String,
    request_target: String,
    version: String,
    headers: Vec<(String, String)>,
    target_host: String,
    target_port: u16,
    is_connect: bool,
    initial_body: Vec<u8>,
}

async fn handle_connection(mut stream: TcpStream, rt: Arc<ProxyRuntime>) -> Result<(), String> {
    let raw_request = read_http_head(&mut stream).await?;
    if raw_request.is_empty() {
        return Ok(());
    }

    let parsed = match parse_request_head(&raw_request) {
        Ok(parsed) => parsed,
        Err(reason) => {
            write_simple_response(&mut stream, "400 Bad Request", &reason).await;
            return Err(reason);
        }
    };

    // 1. Validate allowlist & resolve SSRF-checked destination BEFORE injecting any secret or dialing.
    let validated_addrs = match rt
        .ssrf_guard
        .resolve_and_validate(&parsed.target_host, parsed.target_port)
        .await
    {
        Ok(addrs) => addrs,
        Err(SentinelError::HostNotAllowed { host }) => {
            record_bubblewrap_violation(&rt.agent_id, &rt.task_id, "network_violation_denied");
            write_simple_response(
                &mut stream,
                "403 Forbidden",
                "Denied by sandbox proxy policy",
            )
            .await;
            return Err(format!("Domain {host} is blocked"));
        }
        Err(SentinelError::SsrfBlocked { target, reason }) => {
            record_bubblewrap_violation(&rt.agent_id, &rt.task_id, "network_ssrf_denied");
            write_simple_response(
                &mut stream,
                "403 Forbidden",
                "Denied by sentinel SSRF egress policy",
            )
            .await;
            return Err(format!("SSRF blocked for {target}: {reason}"));
        }
        Err(SentinelError::DnsResolutionFailed { host, reason }) => {
            write_simple_response(&mut stream, "502 Bad Gateway", "Upstream DNS lookup failed")
                .await;
            return Err(format!("DNS failed for {host}: {reason}"));
        }
        Err(err) => {
            write_simple_response(&mut stream, "403 Forbidden", "Denied by sentinel policy").await;
            return Err(err.to_string());
        }
    };

    if parsed.is_connect {
        return handle_connect_tunnel(stream, &validated_addrs).await;
    }

    // 2. JIT surrogate token injection (only for verified tenant/session + allowlisted host + active revocation).
    let injected = match inject_surrogate_headers(&parsed.headers, &parsed.target_host, &rt) {
        Ok(res) => res,
        Err((status, msg)) => {
            record_bubblewrap_violation(&rt.agent_id, &rt.task_id, "surrogate_egress_denied");
            write_simple_response(&mut stream, status, &msg).await;
            return Err(msg);
        }
    };

    // 3. Connect directly to SSRF-validated SocketAddrs (prevents DNS rebinding TOCTOU).
    let mut target_stream = match TcpStream::connect(validated_addrs.as_slice()).await {
        Ok(s) => s,
        Err(err) => {
            write_simple_response(&mut stream, "502 Bad Gateway", "Failed to reach upstream").await;
            return Err(format!("Upstream connect failed: {err}"));
        }
    };

    let outbound_head = serialize_upstream_request(&parsed, &injected.headers);
    if target_stream.write_all(&outbound_head).await.is_err() {
        write_simple_response(
            &mut stream,
            "502 Bad Gateway",
            "Failed writing upstream head",
        )
        .await;
        return Err("Failed writing upstream request head".to_owned());
    }
    if !parsed.initial_body.is_empty()
        && target_stream.write_all(&parsed.initial_body).await.is_err()
    {
        write_simple_response(
            &mut stream,
            "502 Bad Gateway",
            "Failed writing request body",
        )
        .await;
        return Err("Failed writing initial request body".to_owned());
    }

    let content_length = header_content_length(&parsed.headers);
    if let Some(total_len) = content_length
        && total_len > parsed.initial_body.len()
    {
        let remaining = total_len - parsed.initial_body.len();
        let mut limited = (&mut stream).take(remaining as u64);
        if tokio::io::copy(&mut limited, &mut target_stream)
            .await
            .is_err()
        {
            write_simple_response(&mut stream, "502 Bad Gateway", "Failed forwarding body").await;
            return Err("Failed forwarding request body".to_owned());
        }
    }

    // 4. Scan upstream response headers & body with ResponseLeakScanner before bytes return to the agent.
    let scanner = rt.leak_scanner.with_additional_secrets(&injected.secrets);
    forward_scanned_response(
        &mut target_stream,
        &mut stream,
        &scanner,
        &injected.revocations,
    )
    .await
}

struct InjectedEgress {
    headers: Vec<(String, String)>,
    secrets: Vec<Zeroizing<String>>,
    revocations: Vec<watch::Receiver<bool>>,
}

async fn handle_connect_tunnel(
    mut stream: TcpStream,
    validated_addrs: &[SocketAddr],
) -> Result<(), String> {
    if let Ok(mut target_stream) = TcpStream::connect(validated_addrs).await {
        let _ = stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await;

        let (mut ri, mut wi) = stream.split();
        let (mut ro, mut wo) = target_stream.split();

        let client_to_server = tokio::io::copy(&mut ri, &mut wo);
        let server_to_client = tokio::io::copy(&mut ro, &mut wi);

        let _ = tokio::try_join!(client_to_server, server_to_client);
        Ok(())
    } else {
        write_simple_response(&mut stream, "502 Bad Gateway", "Upstream connect failed").await;
        Err("Upstream CONNECT failed".to_owned())
    }
}

fn inject_surrogate_headers(
    headers: &[(String, String)],
    target_host: &str,
    rt: &ProxyRuntime,
) -> Result<InjectedEgress, (&'static str, String)> {
    let mut out_headers = Vec::with_capacity(headers.len());
    let mut injected_secrets = Vec::new();
    let mut revocations = Vec::new();

    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if lower == "proxy-connection" || lower == "proxy-authorization" {
            continue;
        }
        let surrogates = extract_surrogate_tokens(value);
        if surrogates.is_empty() {
            out_headers.push((name.clone(), value.clone()));
            continue;
        }
        let (Some(broker), Some(tenant_id)) = (rt.broker.as_ref(), rt.tenant_id.as_deref()) else {
            return Err((
                "403 Forbidden",
                "Surrogate credential presented without active sentinel broker".to_owned(),
            ));
        };
        let mut rewritten = value.clone();
        for tok in surrogates {
            let resolved = broker
                .resolve_for_egress(&tok, tenant_id, rt.session_id.as_deref(), target_host)
                .map_err(|err| match err {
                    SentinelError::Revoked => (
                        "401 Unauthorized",
                        "Surrogate credential has been revoked".to_owned(),
                    ),
                    SentinelError::TenantMismatch | SentinelError::SessionMismatch => (
                        "403 Forbidden",
                        "Surrogate credential denied: tenant/session isolation violation"
                            .to_owned(),
                    ),
                    SentinelError::HostNotAllowed { .. } => (
                        "403 Forbidden",
                        "Surrogate credential denied for destination host".to_owned(),
                    ),
                    _ => (
                        "403 Forbidden",
                        "Surrogate credential verification failed".to_owned(),
                    ),
                })?;
            rewritten = rewritten.replace(&tok, resolved.secret());
            injected_secrets.push(resolved.secret_zeroizing());
            revocations.push(resolved.revocation_rx());
        }
        out_headers.push((name.clone(), rewritten));
    }

    Ok(InjectedEgress {
        headers: out_headers,
        secrets: injected_secrets,
        revocations,
    })
}

async fn forward_scanned_response(
    upstream: &mut TcpStream,
    client: &mut TcpStream,
    scanner: &ResponseLeakScanner,
    revocations: &[watch::Receiver<bool>],
) -> Result<(), String> {
    let raw_head = tokio::select! {
        biased;
        _ = wait_any_revoked(revocations) => {
            write_simple_response(client, "401 Unauthorized", "Surrogate credential revoked").await;
            return Err("Surrogate revoked while awaiting upstream headers".to_owned());
        }
        res = read_http_head(upstream) => res?,
    };
    if raw_head.is_empty() {
        write_simple_response(client, "502 Bad Gateway", "Empty upstream response").await;
        return Err("Empty upstream response".to_owned());
    }
    let Some(header_end) = find_header_end(&raw_head) else {
        write_simple_response(client, "502 Bad Gateway", "Malformed upstream response").await;
        return Err("Malformed upstream HTTP response".to_owned());
    };
    let head_str = String::from_utf8_lossy(&raw_head[..header_end]);
    let mut lines = head_str.split("\r\n");
    let status_line = lines
        .next()
        .unwrap_or("HTTP/1.1 502 Bad Gateway")
        .to_owned();
    let mut response_headers = Vec::new();
    let mut is_chunked = false;
    let mut content_length: Option<usize> = None;

    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, val)) = line.split_once(':') else {
            continue;
        };
        let name_trimmed = name.trim();
        let val_trimmed = val.trim();
        let lower = name_trimmed.to_ascii_lowercase();
        if lower == "transfer-encoding" && val_trimmed.to_ascii_lowercase().contains("chunked") {
            is_chunked = true;
        }
        if lower == "content-length" {
            content_length = val_trimmed.parse::<usize>().ok();
        }
        let scanned = match scanner.scan_header_value(name_trimmed, val_trimmed) {
            Ok(report) => report.sanitized,
            Err(_) => {
                write_simple_response(
                    client,
                    "502 Bad Gateway",
                    "Upstream response blocked by sentinel leak scanner",
                )
                .await;
                return Err("Upstream response header contained leaked secret".to_owned());
            }
        };
        response_headers.push((name_trimmed.to_owned(), scanned));
    }

    let initial_body = &raw_head[header_end + 4..];

    if !is_chunked && content_length.is_some_and(|len| len <= MAX_BUFFERED_RESPONSE_BYTES) {
        let target_len = content_length.unwrap_or(0);
        let mut body = Vec::with_capacity(target_len);
        body.extend_from_slice(&initial_body[..initial_body.len().min(target_len)]);
        while body.len() < target_len {
            let mut buf = [0u8; 4096];
            let to_read = (target_len - body.len()).min(buf.len());
            let n = tokio::select! {
                biased;
                _ = wait_any_revoked(revocations) => {
                    write_simple_response(client, "401 Unauthorized", "Surrogate credential revoked").await;
                    return Err("Surrogate revoked during response body".to_owned());
                }
                res = upstream.read(&mut buf[..to_read]) => res.map_err(|e| e.to_string())?,
            };
            if n == 0 {
                break;
            }
            body.extend_from_slice(&buf[..n]);
        }
        let sanitized_body = match scanner.scan_text(&String::from_utf8_lossy(&body)) {
            Ok(report) => report.sanitized.into_bytes(),
            Err(_) => {
                write_simple_response(
                    client,
                    "502 Bad Gateway",
                    "Upstream response blocked by sentinel leak scanner",
                )
                .await;
                return Err("Upstream response body contained leaked secret".to_owned());
            }
        };
        let mut out = format!("{status_line}\r\n");
        for (name, val) in response_headers {
            let lower = name.to_ascii_lowercase();
            if lower == "content-length" || lower == "connection" {
                continue;
            }
            out.push_str(&format!("{name}: {val}\r\n"));
        }
        out.push_str(&format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            sanitized_body.len()
        ));
        let _ = client.write_all(out.as_bytes()).await;
        let _ = client.write_all(&sanitized_body).await;
        return Ok(());
    }

    // Streaming / chunked / close-delimited response scanning.
    let mut head_out = format!("{status_line}\r\n");
    for (name, val) in response_headers {
        let lower = name.to_ascii_lowercase();
        if lower == "content-length" || lower == "transfer-encoding" || lower == "connection" {
            continue;
        }
        head_out.push_str(&format!("{name}: {val}\r\n"));
    }
    head_out.push_str("Connection: close\r\n\r\n");

    let mut redactor = scanner.streaming_redactor();
    if scanner.action() == LeakAction::Block {
        // Buffer up to limit when blocking so we can still send clean 502 on leak.
        let mut all = Vec::new();
        all.extend_from_slice(initial_body);
        let mut buf = [0u8; 4096];
        while all.len() < MAX_BUFFERED_RESPONSE_BYTES {
            let n = tokio::select! {
                biased;
                _ = wait_any_revoked(revocations) => {
                    write_simple_response(client, "401 Unauthorized", "Surrogate credential revoked").await;
                    return Err("Surrogate revoked during response stream".to_owned());
                }
                res = upstream.read(&mut buf) => res.map_err(|e| e.to_string())?,
            };
            if n == 0 {
                break;
            }
            all.extend_from_slice(&buf[..n]);
        }
        let body_bytes = if is_chunked {
            decode_chunked_bytes(&all)
        } else {
            all
        };
        let mut out_chunk = match redactor.push_chunk(&body_bytes) {
            Ok(bytes) => bytes,
            Err(_) => {
                write_simple_response(
                    client,
                    "502 Bad Gateway",
                    "Upstream response blocked by sentinel leak scanner",
                )
                .await;
                return Err("Upstream streaming response blocked by leak scanner".to_owned());
            }
        };
        match redactor.finish() {
            Ok(tail) => out_chunk.extend_from_slice(&tail),
            Err(_) => {
                write_simple_response(
                    client,
                    "502 Bad Gateway",
                    "Upstream response blocked by sentinel leak scanner",
                )
                .await;
                return Err("Upstream streaming response blocked by leak scanner".to_owned());
            }
        }
        let _ = client.write_all(head_out.as_bytes()).await;
        let _ = client.write_all(&out_chunk).await;
        return Ok(());
    }

    let _ = client.write_all(head_out.as_bytes()).await;
    let first_payload = if is_chunked {
        decode_chunked_bytes(initial_body)
    } else {
        initial_body.to_vec()
    };
    if !first_payload.is_empty()
        && let Ok(clean) = redactor.push_chunk(&first_payload)
        && !clean.is_empty()
    {
        let _ = client.write_all(&clean).await;
    }
    let mut buf = [0u8; 4096];
    loop {
        let n = tokio::select! {
            biased;
            _ = wait_any_revoked(revocations) => {
                return Err("Surrogate revoked during streaming response".to_owned());
            }
            res = upstream.read(&mut buf) => res.map_err(|e| e.to_string())?,
        };
        if n == 0 {
            break;
        }
        let slice = if is_chunked {
            decode_chunked_bytes(&buf[..n])
        } else {
            buf[..n].to_vec()
        };
        if !slice.is_empty()
            && let Ok(clean) = redactor.push_chunk(&slice)
            && !clean.is_empty()
        {
            let _ = client.write_all(&clean).await;
        }
    }
    if let Ok(tail) = redactor.finish()
        && !tail.is_empty()
    {
        let _ = client.write_all(&tail).await;
    }
    Ok(())
}

async fn wait_any_revoked(revocations: &[watch::Receiver<bool>]) {
    if revocations.is_empty() {
        std::future::pending::<()>().await;
        return;
    }
    if revocations.iter().any(|rx| *rx.borrow()) {
        return;
    }
    let futs = revocations.iter().cloned().map(|mut rx| {
        Box::pin(async move {
            loop {
                if *rx.borrow_and_update() {
                    return;
                }
                if rx.changed().await.is_err() {
                    return;
                }
            }
        })
    });
    let _ = futures_util::future::select_all(futs).await;
}

fn decode_chunked_bytes(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cursor = input;
    while !cursor.is_empty() {
        let Some(crlf) = cursor.windows(2).position(|w| w == b"\r\n") else {
            out.extend_from_slice(cursor);
            break;
        };
        let size_str = String::from_utf8_lossy(&cursor[..crlf]);
        let size_hex = size_str.split(';').next().unwrap_or("").trim();
        let Ok(chunk_len) = usize::from_str_radix(size_hex, 16) else {
            out.extend_from_slice(cursor);
            break;
        };
        cursor = &cursor[crlf + 2..];
        if chunk_len == 0 {
            break;
        }
        let take = chunk_len.min(cursor.len());
        out.extend_from_slice(&cursor[..take]);
        cursor = &cursor[take..];
        if cursor.starts_with(b"\r\n") {
            cursor = &cursor[2..];
        }
    }
    out
}

async fn read_http_head(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf).await.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
        if find_header_end(&data).is_some() {
            break;
        }
        if data.len() > MAX_HEADER_BYTES {
            return Err("HTTP header block exceeds maximum allowed size".to_owned());
        }
    }
    Ok(data)
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_request_head(raw: &[u8]) -> Result<ParsedRequestHead, String> {
    let header_end =
        find_header_end(raw).ok_or_else(|| "Incomplete HTTP request header block".to_owned())?;
    let head_str = String::from_utf8_lossy(&raw[..header_end]);
    let mut lines = head_str.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| "Missing HTTP request line".to_owned())?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "Missing HTTP method".to_owned())?
        .to_owned();
    let request_target = parts
        .next()
        .ok_or_else(|| "Missing HTTP request target".to_owned())?
        .to_owned();
    let version = parts.next().unwrap_or("HTTP/1.1").to_owned();

    let mut headers = Vec::new();
    let mut host_header: Option<String> = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err("Malformed HTTP header line".to_owned());
        };
        let n = name.trim().to_owned();
        let v = value.trim().to_owned();
        if n.eq_ignore_ascii_case("host") {
            host_header = Some(v.clone());
        }
        headers.push((n, v));
    }

    let is_connect = method.eq_ignore_ascii_case("CONNECT");
    let (target_host, target_port) = if is_connect {
        parse_host_port(&request_target, 443)?
    } else if request_target.starts_with("http://") || request_target.starts_with("https://") {
        let url = reqwest::Url::parse(&request_target)
            .map_err(|_| "Invalid absolute proxy request URL".to_owned())?;
        let url_host = url
            .host_str()
            .ok_or_else(|| "Absolute proxy URL missing host".to_owned())?
            .trim_matches(|c| c == '[' || c == ']')
            .to_owned();
        let url_port = url.port_or_known_default().unwrap_or(80);
        if let Some(ref hh) = host_header {
            let (hh_host, _) = parse_host_port(hh, url_port)?;
            if !hh_host.eq_ignore_ascii_case(&url_host) {
                return Err("Host header disagrees with request URI authority".to_owned());
            }
        }
        (url_host, url_port)
    } else if let Some(ref hh) = host_header {
        parse_host_port(hh, 80)?
    } else {
        return Err("Missing Host header or CONNECT target".to_owned());
    };

    Ok(ParsedRequestHead {
        method,
        request_target,
        version,
        headers,
        target_host,
        target_port,
        is_connect,
        initial_body: raw[header_end + 4..].to_vec(),
    })
}

fn parse_host_port(authority: &str, default_port: u16) -> Result<(String, u16), String> {
    let trimmed = authority.trim();
    if trimmed.is_empty() {
        return Err("Empty host authority".to_owned());
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            return Err("Malformed IPv6 authority".to_owned());
        };
        let port = if let Some(port_str) = tail.strip_prefix(':') {
            port_str
                .parse::<u16>()
                .map_err(|_| "Invalid authority port".to_owned())?
        } else if tail.is_empty() {
            default_port
        } else {
            return Err("Malformed IPv6 authority suffix".to_owned());
        };
        return Ok((host.to_owned(), port));
    }
    if let Some((host, port_str)) = trimmed.rsplit_once(':')
        && !host.contains(':')
    {
        let port = port_str
            .parse::<u16>()
            .map_err(|_| "Invalid authority port".to_owned())?;
        return Ok((host.to_owned(), port));
    }
    Ok((trimmed.to_owned(), default_port))
}

fn serialize_upstream_request(parsed: &ParsedRequestHead, headers: &[(String, String)]) -> Vec<u8> {
    let origin_target = if parsed.request_target.starts_with("http://")
        || parsed.request_target.starts_with("https://")
    {
        reqwest::Url::parse(&parsed.request_target)
            .map(|u| match u.query() {
                Some(q) => format!("{}?{q}", u.path()),
                None => u.path().to_owned(),
            })
            .unwrap_or_else(|_| "/".to_owned())
    } else {
        parsed.request_target.clone()
    };

    let mut out = format!("{} {} {}\r\n", parsed.method, origin_target, parsed.version);
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("connection") {
            continue;
        }
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    out.into_bytes()
}

fn header_content_length(headers: &[(String, String)]) -> Option<usize> {
    headers.iter().find_map(|(k, v)| {
        k.eq_ignore_ascii_case("content-length")
            .then(|| v.parse::<usize>().ok())
            .flatten()
    })
}

async fn write_simple_response(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;

    struct EchoUpstream {
        port: u16,
        seen_auth: Arc<Mutex<Vec<String>>>,
        hits: Arc<AtomicUsize>,
        _shutdown: oneshot::Sender<()>,
    }

    async fn read_http_response(stream: &mut TcpStream) -> String {
        let mut resp = Vec::new();
        let _ = stream.read_to_end(&mut resp).await;
        String::from_utf8_lossy(&resp).into_owned()
    }

    async fn start_echo_upstream(
        reflect_secret_in_body: bool,
        reflect_secret_in_header: bool,
    ) -> EchoUpstream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen_auth = Arc::new(Mutex::new(Vec::new()));
        let hit_count = Arc::new(AtomicUsize::new(0));
        let auth_clone = Arc::clone(&seen_auth);
        let hits_clone = Arc::clone(&hit_count);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    res = listener.accept() => {
                        let Ok((mut sock, _)) = res else { break };
                        let req = read_http_head(&mut sock).await.unwrap_or_default();
                        let req_str = String::from_utf8_lossy(&req).into_owned();
                        hits_clone.fetch_add(1, Ordering::SeqCst);
                        let auth = req_str
                            .lines()
                            .find_map(|l| {
                                l.split_once(':')
                                    .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                                    .map(|(_, v)| v.trim().to_owned())
                            })
                            .unwrap_or_default();
                        auth_clone.lock().await.push(auth.clone());

                        let body = if reflect_secret_in_body {
                            format!("echoed auth: {auth}")
                        } else {
                            "upstream ok".to_owned()
                        };
                        let extra_hdr = if reflect_secret_in_header {
                            format!("X-Reflected-Auth: {auth}\r\n")
                        } else {
                            String::new()
                        };
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n{extra_hdr}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    }
                }
            }
        });
        EchoUpstream {
            port,
            seen_auth,
            hits: hit_count,
            _shutdown: shutdown_tx,
        }
    }

    #[tokio::test]
    async fn test_proxy_denied() {
        let proxy = NetworkProxy::new(
            vec!["example.com".to_string()],
            "test_agent".to_string(),
            "test_task".to_string(),
        );
        let (port, _shutdown) = proxy.start(0).await.unwrap();

        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: evil.com\r\n\r\n")
            .await
            .unwrap();

        let response = read_http_response(&mut stream).await;
        assert!(response.contains("403 Forbidden"));
    }

    #[tokio::test]
    async fn test_proxy_empty_allowlist_default_deny() {
        let proxy = NetworkProxy::new(vec![], "test_agent".to_string(), "test_task".to_string());
        let (port, _shutdown) = proxy.start(0).await.unwrap();

        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .await
            .unwrap();

        let response = read_http_response(&mut stream).await;
        assert!(response.contains("403 Forbidden"));
    }

    #[tokio::test]
    async fn test_proxy_allowed() {
        let proxy = NetworkProxy::new(
            vec!["example.com".to_string()],
            "test_agent".to_string(),
            "test_task".to_string(),
        );
        let (port, _shutdown) = proxy.start(0).await.unwrap();

        let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))
            .await
            .unwrap();
        stream
            .write_all(b"CONNECT api.example.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();

        let mut buffer = [0; 1024];
        let bytes_read = stream.read(&mut buffer).await.unwrap();
        let response = String::from_utf8_lossy(&buffer[..bytes_read]);

        assert!(
            response.contains("200 Connection Established") || response.contains("502 Bad Gateway")
        );
    }

    #[tokio::test]
    async fn test_proxy_ssrf_blocks_metadata_rfc1918_and_loopback_without_injecting_secret() {
        let upstream = start_echo_upstream(false, false).await;
        let broker = SurrogateTokenBroker::new();
        let (_tx, rx) = watch::channel(false);
        let secret = "sk-live-never-send-to-ssrf-target-9999";
        let surrogate = broker
            .issue(
                "tenant-a",
                "sess-a",
                "openai_api",
                secret,
                vec![
                    "169.254.169.254".into(),
                    "10.0.0.1".into(),
                    "127.0.0.1".into(),
                ],
                rx,
            )
            .unwrap();

        // Loopback NOT opted-in -> 169.254.169.254, 10.0.0.1, AND 127.0.0.1 must fail closed 403.
        let guard = SsrfGuard::new(vec![
            "169.254.169.254".into(),
            "10.0.0.1".into(),
            "127.0.0.1".into(),
        ]);
        let sentinel = Sentinel::new(broker, guard, ResponseLeakScanner::new());
        let proxy = NetworkProxy::new(vec![], "agent-1".into(), "task-1".into())
            .with_sentinel("tenant-a", "sess-a", sentinel);
        let (proxy_port, _stop) = proxy.start(0).await.unwrap();

        for target in [
            "169.254.169.254:80".to_owned(),
            "10.0.0.1:80".to_owned(),
            format!("127.0.0.1:{}", upstream.port),
        ] {
            let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).await.unwrap();
            let req = format!(
                "GET /latest/meta-data HTTP/1.1\r\nHost: {target}\r\nAuthorization: Bearer {surrogate}\r\n\r\n"
            );
            stream.write_all(req.as_bytes()).await.unwrap();
            let resp = read_http_response(&mut stream).await;
            assert!(
                resp.contains("403 Forbidden"),
                "expected 403 for {target}, got: {resp}"
            );
            assert!(!resp.contains(secret));
        }

        assert_eq!(upstream.hits.load(Ordering::SeqCst), 0);
        assert!(upstream.seen_auth.lock().await.is_empty());
    }

    #[tokio::test]
    async fn test_proxy_jit_surrogate_injection_leak_redaction_revocation_and_tenant_isolation() {
        let upstream = start_echo_upstream(true, true).await;
        let broker = SurrogateTokenBroker::new();
        let (revoke_tx, revoke_rx) = watch::channel(false);
        let real_secret = "sk-live-real-tenant-a-secret-44556677";

        let surrogate_a = broker
            .issue(
                "tenant-a",
                "sess-a",
                "openai_api",
                real_secret,
                vec!["127.0.0.1".into()],
                revoke_rx,
            )
            .unwrap();
        assert!(!surrogate_a.contains(real_secret));

        let guard = SsrfGuard::new(vec!["127.0.0.1".into()]).with_loopback_allowed_for_tests(true);
        let sentinel = Sentinel::new(broker.clone(), guard.clone(), ResponseLeakScanner::new());

        let proxy_a = NetworkProxy::new(vec![], "agent-a".into(), "task-a".into()).with_sentinel(
            "tenant-a",
            "sess-a",
            sentinel.clone(),
        );
        let (port_a, _stop_a) = proxy_a.start(0).await.unwrap();

        // 1. Tenant A sends surrogate -> upstream receives REAL secret, reflected body & header are scrubbed!
        let mut s1 = TcpStream::connect(("127.0.0.1", port_a)).await.unwrap();
        let req1 = format!(
            "GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {surrogate_a}\r\n\r\n",
            upstream.port
        );
        s1.write_all(req1.as_bytes()).await.unwrap();
        let resp1 = read_http_response(&mut s1).await;
        assert!(resp1.contains("200 OK"), "unexpected response: {resp1}");
        assert!(
            !resp1.contains(real_secret),
            "leaked real secret in response: {resp1}"
        );
        assert!(resp1.contains("[REDACTED]"));
        assert_eq!(
            upstream.seen_auth.lock().await.as_slice(),
            &[format!("Bearer {real_secret}")]
        );

        // 2. Cross-tenant isolation: Tenant B's proxy rejects Tenant A's surrogate with 403!
        let proxy_b = NetworkProxy::new(vec![], "agent-b".into(), "task-b".into())
            .with_sentinel("tenant-b", "sess-b", sentinel);
        let (port_b, _stop_b) = proxy_b.start(0).await.unwrap();
        let mut s2 = TcpStream::connect(("127.0.0.1", port_b)).await.unwrap();
        let req2 = format!(
            "GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {surrogate_a}\r\n\r\n",
            upstream.port
        );
        s2.write_all(req2.as_bytes()).await.unwrap();
        let resp2 = read_http_response(&mut s2).await;
        assert!(
            resp2.contains("403 Forbidden"),
            "expected 403 for cross-tenant use: {resp2}"
        );
        assert_eq!(upstream.hits.load(Ordering::SeqCst), 1);

        // 3. Revocation via watch::Receiver<bool> immediately rejects subsequent requests (401)!
        revoke_tx.send_replace(true);
        let mut s3 = TcpStream::connect(("127.0.0.1", port_a)).await.unwrap();
        s3.write_all(req1.as_bytes()).await.unwrap();
        let resp3 = read_http_response(&mut s3).await;
        assert!(
            resp3.contains("401 Unauthorized"),
            "expected 401 after revocation: {resp3}"
        );
        assert_eq!(upstream.hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sentinel_unit_broker_scanner_and_budget() {
        use crate::sentinel::{SURROGATE_TOKEN_PREFIX, WasmToolBudget};

        let broker = SurrogateTokenBroker::new();
        let (tx, rx) = watch::channel(false);
        let sec = "sk-live-CanarySecret+1/2=";
        let tok = broker
            .issue(
                "t-a",
                "s-1",
                "openai_api",
                sec,
                vec!["api.openai.com".into()],
                rx,
            )
            .unwrap();
        assert!(tok.starts_with(SURROGATE_TOKEN_PREFIX) && !tok.contains("Canary"));
        let r = broker
            .resolve_for_egress(&tok, "t-a", Some("s-1"), "api.openai.com")
            .unwrap();
        assert_eq!(r.secret(), sec);
        assert!(!format!("{r:?}").contains("Canary"));
        assert_eq!(
            broker
                .resolve_for_egress(&tok, "t-b", Some("s-1"), "api.openai.com")
                .unwrap_err(),
            SentinelError::TenantMismatch
        );
        assert_eq!(
            broker
                .resolve_for_egress(&tok, "t-a", Some("s-2"), "api.openai.com")
                .unwrap_err(),
            SentinelError::SessionMismatch
        );
        assert!(matches!(
            broker.resolve_for_egress(&tok, "t-a", Some("s-1"), "evil.net"),
            Err(SentinelError::HostNotAllowed { .. })
        ));
        tx.send_replace(true);
        assert_eq!(
            broker
                .resolve_for_egress(&tok, "t-a", Some("s-1"), "api.openai.com")
                .unwrap_err(),
            SentinelError::Revoked
        );

        let sc = ResponseLeakScanner::new();
        sc.register_secret(sec);
        let b64 = "c2stbGl2ZS1DYW5hcnlTZWNyZXQrMS8yPQ==";
        let ue = "sk-live-CanarySecret%2B1%2F2%3D";
        let txt = format!(
            "r={sec} b={b64} u={ue} a=Bearer ExtBearerTok123456 k=sk-proj-abcdefghijklmnop1234"
        );
        let rep = sc.scan_text(&txt).unwrap().sanitized;
        for f in [
            sec,
            b64,
            ue,
            "ExtBearerTok123456",
            "sk-proj-abcdefghijklmnop1234",
        ] {
            assert!(!rep.contains(f), "expected {f} redacted in {rep}");
        }
        let mut st = sc.streaming_redactor();
        let mut out = st
            .push_chunk(format!("c1 {}", &b64[..10]).as_bytes())
            .unwrap();
        out.extend(
            st.push_chunk(format!("{} end", &b64[10..]).as_bytes())
                .unwrap(),
        );
        out.extend(st.finish().unwrap());
        assert_eq!(String::from_utf8(out).unwrap(), "c1 [REDACTED] end");
        assert!(
            sc.with_action(LeakAction::Block)
                .scan_header_value("x-echo", sec)
                .is_err()
        );

        let w = WasmToolBudget::default();
        assert!(!w.allow_network && w.validate().is_ok() && w.enforce_usage(1024, 100, 50).is_ok());
        assert!(w.enforce_usage(w.max_memory_bytes + 1, 1, 1).is_err());
        assert!(w.enforce_usage(1, w.max_fuel + 1, 1).is_err());
        assert!(w.enforce_usage(1, 1, w.max_wall_ms + 1).is_err());
    }

    #[tokio::test]
    async fn sentinel_unit_ssrf_guard() {
        assert!(matches!(
            SsrfGuard::new(vec![])
                .resolve_and_validate("example.com", 443)
                .await,
            Err(SentinelError::HostNotAllowed { .. })
        ));
        let blocked = [
            "10.0.0.1",
            "172.16.0.5",
            "192.168.1.10",
            "169.254.169.254",
            "127.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:169.254.169.254",
            "localhost",
        ];
        let mut allow: Vec<String> = blocked.iter().map(|s| (*s).to_owned()).collect();
        allow.push("93.184.216.34".into());
        let guard = SsrfGuard::new(allow);
        for t in blocked {
            assert!(
                matches!(
                    guard.resolve_and_validate(t, 80).await,
                    Err(SentinelError::SsrfBlocked { .. })
                ),
                "expected SSRF block for {t}"
            );
        }
        assert_eq!(
            guard
                .resolve_and_validate("93.184.216.34", 443)
                .await
                .unwrap()
                .len(),
            1
        );
        let lb = SsrfGuard::new(vec!["127.0.0.1".into(), "169.254.169.254".into()])
            .with_loopback_allowed_for_tests(true);
        assert!(lb.resolve_and_validate("127.0.0.1", 8080).await.is_ok());
        assert!(matches!(
            lb.resolve_and_validate("169.254.169.254", 80).await,
            Err(SentinelError::SsrfBlocked { .. })
        ));
    }
}
