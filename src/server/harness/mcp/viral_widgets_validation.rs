//! Fail-closed validation, JSON Schema generation, and CSP-safe embed snippet
//! builders for OneHumanCorp viral growth widgets.

use super::catalog::{EmbedKind, ParamRule, ParamSpec, WidgetSpec};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

pub const RECOMMENDED_HTML_EMBED_CSP: &str = "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; img-src https: data:; font-src 'self'; base-uri 'none'; form-action 'none'";
pub const RECOMMENDED_SVG_EMBED_CSP: &str =
    "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; base-uri 'none'";

pub(crate) fn validate_tenant_id(
    raw: &str,
    verified_tenant: Option<&str>,
) -> Result<String, (i32, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > 64
        || trimmed != raw
        || trimmed.eq_ignore_ascii_case("system")
        || trimmed.eq_ignore_ascii_case("default")
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err((
            -32602,
            "Invalid tenant_id: must be 1..=64 ASCII [a-zA-Z0-9_-] and not 'system' or 'default'"
                .to_string(),
        ));
    }
    if let Some(expected) = verified_tenant
        && trimmed != expected
    {
        return Err((
            -32602,
            "tenant_id does not match the authenticated tenant context".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

pub(crate) fn validate_plain_text(
    field: &str,
    raw: &str,
    max_len: usize,
) -> Result<String, (i32, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err((-32602, format!("'{field}' must not be empty")));
    }
    if trimmed.chars().count() > max_len {
        return Err((
            -32602,
            format!("'{field}' exceeds maximum length of {max_len} characters"),
        ));
    }
    if trimmed
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | '`'))
        || trimmed.contains("${")
    {
        return Err((
            -32602,
            format!("'{field}' contains forbidden markup, script, or control characters"),
        ));
    }
    let lower = trimmed.to_ascii_lowercase();
    for forbidden in [
        "javascript:",
        "vbscript:",
        "data:",
        "expression(",
        "onerror=",
        "onload=",
        "onclick=",
        "onmouseover=",
    ] {
        if lower.contains(forbidden) {
            return Err((
                -32602,
                format!("'{field}' contains a forbidden script or event-handler sequence"),
            ));
        }
    }
    Ok(trimmed.to_string())
}

fn validate_hex_color(field: &str, raw: &str) -> Result<String, (i32, String)> {
    let trimmed = raw.trim();
    let hex = trimmed.strip_prefix('#').ok_or_else(|| {
        (
            -32602,
            format!("'{field}' must be a hex color like #2563eb"),
        )
    })?;
    if !matches!(hex.len(), 3 | 6) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err((-32602, format!("'{field}' must match #RGB or #RRGGBB")));
    }
    if hex.len() == 3 {
        let mut expanded = String::with_capacity(7);
        expanded.push('#');
        for ch in hex.chars() {
            let lower = ch.to_ascii_lowercase();
            expanded.push(lower);
            expanded.push(lower);
        }
        Ok(expanded)
    } else {
        Ok(format!("#{}", hex.to_ascii_lowercase()))
    }
}

fn is_non_public_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    let is_cgnat = octets[0] == 100 && (octets[1] & 0b1100_0000) == 64;
    let is_benchmarking = octets[0] == 198 && (octets[1] & 0b1111_1110) == 18;
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || is_cgnat
        || is_benchmarking
        || octets[0] == 0
        || octets[0] >= 240
}

fn is_non_public_ipv6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
    {
        return true;
    }
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_non_public_ipv4(v4);
    }
    let segs = ip.segments();
    // IPv4-compatible deprecated ::/96
    if segs[0..6] == [0, 0, 0, 0, 0, 0] {
        return true;
    }
    // NAT64 well-known prefix 64:ff9b::/96, 6to4 2002::/16, Teredo 2001::/32, Doc 2001:db8::/32
    let is_nat64 = segs[0] == 0x0064 && segs[1] == 0xff9b && segs[2..6] == [0, 0, 0, 0];
    let is_6to4 = segs[0] == 0x2002;
    let is_teredo = segs[0] == 0x2001 && segs[1] == 0x0000;
    let is_doc = segs[0] == 0x2001 && segs[1] == 0x0db8;
    is_nat64 || is_6to4 || is_teredo || is_doc
}

