//! Tier 1 Sentinel: surrogate credential broker, hardened SSRF egress guard,
//! upstream response secret leak scanner, and WASM tool budget ceilings.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use uuid::Uuid;
use zeroize::Zeroizing;

pub const SURROGATE_TOKEN_PREFIX: &str = "ohc_surr_";
pub const REDACTED_MARKER: &str = "[REDACTED]";
const MAX_WASM_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_WASM_WALL_MS: u64 = 300_000;
const MAX_SHAPE_RETAIN_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SentinelError {
    InvalidInput(String),
    UnknownSurrogate,
    TenantMismatch,
    SessionMismatch,
    HostNotAllowed { host: String },
    Revoked,
    SsrfBlocked { target: String, reason: String },
    DnsResolutionFailed { host: String, reason: String },
    ResponseLeakDetected(String),
    BudgetExceeded(String),
}

impl std::fmt::Display for SentinelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidInput(m) => write!(f, "invalid sentinel input: {m}"),
            Self::UnknownSurrogate => f.write_str("unknown surrogate token"),
            Self::TenantMismatch => f.write_str("surrogate tenant mismatch"),
            Self::SessionMismatch => f.write_str("surrogate session mismatch"),
            Self::HostNotAllowed { host } => write!(f, "host '{host}' not allowed"),
            Self::Revoked => f.write_str("surrogate credential revoked"),
            Self::SsrfBlocked { target, reason } => write!(f, "SSRF blocked '{target}': {reason}"),
            Self::DnsResolutionFailed { host, reason } => {
                write!(f, "DNS failed '{host}': {reason}")
            }
            Self::ResponseLeakDetected(r) => write!(f, "response leak: {r}"),
            Self::BudgetExceeded(r) => write!(f, "WASM budget exceeded: {r}"),
        }
    }
}
impl std::error::Error for SentinelError {}

fn ssrf_err(target: impl Into<String>, reason: impl Into<String>) -> SentinelError {
    SentinelError::SsrfBlocked {
        target: target.into(),
        reason: reason.into(),
    }
}

pub fn normalize_host(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty()
        || t.bytes()
            .any(|b| b <= 0x20 || b == 0x7f || b"/?#@\\".contains(&b))
    {
        return None;
    }
    if let Some(v6) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return v6.parse::<Ipv6Addr>().ok().map(|ip| ip.to_string());
    }
    let low = t.trim_end_matches('.').to_ascii_lowercase();
    (!low.is_empty()).then(|| low.parse::<IpAddr>().map_or(low, |ip| ip.to_string()))
}

fn norm_allow(raw: &str) -> Option<String> {
    match raw.trim().strip_prefix("*.") {
        Some(r) => normalize_host(r)
            .filter(|n| n.parse::<IpAddr>().is_err())
            .map(|n| format!("*.{n}")),
        None => normalize_host(raw),
    }
}

pub fn host_matches_allowlist(target_host: &str, allowed: &[String]) -> bool {
    let Some(target) = (!allowed.is_empty())
        .then(|| normalize_host(target_host))
        .flatten()
    else {
        return false;
    };
    let is_ip = target.parse::<IpAddr>().is_ok();
    allowed.iter().any(|pat| {
        let wc = pat.trim().strip_prefix("*.");
        let Some(a) = normalize_host(wc.unwrap_or(pat.trim())) else {
            return false;
        };
        if is_ip || a.parse::<IpAddr>().is_ok() {
            return wc.is_none() && target == a;
        }
        let dot = target.len().checked_sub(a.len() + 1);
        dot.is_some_and(|i| target.as_bytes()[i] == b'.' && target.ends_with(&a))
            || (wc.is_none() && target == a)
    })
}

pub fn is_explicit_loopback_host(host: &str) -> bool {
    normalize_host(host)
        .is_some_and(|n| n == "localhost" || n.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()))
}

// --- 1. SurrogateTokenBroker ---

#[derive(Clone)]
struct SurrogateEntry {
    tenant_id: String,
    session_id: String,
    provider_id: String,
    real_secret: Zeroizing<String>,
    allowed_hosts: Vec<String>,
    revocation_rx: watch::Receiver<bool>,
}

