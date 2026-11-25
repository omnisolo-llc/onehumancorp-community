use super::*;
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;

fn server_for_tests() -> ViralWidgetsMcpServer {
    let config = ViralWidgetsServerConfig::new("https://app.onehumancorp.example").unwrap();
    ViralWidgetsMcpServer::new(config)
}

#[test]
fn config_rejects_insecure_or_malformed_origins() {
    assert!(ViralWidgetsServerConfig::new("http://app.onehumancorp.example").is_err());
    assert!(ViralWidgetsServerConfig::new("http://omnisolo.attacker.example").is_err());
    assert!(ViralWidgetsServerConfig::new("https://user:pass@app.example.com").is_err());
    assert!(ViralWidgetsServerConfig::new("https://app.example.com/path").is_err());
    assert!(ViralWidgetsServerConfig::new("https://app.example.com?a=1").is_err());
    assert!(ViralWidgetsServerConfig::new("javascript:alert(1)").is_err());

    assert!(ViralWidgetsServerConfig::new("https://app.example.com").is_ok());
    assert!(ViralWidgetsServerConfig::new("http://127.0.0.1:3000").is_ok());
    assert!(ViralWidgetsServerConfig::new("http://localhost:3000").is_ok());
}

#[tokio::test]
async fn json_rpc_initialize_and_list_tools() {
    let server = server_for_tests();

    let init = server
        .handle_json_rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    assert_eq!(init["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
    assert_eq!(
        init["result"]["serverInfo"]["name"],
        "ohc-viral-widgets-mcp"
    );

    let list = server
        .handle_json_rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#)
        .await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec![
            "viral_widgets.list",
            "viral_widgets.get_schema",
            "viral_widgets.configure",
            "viral_widgets.generate_embed",
        ]
    );
}

#[tokio::test]
async fn list_and_get_schema_cover_all_catalog_widgets() {
    let server = server_for_tests();

    let all = server
        .call_tool("viral_widgets.list", json!({}), None)
        .await
        .unwrap();
    let widgets = all["widgets"].as_array().unwrap();
    assert!(widgets.len() >= 14);
    assert!(widgets.iter().any(|w| w["id"] == "referral_fab"));
    assert!(widgets.iter().any(|w| w["id"] == "secret_menu"));
    assert!(widgets.iter().any(|w| w["id"] == "discount_code"));
    assert!(widgets.iter().any(|w| w["id"] == "milestone_card"));
    assert!(widgets.iter().any(|w| w["id"] == "booking_embed"));

    let referral_only = server
        .call_tool(
            "viral_widgets.list",
            json!({ "category": "referral" }),
            None,
        )
        .await
        .unwrap();
    assert!(
        referral_only["widgets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["category"] == "referral")
    );

    let schema = server
        .call_tool(
            "viral_widgets.get_schema",
            json!({ "widget_id": "discount_code" }),
            None,
        )
        .await
        .unwrap();
    assert_eq!(schema["schema"]["additionalProperties"], json!(false));
    assert_eq!(
        schema["schema"]["properties"]["discount_percent"]["maximum"],
        json!(90)
    );
    assert!(schema["presets"].as_array().unwrap().len() >= 2);

    let unknown = server
        .call_tool(
            "viral_widgets.get_schema",
            json!({ "widget_id": "nonexistent" }),
            None,
        )
        .await;
    assert!(unknown.is_err());
}

#[tokio::test]
async fn configure_persists_to_real_sqlite_and_enforces_tenant_isolation() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    let config = ViralWidgetsServerConfig::new("https://app.onehumancorp.example").unwrap();
    let server = ViralWidgetsMcpServer::with_sqlite(config, pool)
        .await
        .unwrap();

    // Default configure without owner_confirmed stays in draft state.
    let draft = server
        .call_tool(
            "viral_widgets.configure",
            json!({
                "tenant_id": "tenant-alpha",
                "widget_id": "referral_fab",
                "params": {
                    "reward": "$15 off",
                    "theme_color": "#0af",
                    "remove_branding": false
                }
            }),
            Some("tenant-alpha"),
        )
        .await
        .unwrap();
    assert_eq!(draft["status"], "draft_awaiting_owner_confirmation");
    assert_eq!(draft["requires_owner_confirmation"], true);
    assert_eq!(draft["validated_params"]["theme_color"], "#00aaff");
    assert!(draft["embed_html"].as_str().unwrap().starts_with(
        "<script src=\"https://app.onehumancorp.example/api/v1/growth/referral-fab/embed?"
    ));
    assert!(draft["embed_html"].as_str().unwrap().contains("defer"));

    // Owner confirmation transitions persisted config to ready-to-embed.
    let confirmed = server
        .call_tool(
            "viral_widgets.configure",
            json!({
                "tenant_id": "tenant-alpha",
                "widget_id": "referral_fab",
                "owner_confirmed": true,
                "params": {
                    "reward": "$15 off",
                    "theme_color": "#2563EB"
                }
            }),
            Some("tenant-alpha"),
        )
        .await
        .unwrap();
    assert_eq!(confirmed["status"], "configured_ready_to_embed");
    assert_eq!(confirmed["requires_owner_confirmation"], false);

    // generate_embed reads persisted SQLite config for tenant-alpha.
    let generated = server
        .call_tool(
            "viral_widgets.generate_embed",
            json!({
                "tenant_id": "tenant-alpha",
                "widget_id": "referral_fab"
            }),
            Some("tenant-alpha"),
        )
        .await
        .unwrap();
    assert_eq!(generated["from_saved_config"], true);
    assert_eq!(generated["config_status"], "configured_ready_to_embed");
    assert!(
        generated["preview_url"]
            .as_str()
            .unwrap()
            .contains("themeColor=%232563eb")
    );

    // Tenant beta cannot read tenant alpha's saved config or spoof tenant-alpha.
    let cross_tenant_spoof = server
        .call_tool(
            "viral_widgets.generate_embed",
            json!({ "tenant_id": "tenant-alpha", "widget_id": "referral_fab" }),
            Some("tenant-beta"),
        )
        .await;
    assert!(cross_tenant_spoof.is_err());

    let missing_for_beta = server
        .call_tool(
            "viral_widgets.generate_embed",
            json!({ "tenant_id": "tenant-beta", "widget_id": "referral_fab" }),
            Some("tenant-beta"),
        )
        .await;
    assert!(missing_for_beta.is_err());
}

#[tokio::test]
async fn rejects_xss_payloads_invalid_colors_out_of_range_and_bad_tenants() {
    let server = server_for_tests();

    // XSS script / template injection payloads must fail closed.
    for payload in [
        "<script>alert(1)</script>",
        "${alert(1)}",
        "`+alert(1)+`",
        "javascript:alert(1)",
        "hello onerror=alert(1)",
        "line1\nline2",
    ] {
        let res = server
            .call_tool(
                "viral_widgets.configure",
                json!({
                    "tenant_id": "tenant-1",
                    "widget_id": "referral_fab",
                    "params": { "reward": payload }
                }),
                Some("tenant-1"),
            )
            .await;
        assert!(
            res.is_err(),
            "expected XSS payload '{payload}' to be rejected"
        );
    }

    // Invalid hex color rejected.
    assert!(
        server
            .call_tool(
                "viral_widgets.configure",
                json!({
                    "tenant_id": "tenant-1",
                    "widget_id": "referral_fab",
                    "params": { "reward": "$10", "theme_color": "red; background:url(javascript:alert(1))" }
                }),
                Some("tenant-1"),
            )
            .await
            .is_err()
    );

    // Out-of-range discount rejected.
    assert!(
        server
            .call_tool(
                "viral_widgets.configure",
                json!({
                    "tenant_id": "tenant-1",
                    "widget_id": "discount_code",
                    "params": { "discount_percent": 99, "code": "FREE99" }
                }),
                Some("tenant-1"),
            )
            .await
            .is_err()
    );

    // Invalid coupon code with HTML/SQL characters rejected.
    assert!(
        server
            .call_tool(
                "viral_widgets.configure",
                json!({
                    "tenant_id": "tenant-1",
                    "widget_id": "discount_code",
                    "params": { "discount_percent": 15, "code": "SAVE\"<script>" }
                }),
                Some("tenant-1"),
            )
            .await
            .is_err()
    );

    // SSRF / non-HTTPS / private & IPv6-mapped IP image URLs rejected on before_after_slider.
    for blocked_url in [
        "http://169.254.169.254/latest/meta-data",
        "https://172.16.0.1/before.jpg",
        "https://100.64.0.1/before.jpg",
        "https://[::ffff:169.254.169.254]/latest/meta-data",
        "https://[::ffff:127.0.0.1]/secret",
        "https://[fc00::1]/before.jpg",
        "https://[fe80::1]/before.jpg",
        "https://[::]/before.jpg",
        "https://backend/internal.jpg",
    ] {
        assert!(
            server
                .call_tool(
                    "viral_widgets.configure",
                    json!({
                        "tenant_id": "tenant-1",
                        "widget_id": "before_after_slider",
                        "params": {
                            "title": "Test",
                            "before_image_url": blocked_url,
                            "after_image_url": "https://cdn.example.com/after.jpg"
                        }
                    }),
                    Some("tenant-1"),
                )
                .await
                .is_err(),
            "expected SSRF / non-public URL '{blocked_url}' to be rejected"
        );
    }

    // Missing / system / default / malformed tenant IDs rejected.
    for bad_tenant in ["", "   ", "system", "SYSTEM", "default", "../other", "a b"] {
        assert!(
            server
                .call_tool(
                    "viral_widgets.configure",
                    json!({
                        "tenant_id": bad_tenant,
                        "widget_id": "booking_embed",
                        "params": { "service": "Consult" }
                    }),
                    None,
                )
                .await
                .is_err(),
            "expected bad tenant '{bad_tenant}' to be rejected"
        );
    }

    // Unknown extra parameter rejected (additionalProperties: false).
    assert!(
        server
            .call_tool(
                "viral_widgets.configure",
                json!({
                    "tenant_id": "tenant-1",
                    "widget_id": "booking_embed",
                    "params": { "service": "Consult", "injected_js": "alert(1)" }
                }),
                Some("tenant-1"),
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn generate_embed_via_json_rpc_escapes_and_sandboxes_iframe() {
    let server = server_for_tests();
    let rpc = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "viral_widgets.generate_embed",
            "arguments": {
                "tenant_id": "acme-bakery",
                "widget_id": "secret_menu",
                "width": 420,
                "height": 560,
                "title": "Acme's Secret Menu & Rewards",
                "params": {
                    "item_name": "Matcha Croissant & Tart",
                    "item_desc": "Unlock with 3 friend shares",
                    "access_code": "SECRET3",
                    "shares_required": 3
                }
            }
        }
    });

    let resp = server
        .handle_json_rpc_for_tenant(&rpc.to_string(), Some("acme-bakery"))
        .await;
    assert_eq!(resp["result"]["isError"], false);
    let structured = &resp["result"]["structuredContent"];
    let html = structured["embed_html"].as_str().unwrap();
    assert!(html.contains("<iframe "));
    assert!(html.contains("sandbox=\"allow-forms allow-popups allow-popups-to-escape-sandbox\""));
    assert!(html.contains("title=\"Acme&#x27;s Secret Menu &amp; Rewards\""));
    assert!(html.contains("item_name=Matcha%20Croissant%20%26%20Tart"));
    assert_eq!(structured["recommended_csp"], RECOMMENDED_HTML_EMBED_CSP);
}

#[test]
fn canonical_viral_widget_skills_exist_and_reference_mcp_tools() {
    let skills_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../agents/builtin/skills/viral_widgets");
    let manifest_raw = std::fs::read_to_string(skills_root.join("manifest.json"))
        .expect("viral_widgets manifest.json must exist");
    let manifest: Value = serde_json::from_str(&manifest_raw).expect("valid manifest.json");
    let required_tools = manifest["required_mcp_tools"].as_array().unwrap();
    assert_eq!(required_tools.len(), 4);

    for skill in manifest["skills"].as_array().unwrap() {
        let file = skill["file"].as_str().unwrap();
        let body = std::fs::read_to_string(skills_root.join(file))
            .unwrap_or_else(|e| panic!("failed to read skill {file}: {e}"));
        assert!(body.starts_with("# "), "{file} must start with '# ' title");
        assert!(
            body.lines()
                .any(|l| l.to_ascii_lowercase().starts_with("keywords:")),
            "{file} must declare ProgressiveSkillManager Keywords:"
        );
        assert!(
            body.contains("viral_widgets.configure") && body.contains("owner_confirmed"),
            "{file} must instruct draft-first viral_widgets.configure with owner_confirmed"
        );
    }
}