fn validate_https_url(field: &str, raw: &str, max_len: usize) -> Result<String, (i32, String)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > max_len {
        return Err((-32602, format!("'{field}' must be a bounded HTTPS URL")));
    }
    let parsed = reqwest::Url::parse(trimmed)
        .map_err(|_| (-32602, format!("'{field}' must be a valid HTTPS URL")))?;
    if parsed.scheme() != "https" || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err((
            -32602,
            format!("'{field}' must use https:// without credentials"),
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| (-32602, format!("'{field}' must include a public host")))?
        .to_ascii_lowercase();
    let bare_host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(&host);

    if let Ok(ip) = bare_host.parse::<IpAddr>() {
        let blocked = match ip {
            IpAddr::V4(v4) => is_non_public_ipv4(v4),
            IpAddr::V6(v6) => is_non_public_ipv6(v6),
        };
        if blocked {
            return Err((-32602, format!("'{field}' must target a public HTTPS host")));
        }
        return Ok(parsed.to_string());
    }

    if bare_host == "localhost"
        || bare_host.ends_with(".localhost")
        || bare_host.ends_with(".local")
        || bare_host.ends_with(".internal")
        || !bare_host.contains('.')
    {
        return Err((-32602, format!("'{field}' must target a public HTTPS host")));
    }
    Ok(parsed.to_string())
}

pub(crate) fn validate_widget_params(
    spec: &WidgetSpec,
    raw: &Map<String, Value>,
) -> Result<Map<String, Value>, (i32, String)> {
    for key in raw.keys() {
        if !spec.params.iter().any(|p| p.name == key) {
            return Err((
                -32602,
                format!("Unknown parameter '{key}' for widget '{}'", spec.id),
            ));
        }
    }

    let mut validated = Map::new();
    for param in &spec.params {
        let Some(value) = raw.get(param.name) else {
            if param.required {
                return Err((
                    -32602,
                    format!("Missing required parameter '{}'", param.name),
                ));
            }
            validated.insert(param.name.to_string(), param.default.clone());
            continue;
        };

        let normalized = match param.rule {
            ParamRule::Text { max_len } => {
                let s = value
                    .as_str()
                    .ok_or_else(|| (-32602, format!("'{}' must be a string", param.name)))?;
                Value::String(validate_plain_text(param.name, s, max_len)?)
            }
            ParamRule::Code { max_len } => {
                let s = value
                    .as_str()
                    .ok_or_else(|| (-32602, format!("'{}' must be a string", param.name)))?
                    .trim();
                if s.is_empty()
                    || s.len() > max_len
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err((
                        -32602,
                        format!("'{}' must be 1..={max_len} ASCII [A-Za-z0-9_-]", param.name),
                    ));
                }
                Value::String(s.to_string())
            }
            ParamRule::HexColor => {
                let s = value.as_str().ok_or_else(|| {
                    (
                        -32602,
                        format!("'{}' must be a hex color string", param.name),
                    )
                })?;
                Value::String(validate_hex_color(param.name, s)?)
            }
            ParamRule::HttpsUrl { max_len } => {
                let s = value.as_str().ok_or_else(|| {
                    (
                        -32602,
                        format!("'{}' must be an HTTPS URL string", param.name),
                    )
                })?;
                Value::String(validate_https_url(param.name, s, max_len)?)
            }
            ParamRule::Integer { min, max } => {
                let n = value
                    .as_i64()
                    .ok_or_else(|| (-32602, format!("'{}' must be an integer", param.name)))?;
                if n < min || n > max {
                    return Err((
                        -32602,
                        format!("'{}' must be between {min} and {max}", param.name),
                    ));
                }
                Value::Number(n.into())
            }
            ParamRule::Boolean => {
                let b = value
                    .as_bool()
                    .ok_or_else(|| (-32602, format!("'{}' must be a boolean", param.name)))?;
                Value::Bool(b)
            }
            ParamRule::Enum(allowed) => {
                let s = value
                    .as_str()
                    .ok_or_else(|| (-32602, format!("'{}' must be a string", param.name)))?;
                if !allowed.contains(&s) {
                    return Err((
                        -32602,
                        format!("'{}' must be one of: {}", param.name, allowed.join(", ")),
                    ));
                }
                Value::String(s.to_string())
            }
        };
        validated.insert(param.name.to_string(), normalized);
    }

    Ok(validated)
}