pub struct ResolvedSurrogate {
    pub tenant_id: String,
    pub session_id: String,
    pub provider_id: String,
    secret: Zeroizing<String>,
    revocation_rx: watch::Receiver<bool>,
}

impl ResolvedSurrogate {
    pub fn secret(&self) -> &str {
        self.secret.as_str()
    }
    pub fn secret_zeroizing(&self) -> Zeroizing<String> {
        self.secret.clone()
    }
    pub fn revocation_rx(&self) -> watch::Receiver<bool> {
        self.revocation_rx.clone()
    }
}

impl std::fmt::Debug for ResolvedSurrogate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResolvedSurrogate(<redacted>)")
    }
}

#[derive(Clone, Default)]
pub struct SurrogateTokenBroker {
    entries: Arc<RwLock<HashMap<String, SurrogateEntry>>>,
}

impl std::fmt::Debug for SurrogateTokenBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SurrogateTokenBroker(<redacted>)")
    }
}

impl SurrogateTokenBroker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn issue(
        &self,
        tenant_id: impl Into<String>,
        session_id: impl Into<String>,
        provider_id: impl Into<String>,
        real_secret: impl Into<String>,
        allowed_hosts: Vec<String>,
        revocation_rx: watch::Receiver<bool>,
    ) -> Result<String, SentinelError> {
        let (tid, sid, pid) = (tenant_id.into(), session_id.into(), provider_id.into());
        let sec = Zeroizing::new(real_secret.into());
        if [&tid, &sid, &pid, &*sec]
            .iter()
            .any(|v| v.trim().is_empty() || v.chars().any(char::is_control))
        {
            return Err(SentinelError::InvalidInput(
                "empty or control-char field".into(),
            ));
        }
        if *revocation_rx.borrow() {
            return Err(SentinelError::Revoked);
        }
        if allowed_hosts.is_empty() {
            return Err(SentinelError::InvalidInput(
                "allowed_hosts must be non-empty".into(),
            ));
        }
        let hosts: Vec<String> = allowed_hosts
            .into_iter()
            .map(|h| norm_allow(&h).ok_or(SentinelError::InvalidInput(h)))
            .collect::<Result<_, _>>()?;
        let tok = format!(
            "{SURROGATE_TOKEN_PREFIX}{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        );
        let e = SurrogateEntry {
            tenant_id: tid,
            session_id: sid,
            provider_id: pid,
            real_secret: sec,
            allowed_hosts: hosts,
            revocation_rx,
        };
        self.entries
            .write()
            .expect("broker lock")
            .insert(tok.clone(), e);
        Ok(tok)
    }

    pub fn resolve_for_egress(
        &self,
        token: &str,
        expected_tenant: &str,
        expected_session: Option<&str>,
        target_host: &str,
    ) -> Result<ResolvedSurrogate, SentinelError> {
        let ok_fmt = token
            .strip_prefix(SURROGATE_TOKEN_PREFIX)
            .is_some_and(|r| r.len() == 64 && r.bytes().all(|b| b.is_ascii_hexdigit()));
        if !ok_fmt {
            return Err(SentinelError::UnknownSurrogate);
        }
        let e = self
            .entries
            .read()
            .expect("broker lock")
            .get(token)
            .cloned()
            .ok_or(SentinelError::UnknownSurrogate)?;
        if *e.revocation_rx.borrow() {
            return Err(SentinelError::Revoked);
        }
        if e.tenant_id != expected_tenant {
            return Err(SentinelError::TenantMismatch);
        }
        if expected_session.is_some_and(|sid| e.session_id != sid) {
            return Err(SentinelError::SessionMismatch);
        }
        if !host_matches_allowlist(target_host, &e.allowed_hosts) {
            return Err(SentinelError::HostNotAllowed {
                host: target_host.to_owned(),
            });
        }
        Ok(ResolvedSurrogate {
            tenant_id: e.tenant_id,
            session_id: e.session_id,
            provider_id: e.provider_id,
            secret: e.real_secret,
            revocation_rx: e.revocation_rx,
        })
    }

    pub fn revoke_token(&self, token: &str) -> bool {
        self.entries
            .write()
            .expect("broker lock")
            .remove(token)
            .is_some()
    }
}