pub(crate) fn param_to_json_schema(param: &ParamSpec) -> Value {
    let mut schema = match param.rule {
        ParamRule::Text { max_len } => {
            json!({ "type": "string", "minLength": 1, "maxLength": max_len })
        }
        ParamRule::Code { max_len } => {
            json!({ "type": "string", "minLength": 1, "maxLength": max_len, "pattern": "^[A-Za-z0-9_-]+$" })
        }
        ParamRule::HexColor => {
            json!({ "type": "string", "pattern": "^#([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$" })
        }
        ParamRule::HttpsUrl { max_len } => {
            json!({ "type": "string", "format": "uri", "maxLength": max_len, "pattern": "^https://" })
        }
        ParamRule::Integer { min, max } => {
            json!({ "type": "integer", "minimum": min, "maximum": max })
        }
        ParamRule::Boolean => json!({ "type": "boolean" }),
        ParamRule::Enum(values) => json!({ "type": "string", "enum": values }),
    };
    if let Some(obj) = schema.as_object_mut() {
        obj.insert("description".to_string(), json!(param.description));
        obj.insert("default".to_string(), param.default.clone());
    }
    schema
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{:02X}", byte));
        }
    }
    out
}

fn escape_html_attr(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

fn value_to_query_string(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

pub(crate) fn build_preview_url(
    origin: &str,
    tenant_id: &str,
    spec: &WidgetSpec,
    validated: &Map<String, Value>,
) -> String {
    let mut pairs = vec![format!("tenant={}", percent_encode(tenant_id))];
    for param in &spec.params {
        if let Some(val) = validated.get(param.name) {
            let encoded = percent_encode(&value_to_query_string(val));
            pairs.push(format!("{}={}", param.query_key, encoded));
        }
    }
    format!("{}{}?{}", origin, spec.embed_endpoint, pairs.join("&"))
}

pub(crate) fn build_embed_snippet(
    spec: &WidgetSpec,
    preview_url: &str,
    width: u16,
    height: u16,
    title: &str,
) -> String {
    let safe_url = escape_html_attr(preview_url);
    let safe_title = escape_html_attr(title);
    match spec.embed_kind {
        EmbedKind::Script => format!(
            "<script src=\"{safe_url}\" defer referrerpolicy=\"strict-origin-when-cross-origin\"></script>"
        ),
        EmbedKind::Image => format!(
            "<img src=\"{safe_url}\" alt=\"{safe_title}\" width=\"{width}\" height=\"{height}\" loading=\"lazy\" referrerpolicy=\"strict-origin-when-cross-origin\" />"
        ),
        EmbedKind::Iframe => format!(
            "<iframe src=\"{safe_url}\" title=\"{safe_title}\" width=\"{width}\" height=\"{height}\" loading=\"lazy\" referrerpolicy=\"strict-origin-when-cross-origin\" sandbox=\"allow-forms allow-popups allow-popups-to-escape-sandbox\" style=\"border:0;border-radius:16px;max-width:100%;display:block;\"></iframe>"
        ),
    }
}

pub(crate) fn csp_for_embed_kind(kind: EmbedKind) -> &'static str {
    match kind {
        EmbedKind::Image => RECOMMENDED_SVG_EMBED_CSP,
        EmbedKind::Iframe | EmbedKind::Script => RECOMMENDED_HTML_EMBED_CSP,
    }
}

pub(crate) fn compute_config_id(
    tenant_id: &str,
    widget_id: &str,
    validated: &Map<String, Value>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tenant_id.as_bytes());
    hasher.update(b":");
    hasher.update(widget_id.as_bytes());
    hasher.update(b":");
    hasher.update(
        serde_json::to_string(validated)
            .unwrap_or_default()
            .as_bytes(),
    );
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("vwcfg_{hex}")
}