pub fn extract_surrogate_tokens(text: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"ohc_surr_[0-9a-fA-F]{64}").expect("re"));
    let mut seen = HashSet::new();
    re.find_iter(text)
        .filter(|m| seen.insert(m.as_str()))
        .map(|m| m.as_str().to_owned())
        .collect()
}

// --- 2. SsrfGuard ---

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SsrfGuard {
    allowed_domains: Vec<String>,
    allow_loopback_for_tests: bool,
}

impl SsrfGuard {
    pub fn new(allowed_domains: Vec<String>) -> Self {
        Self {
            allowed_domains: allowed_domains
                .into_iter()
                .filter_map(|d| norm_allow(&d))
                .collect(),
            allow_loopback_for_tests: false,
        }
    }
    /// Test-only opt-in allowing loopback (`127.0.0.0/8`, `::1`) when also allowlisted.
    pub fn with_loopback_allowed_for_tests(mut self, allow: bool) -> Self {
        self.allow_loopback_for_tests = allow;
        self
    }
    pub fn allowed_domains(&self) -> &[String] {
        &self.allowed_domains
    }

    pub fn validate_ip(&self, ip: IpAddr) -> Result<(), SentinelError> {
        let res = match ip {
            IpAddr::V4(v4) => check_ipv4(v4, self.allow_loopback_for_tests),
            IpAddr::V6(v6) => check_ipv6(v6, self.allow_loopback_for_tests),
        };
        res.map_err(|r| ssrf_err(ip.to_string(), r))
    }

    pub async fn resolve_and_validate(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, SentinelError> {
        let norm = normalize_host(host).ok_or_else(|| ssrf_err(host, "invalid host"))?;
        if let Ok(ip) = norm.parse::<IpAddr>() {
            self.validate_ip(ip)?;
            return host_matches_allowlist(&norm, &self.allowed_domains)
                .then(|| vec![SocketAddr::new(ip, port)])
                .ok_or(SentinelError::HostNotAllowed { host: norm });
        }
        if !self.allow_loopback_for_tests && (norm == "localhost" || norm.ends_with(".localhost")) {
            return Err(ssrf_err(norm, "loopback hostname blocked"));
        }
        if !host_matches_allowlist(&norm, &self.allowed_domains) {
            return Err(SentinelError::HostNotAllowed { host: norm });
        }
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((norm.as_str(), port))
            .await
            .map_err(|e| SentinelError::DnsResolutionFailed {
                host: norm.clone(),
                reason: e.to_string(),
            })?
            .collect();
        if addrs.is_empty() {
            return Err(SentinelError::DnsResolutionFailed {
                host: norm,
                reason: "no addresses".into(),
            });
        }
        for a in &addrs {
            self.validate_ip(a.ip())?;
        }
        Ok(addrs)
    }

    pub async fn validate_url(&self, url: &reqwest::Url) -> Result<Vec<SocketAddr>, SentinelError> {
        let host = url
            .host_str()
            .ok_or_else(|| ssrf_err(url.as_str(), "missing host"))?;
        let port = url
            .port_or_known_default()
            .ok_or_else(|| ssrf_err(host, "missing port"))?;
        self.resolve_and_validate(host, port).await
    }
}

fn check_ipv4(ip: Ipv4Addr, allow_lb: bool) -> Result<(), &'static str> {
    match ip.octets() {
        [127, ..] if allow_lb => Ok(()),
        [127, ..] => Err("loopback IPv4 (127.0.0.0/8) blocked"),
        [0, ..] => Err("unspecified IPv4 blocked"),
        [10, ..] | [172, 16..=31, ..] | [192, 168, ..] => Err("RFC1918 private IPv4 blocked"),
        [169, 254, ..] => Err("link-local / cloud metadata IPv4 (169.254.0.0/16) blocked"),
        [100, 64..=127, ..] => Err("CGNAT IPv4 blocked"),
        [224..=255, ..] | [192, 0, 0, _] | [198, 18..=19, ..] => Err("reserved IPv4 blocked"),
        _ => Ok(()),
    }
}

fn check_ipv6(ip: Ipv6Addr, allow_lb: bool) -> Result<(), &'static str> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return check_ipv4(v4, allow_lb);
    }
    if ip.is_loopback() {
        return allow_lb.then_some(()).ok_or("loopback IPv6 (::1) blocked");
    }
    if ip.is_unspecified() {
        return Err("unspecified IPv6 blocked");
    }
    let s = ip.segments();
    if s[..6] == [0; 6] {
        let (hi, lo) = (s[6].to_be_bytes(), s[7].to_be_bytes());
        return check_ipv4(Ipv4Addr::new(hi[0], hi[1], lo[0], lo[1]), allow_lb);
    }
    match s[0] {
        x if x & 0xfe00 == 0xfc00 => Err("ULA IPv6 (fc00::/7) blocked"),
        x if x & 0xffc0 == 0xfe80 => Err("link-local IPv6 (fe80::/10) blocked"),
        x if x & 0xffc0 == 0xfec0 || x & 0xff00 == 0xff00 => Err("reserved IPv6 blocked"),
        _ => Ok(()),
    }
}

// --- 3. ResponseLeakScanner & StreamingLeakRedactor ---

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LeakAction {
    #[default]
    Redact,
    Block,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeakScanReport {
    pub sanitized: String,
    pub leaks_detected: usize,
}

#[derive(Clone, Default)]
pub struct ResponseLeakScanner {
    secrets: Arc<RwLock<Vec<Zeroizing<String>>>>,
    action: LeakAction,
}

impl std::fmt::Debug for ResponseLeakScanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResponseLeakScanner(<redacted>)")
    }
}

impl ResponseLeakScanner {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_action(mut self, action: LeakAction) -> Self {
        self.action = action;
        self
    }
    pub fn action(&self) -> LeakAction {
        self.action
    }
    pub fn register_secret(&self, secret: impl Into<String>) {
        let s = secret.into();
        if !s.is_empty() {
            let mut g = self.secrets.write().expect("scanner lock");
            if !g.iter().any(|e| e.as_str() == s) {
                g.push(Zeroizing::new(s));
            }
        }
    }
    pub fn with_additional_secrets(&self, extras: &[Zeroizing<String>]) -> Self {
        let mut all = self.secrets.read().expect("scanner lock").clone();
        for x in extras {
            if !x.is_empty() && !all.iter().any(|e| e.as_str() == x.as_str()) {
                all.push(x.clone());
            }
        }
        Self {
            secrets: Arc::new(RwLock::new(all)),
            action: self.action,
        }
    }

    fn compiled_needles(&self) -> Vec<Zeroizing<String>> {
        let (mut seen, mut out) = (HashSet::new(), Vec::new());
        let mut add = |c: String, min: usize| {
            if c.len() >= min && seen.insert(c.clone()) {
                out.push(Zeroizing::new(c));
            }
        };
        for s in self.secrets.read().expect("scanner lock").iter() {
            let raw = s.as_str();
            add(raw.to_owned(), 1);
            if raw.len() >= 4 {
                let b64 = base64_std(raw.as_bytes());
                let ub64 = b64.replace('+', "-").replace('/', "_");
                for v in [
                    b64.clone(),
                    b64.trim_end_matches('=').into(),
                    ub64.clone(),
                    ub64.trim_end_matches('=').into(),
                ] {
                    add(v, 4);
                }
                for enc in [
                    percent_encode(raw.as_bytes(), false),
                    percent_encode(raw.as_bytes(), true),
                ] {
                    add(enc.to_ascii_lowercase(), 4);
                    add(enc, 4);
                }
            }
        }
        out.sort_by_key(|b| std::cmp::Reverse(b.len()));
        out
    }

    pub fn scan_text(&self, input: &str) -> Result<LeakScanReport, SentinelError> {
        let rep = scan_text_with_needles(input, &self.compiled_needles());
        if self.action == LeakAction::Block && rep.leaks_detected > 0 {
            return Err(SentinelError::ResponseLeakDetected(format!(
                "{} leak(s)",
                rep.leaks_detected
            )));
        }
        Ok(rep)
    }
    pub fn redact_text(&self, input: &str) -> String {
        scan_text_with_needles(input, &self.compiled_needles()).sanitized
    }
    pub fn scan_header_value(
        &self,
        name: &str,
        value: &str,
    ) -> Result<LeakScanReport, SentinelError> {
        self.scan_text(value)
            .map_err(|_| SentinelError::ResponseLeakDetected(format!("header '{name}'")))
    }
    pub fn streaming_redactor(&self) -> StreamingLeakRedactor {
        StreamingLeakRedactor {
            needles: self.compiled_needles(),
            action: self.action,
            pending: Vec::new(),
        }
    }
}

pub struct StreamingLeakRedactor {
    needles: Vec<Zeroizing<String>>,
    action: LeakAction,
    pending: Vec<u8>,
}

impl StreamingLeakRedactor {
    fn apply(&self, bytes: &[u8], needles: &[Zeroizing<String>]) -> Result<Vec<u8>, SentinelError> {
        let rep = scan_text_with_needles(&String::from_utf8_lossy(bytes), needles);
        if self.action == LeakAction::Block && rep.leaks_detected > 0 {
            return Err(SentinelError::ResponseLeakDetected(
                "secret leak in stream".into(),
            ));
        }
        Ok(rep.sanitized.into_bytes())
    }

    pub fn push_chunk(&mut self, chunk: &[u8]) -> Result<Vec<u8>, SentinelError> {
        self.pending.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((pos, len)) = earliest_safe_needle(&self.pending, &self.needles) {
            if self.action == LeakAction::Block {
                return Err(SentinelError::ResponseLeakDetected("stream secret".into()));
            }
            if pos > 0 {
                out.extend_from_slice(&self.apply(&self.pending[..pos], &[])?);
            }
            out.extend_from_slice(REDACTED_MARKER.as_bytes());
            self.pending.drain(..pos + len);
        }
        let end = self
            .pending
            .len()
            .saturating_sub(retain_tail_len(&self.pending, &self.needles));
        if end > 0 {
            let drained: Vec<u8> = self.pending.drain(..end).collect();
            out.extend_from_slice(&self.apply(&drained, &[])?);
        }
        Ok(out)
    }

    pub fn finish(&mut self) -> Result<Vec<u8>, SentinelError> {
        if self.pending.is_empty() {
            return Ok(Vec::new());
        }
        let rem = std::mem::take(&mut self.pending);
        self.apply(&rem, &self.needles)
    }
}

fn earliest_safe_needle(buf: &[u8], needles: &[Zeroizing<String>]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for nb in needles
        .iter()
        .map(|n| n.as_bytes())
        .filter(|nb| !nb.is_empty() && buf.len() >= nb.len())
    {
        let Some(pos) = buf.windows(nb.len()).position(|w| w == nb) else {
            continue;
        };
        let longer = needles.iter().any(|o| {
            let ob = o.as_bytes();
            ob.len() > nb.len() && buf.len() - pos < ob.len() && ob.starts_with(&buf[pos..])
        });
        if !longer && best.is_none_or(|(bp, bl)| pos < bp || (pos == bp && nb.len() > bl)) {
            best = Some((pos, nb.len()));
        }
    }
    best
}

fn retain_tail_len(buf: &[u8], needles: &[Zeroizing<String>]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let mut retain = match std::str::from_utf8(&buf[buf.len().saturating_sub(3)..]) {
        Err(e) if e.error_len().is_none() => buf.len().min(3) - e.valid_up_to(),
        _ => 0,
    };
    for nb in needles.iter().map(|n| n.as_bytes()) {
        if let Some(k) = (retain + 1..nb.len().min(buf.len() + 1))
            .rev()
            .find(|&k| buf.ends_with(&nb[..k]))
        {
            retain = k;
        }
    }
    let low: Vec<u8> = buf[buf.len().saturating_sub(MAX_SHAPE_RETAIN_BYTES)..]
        .iter()
        .map(u8::to_ascii_lowercase)
        .collect();
    for t in "bearer |bearer\t|sk-|sk_|rk_|ghp_|gho_|ghu_|ghs_|ghr_|akia|ohc_surr_|-----begin"
        .split('|')
        .map(str::as_bytes)
    {
        if let Some(k) = (1..=t.len().min(low.len()))
            .rev()
            .find(|&k| low.ends_with(&t[..k]))
        {
            retain = retain.max(k);
        }
        if let Some(idx) = low.windows(t.len()).rposition(|w| w == t) {
            let r = &low[idx + t.len()..];
            let active = (t == b"-----begin" && !r.windows(16).any(|w| w == b"private key-----"))
                || (t != b"-----begin"
                    && r.iter()
                        .all(|&b| b.is_ascii_alphanumeric() || b"._~+/=-".contains(&b)));
            if active {
                retain = retain.max(low.len() - idx);
            }
        }
    }
    retain.min(buf.len())
}

fn scan_text_with_needles(input: &str, needles: &[Zeroizing<String>]) -> LeakScanReport {
    static PATS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    let pats = PATS.get_or_init(|| {
        [
            (r"(?i)\bBearer[ \t]+[A-Za-z0-9._~+/=-]{12,}", "Bearer [REDACTED]"),
            (r"\b(?:sk-(?:proj-|ant-|live-|test-)?[A-Za-z0-9_-]{16,}|(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{12,})", REDACTED_MARKER),
            (r"\b(?:(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|ohc_surr_[0-9a-fA-F]{16,})", REDACTED_MARKER),
            (r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z0-9 ]*PRIVATE KEY-----", REDACTED_MARKER),
        ].into_iter().map(|(p, r)| (Regex::new(p).expect("re"), r)).collect()
    });
    let (mut text, mut leaks) = (input.to_owned(), 0usize);
    for s in needles.iter().map(|n| n.as_str()).filter(|s| !s.is_empty()) {
        let c = text.matches(s).count();
        if c > 0 {
            leaks += c;
            text = text.replace(s, REDACTED_MARKER);
        }
    }
    for (re, rep) in pats {
        let c = re.find_iter(&text).count();
        if c > 0 {
            leaks += c;
            text = re.replace_all(&text, *rep).into_owned();
        }
    }
    LeakScanReport {
        sanitized: text,
        leaks_detected: leaks,
    }
}

fn base64_std(input: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for c in input.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        let ch = |sh: u32| A[((n >> sh) & 63u32) as usize] as char;
        out.extend([
            ch(18),
            ch(12),
            if c.len() > 1 { ch(6) } else { '=' },
            if c.len() > 2 { ch(0) } else { '=' },
        ]);
    }
    out
}

fn percent_encode(input: &[u8], enc_punct: bool) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input {
        if b.is_ascii_alphanumeric() || (!enc_punct && b"-_.~".contains(&b)) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// --- 4. WasmToolBudget & Sentinel aggregate ---

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WasmToolBudget {
    pub max_memory_bytes: u64,
    pub max_fuel: u64,
    pub max_wall_ms: u64,
    #[serde(default)]
    pub allow_network: bool,
}

impl Default for WasmToolBudget {
    fn default() -> Self {
        Self {
            max_memory_bytes: 64 * 1024 * 1024,
            max_fuel: 10_000_000,
            max_wall_ms: 10_000,
            allow_network: false,
        }
    }
}

impl WasmToolBudget {
    pub fn validate(&self) -> Result<(), SentinelError> {
        let ok = (1..=MAX_WASM_MEMORY_BYTES).contains(&self.max_memory_bytes)
            && self.max_fuel > 0
            && (1..=MAX_WASM_WALL_MS).contains(&self.max_wall_ms);
        ok.then_some(())
            .ok_or_else(|| SentinelError::InvalidInput("WASM budget out of range".into()))
    }
    pub fn max_wall_duration(&self) -> Duration {
        Duration::from_millis(self.max_wall_ms)
    }
    pub fn enforce_usage(&self, mem: u64, fuel: u64, wall_ms: u64) -> Result<(), SentinelError> {
        if mem > self.max_memory_bytes || fuel > self.max_fuel || wall_ms > self.max_wall_ms {
            return Err(SentinelError::BudgetExceeded(format!(
                "mem={mem},fuel={fuel},wall_ms={wall_ms}"
            )));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Sentinel {
    pub broker: SurrogateTokenBroker,
    pub ssrf_guard: SsrfGuard,
    pub leak_scanner: ResponseLeakScanner,
    pub wasm_budget: WasmToolBudget,
}

impl Sentinel {
    pub fn new(
        broker: SurrogateTokenBroker,
        ssrf_guard: SsrfGuard,
        leak_scanner: ResponseLeakScanner,
    ) -> Self {
        Self {
            broker,
            ssrf_guard,
            leak_scanner,
            wasm_budget: WasmToolBudget::default(),
        }
    }
    pub fn with_wasm_budget(mut self, budget: WasmToolBudget) -> Self {
        self.wasm_budget = budget;
        self
    }
}
