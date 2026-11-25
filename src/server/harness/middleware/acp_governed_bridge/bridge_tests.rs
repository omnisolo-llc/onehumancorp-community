use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;

use super::*;
use crate::middleware::acp::ACP_PROTOCOL_VERSION;
use crate::middleware::harness::HarnessAdapterError;
use crate::middleware::usage_ledger::{PayerMode, RateCard, UsageLedger, UsageScope};

const ISOLATED_ENV_TEST_MARKER: &str = "OHC_ACP_ISOLATED_ENV_TEST";
const ISOLATED_ENV_SUCCESS_EXIT: i32 = 87;

struct TempWorkspace {
    root: PathBuf,
}

impl TempWorkspace {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!("ohc-acp-bridge-{label}-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("create temp ACP workspace");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn write_script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, body).expect("write ACP test harness script");
        path
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

async fn sqlite_ledger_with_limit(tenant: &str, limit_micros: i64) -> UsageLedger {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("open in-memory SQLite ledger");
    let ledger = UsageLedger::Sqlite(pool);
    ledger.initialize().await.expect("initialize usage ledger");
    ledger
        .set_limit(tenant, limit_micros)
        .await
        .expect("set tenant micro-USD limit");
    ledger
}

fn managed_scope(tenant: &str) -> UsageScope {
    UsageScope {
        tenant_id: tenant.to_owned(),
        task_id: "task-acp-1".to_owned(),
        attempt_id: "attempt-acp-1".to_owned(),
        provider: "sentinel-proxy".to_owned(),
        model: "acp-routed".to_owned(),
        payer: PayerMode::ManagedApi,
        rate_card: Some(RateCard {
            revision: "acp-rate-v1".to_owned(),
            input_micros_per_million: 1_000_000,
            output_micros_per_million: 2_000_000,
            cached_input_micros_per_million: 100_000,
        }),
    }
}

fn node_harness_spec(script_path: &Path, with_mcp_bridge: bool) -> AcpExternalHarnessSpec {
    let mut spec = AcpExternalHarnessSpec::new(
        "local-node-acp",
        "node",
        vec![script_path.to_string_lossy().into_owned()],
        [
            AcpSandboxTier::Tier2LocalOs,
            AcpSandboxTier::Tier3CloudMicroVm,
        ],
    );
    if with_mcp_bridge {
        spec = spec.with_mcp_bridge(AcpMcpBridgeCommand::ohc_stdio_bridge(
            "node",
            "scripts/ohc-mcp-stdio.mjs",
        ));
    }
    spec
}

const REAL_ACP_RESPONDER_JS: &str = r#"
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
const mode = process.argv[2] || 'lifecycle';
let activeSessionId = 'acp-sess-1';
let pendingPromptId = null;
let lastNewSessionParams = null;
let permissionPhase = 0;
let permissionResults = [];

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

function sendUpdate(update) {
  send({
    jsonrpc: '2.0',
    method: 'session/update',
    params: { sessionId: activeSessionId, update },
  });
}

function sendNextPermission() {
  permissionPhase += 1;
  if (permissionPhase === 1) {
    send({
      jsonrpc: '2.0',
      id: 'perm-read-1',
      method: 'session/request_permission',
      params: {
        sessionId: activeSessionId,
        toolCall: {
          toolCallId: 'tc-read-1',
          toolName: 'read_file',
          title: 'Read README.md',
          kind: 'read',
          estimatedCostMicros: 200,
          rawInput: { path: 'README.md' },
        },
        options: [
          { optionId: 'allow_once', name: 'Allow once', kind: 'allow_once' },
          { optionId: 'reject_once', name: 'Reject once', kind: 'reject_once' },
        ],
      },
    });
  } else if (permissionPhase === 2) {
    send({
      jsonrpc: '2.0',
      id: 'perm-email-2',
      method: 'session/request_permission',
      params: {
        sessionId: activeSessionId,
        toolCall: {
          toolCallId: 'tc-email-2',
          toolName: 'send_email',
          title: 'Send customer email',
          kind: 'other',
          rawInput: { to: 'owner@example.com', subject: 'Invoice', body: 'Hello' },
        },
        options: [
          { optionId: 'allow_once', name: 'Allow once', kind: 'allow_once' },
          { optionId: 'reject_once', name: 'Reject once', kind: 'reject_once' },
        ],
      },
    });
  } else if (permissionPhase === 3) {
    send({
      jsonrpc: '2.0',
      id: 'perm-finance-3',
      method: 'session/request_permission',
      params: {
        sessionId: activeSessionId,
        toolCall: {
          toolCallId: 'tc-finance-3',
          toolName: 'financial_commit',
          title: 'Commit vendor payment',
          kind: 'other',
          estimatedCostMicros: 500,
          rawInput: { invoice_id: 'inv-42', amount_micros: 500 },
        },
        options: [
          { optionId: 'allow_once', name: 'Allow once', kind: 'allow_once' },
          { optionId: 'reject_once', name: 'Reject once', kind: 'reject_once' },
        ],
      },
    });
  } else if (permissionPhase === 4) {
    send({
      jsonrpc: '2.0',
      id: 'perm-budget-4',
      method: 'session/request_permission',
      params: {
        sessionId: activeSessionId,
        toolCall: {
          toolCallId: 'tc-budget-4',
          toolName: 'search_crm',
          title: 'Expensive search exceeding remaining ledger budget',
          kind: 'search',
          estimatedCostMicros: 900,
          rawInput: { query: 'all-accounts' },
        },
        options: [
          { optionId: 'allow_once', name: 'Allow once', kind: 'allow_once' },
          { optionId: 'reject_once', name: 'Reject once', kind: 'reject_once' },
        ],
      },
    });
  } else if (permissionPhase === 5) {
    send({
      jsonrpc: '2.0',
      id: 'disallowed-fs-5',
      method: 'fs/write_text_file',
      params: { path: '/etc/passwd', content: 'denied' },
    });
  } else {
    sendUpdate({
      sessionUpdate: 'agent_message_chunk',
      content: { type: 'text', text: JSON.stringify(permissionResults) },
    });
    send({
      jsonrpc: '2.0',
      id: pendingPromptId,
      result: { stopReason: 'end_turn', usage: { inputTokens: 25, outputTokens: 15 } },
    });
    pendingPromptId = null;
  }
}

rl.on('line', (line) => {
  if (!line.trim()) return;
  const msg = JSON.parse(line);

  // Response from OHC to our bidirectional server-to-client request
  if (!msg.method && msg.id !== undefined) {
    permissionResults.push({
      id: msg.id,
      result: msg.result || null,
      error: msg.error || null,
    });
    if (mode === 'permissions') {
      sendNextPermission();
    }
    return;
  }

  if (msg.method === 'initialize') {
    send({
      jsonrpc: '2.0',
      id: msg.id,
      result: {
        protocolVersion: 1,
        agentCapabilities: { loadSession: false },
        agentInfo: { name: 'local-node-acp', version: '1.0.0' },
      },
    });
  } else if (msg.method === 'session/new') {
    lastNewSessionParams = msg.params;
    send({
      jsonrpc: '2.0',
      id: msg.id,
      result: { sessionId: activeSessionId },
    });
  } else if (msg.method === 'session/prompt') {
    pendingPromptId = msg.id;
    if (mode === 'env_audit') {
      fs.writeFileSync(
        path.join(process.cwd(), 'env-audit.json'),
        JSON.stringify(process.env),
        'utf8',
      );
      sendUpdate({
        sessionUpdate: 'agent_message_chunk',
        content: { type: 'text', text: 'env-audit-recorded' },
      });
      send({
        jsonrpc: '2.0',
        id: msg.id,
        result: { stopReason: 'end_turn' },
      });
      pendingPromptId = null;
    } else if (mode === 'lifecycle') {
      sendUpdate({
        sessionUpdate: 'agent_thought_chunk',
        content: { type: 'text', text: 'Checking MCP server wiring.' },
      });
      sendUpdate({
        sessionUpdate: 'agent_message_chunk',
        content: { type: 'text', text: JSON.stringify(lastNewSessionParams) },
      });
      sendUpdate({
        sessionUpdate: 'agent_message_chunk',
        content: { type: 'text', text: '::OK' },
      });
      sendUpdate({
        sessionUpdate: 'usage_update',
        used: 120,
        size: 8192,
      });
      send({
        jsonrpc: '2.0',
        id: msg.id,
        result: { stopReason: 'end_turn', usage: { inputTokens: 80, outputTokens: 40 } },
      });
      pendingPromptId = null;
    } else if (mode === 'permissions') {
      sendNextPermission();
    } else if (mode === 'cancel_graceful') {
      sendUpdate({
        sessionUpdate: 'agent_message_chunk',
        content: { type: 'text', text: 'working...' },
      });
      // Stay open until session/cancel arrives.
    } else if (mode === 'cancel_hang') {
      sendUpdate({
        sessionUpdate: 'agent_message_chunk',
        content: { type: 'text', text: 'stuck...' },
      });
      setInterval(() => {}, 10_000);
    } else if (mode === 'oversized') {
      process.stdout.write('{"jsonrpc":"2.0","method":"session/update","params":{"blob":"' + 'A'.repeat(270000) + '"}}\n');
    }
  } else if (msg.method === 'session/cancel') {
    if (mode === 'cancel_graceful' && pendingPromptId !== null) {
      send({
        jsonrpc: '2.0',
        id: pendingPromptId,
        result: { stopReason: 'cancelled' },
      });
      pendingPromptId = null;
    }
  }
});
"#;

#[test]
fn host_secrets_stripped_and_only_surrogate_and_proxy_reach_child_process() {
    let test_name = "middleware::acp_governed_bridge::bridge_tests::host_secrets_stripped_and_only_surrogate_and_proxy_reach_child_process";
    if std::env::var(ISOLATED_ENV_TEST_MARKER).as_deref() == Ok(test_name) {
        assert_eq!(
            std::env::var("OPENAI_API_KEY").as_deref(),
            Ok("sk-live-host-openai-canary-9999")
        );
        assert_eq!(
            std::env::var("STRIPE_SECRET_KEY").as_deref(),
            Ok("sk_live_host_stripe_canary_8888")
        );
        assert_eq!(
            std::env::var("ANTHROPIC_API_KEY").as_deref(),
            Ok("sk-ant-host-anthropic-canary-7777")
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build isolated runtime");
        rt.block_on(async {
            let workspace = TempWorkspace::new("env");
            let script = workspace.write_script("acp_env_audit.mjs", REAL_ACP_RESPONDER_JS);
            let spec = AcpExternalHarnessSpec::new(
                "local-node-acp",
                "node",
                vec![
                    script.to_string_lossy().into_owned(),
                    "env_audit".to_owned(),
                ],
                [AcpSandboxTier::Tier2LocalOs],
            );
            let policy = AcpStandingPolicy::fail_closed("tenant-env", "ops");
            let config = AcpSessionLaunchConfig::new(
                workspace.path(),
                AcpSandboxTier::Tier2LocalOs,
                policy,
                "ohc_surr_envcanary123456",
                "http://127.0.0.1:18443",
            );

            let mut session = AcpGovernedSession::spawn(&spec, config).expect("spawn ACP child");
            session.start_session().await.expect("start ACP session");
            let turn = session
                .prompt("report env")
                .await
                .expect("prompt ACP child");
            session.shutdown().await.expect("shutdown ACP session");
            assert_eq!(turn.final_text.as_deref(), Some("env-audit-recorded"));

            let audit_raw = fs::read_to_string(workspace.path().join("env-audit.json"))
                .expect("read child env audit file");
            let child_env: BTreeMap<String, String> =
                serde_json::from_str(&audit_raw).expect("parse child process.env dump");

            assert!(!child_env.contains_key("OPENAI_API_KEY"));
            assert!(!child_env.contains_key("STRIPE_SECRET_KEY"));
            assert!(!child_env.contains_key("ANTHROPIC_API_KEY"));
            for (k, v) in &child_env {
                assert!(
                    !v.contains("sk-live-") && !v.contains("sk_live_") && !v.contains("sk-ant-"),
                    "raw host secret leaked into child env key {k}"
                );
            }
            assert_eq!(
                child_env.get("OHC_SURROGATE_TOKEN").map(String::as_str),
                Some("ohc_surr_envcanary123456")
            );
            assert_eq!(
                child_env.get("OHC_SENTINEL_PROXY_URL").map(String::as_str),
                Some("http://127.0.0.1:18443")
            );
            assert_eq!(
                child_env.get("HTTP_PROXY").map(String::as_str),
                Some("http://127.0.0.1:18443")
            );
            assert_eq!(
                child_env.get("HTTPS_PROXY").map(String::as_str),
                Some("http://127.0.0.1:18443")
            );
            assert!(child_env.get("PATH").is_some_and(|v| !v.is_empty()));
            let expected_home = workspace
                .path()
                .join(".ohc-home")
                .to_string_lossy()
                .into_owned();
            let expected_tmp = workspace
                .path()
                .join(".ohc-tmp")
                .to_string_lossy()
                .into_owned();
            assert_eq!(
                child_env.get("HOME").map(String::as_str),
                Some(expected_home.as_str())
            );
            assert_eq!(
                child_env.get("TMPDIR").map(String::as_str),
                Some(expected_tmp.as_str())
            );
            assert_eq!(
                child_env.get("OHC_TENANT_ID").map(String::as_str),
                Some("tenant-env")
            );
            assert_eq!(
                child_env.get("OHC_SANDBOX_TIER").map(String::as_str),
                Some("tier2_local_os")
            );
        });
        std::process::exit(ISOLATED_ENV_SUCCESS_EXIT);
    }

    let mut command = StdCommand::new(std::env::current_exe().expect("current test binary"));
    command
        .args(["--exact", test_name, "--nocapture"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    for key in ["PATH", "LANG", "LC_ALL", "TMPDIR", "TMP", "TEMP"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command
        .env(ISOLATED_ENV_TEST_MARKER, test_name)
        .env("OPENAI_API_KEY", "sk-live-host-openai-canary-9999")
        .env("STRIPE_SECRET_KEY", "sk_live_host_stripe_canary_8888")
        .env("ANTHROPIC_API_KEY", "sk-ant-host-anthropic-canary-7777")
        .spawn()
        .expect("spawn isolated env test");

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert_eq!(
                    status.code(),
                    Some(ISOLATED_ENV_SUCCESS_EXIT),
                    "isolated env test exited unexpectedly: {status}"
                );
                return;
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            other => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("isolated env test timed out or failed: {other:?}");
            }
        }
    }
}

#[tokio::test]
async fn full_acp_lifecycle_injects_mcp_servers_and_streams_updates() {
    let workspace = TempWorkspace::new("lifecycle");
    let script = workspace.write_script("acp_lifecycle.mjs", REAL_ACP_RESPONDER_JS);
    let mut spec = node_harness_spec(&script, true);
    spec.args.push("lifecycle".to_owned());

    let policy = AcpStandingPolicy::fail_closed("tenant-alpha", "engineering");
    let config = AcpSessionLaunchConfig::new(
        workspace.path(),
        AcpSandboxTier::Tier3CloudMicroVm,
        policy,
        "ohc_surr_lifecycle987654",
        "http://127.0.0.1:18443",
    )
    .with_ohc_mcp_base_url("http://127.0.0.1:8080");

    let mut session = AcpGovernedSession::spawn(&spec, config).expect("spawn ACP session");
    let (info, native) = session
        .start_session()
        .await
        .expect("initialize + session/new");
    assert_eq!(info.protocol_version, ACP_PROTOCOL_VERSION);
    assert_eq!(info.agent_name.as_deref(), Some("local-node-acp"));
    assert_eq!(native.native_session_id, "acp-sess-1");

    let (stream_tx, mut stream_rx) = mpsc::channel(32);
    let turn = session
        .prompt_with_stream("Inspect injected MCP configuration", Some(stream_tx))
        .await
        .expect("run ACP prompt");
    session.shutdown().await.expect("shutdown session");

    let mut streamed_types = Vec::new();
    while let Ok(event) = stream_rx.try_recv() {
        streamed_types.push(event.event_type);
    }
    assert_eq!(
        streamed_types,
        vec![
            "assistant.reasoning",
            "assistant.text_chunk",
            "assistant.text_chunk",
            "usage.recorded",
            "turn.completed",
        ]
    );
    assert_eq!(turn.stop_reason, "end_turn");
    assert!(!turn.cancelled);
    assert_eq!(
        turn.usage,
        Some(json!({ "inputTokens": 80, "outputTokens": 40 }))
    );

    let text = turn
        .final_text
        .as_deref()
        .expect("accumulated streamed text");
    let (session_params_json, suffix) = text.split_once("::OK").expect("streamed suffix");
    assert!(suffix.is_empty());
    let new_params: Value = serde_json::from_str(session_params_json).expect("session/new params");
    assert_eq!(
        new_params["cwd"],
        Value::String(workspace.path().to_string_lossy().into_owned())
    );
    let mcp_servers = new_params["mcpServers"]
        .as_array()
        .expect("mcpServers array");
    assert_eq!(mcp_servers.len(), 1);
    assert_eq!(mcp_servers[0]["name"], "ohc");
    assert_eq!(mcp_servers[0]["command"], "node");
    assert_eq!(
        mcp_servers[0]["args"],
        json!([
            "scripts/ohc-mcp-stdio.mjs",
            "--base-url",
            "http://127.0.0.1:8080"
        ])
    );
    let env_entries = mcp_servers[0]["env"].as_array().expect("mcp env array");
    assert!(env_entries.contains(&json!({
        "name": "OHC_BEARER_TOKEN",
        "value": "ohc_surr_lifecycle987654"
    })));
    assert!(env_entries.contains(&json!({
        "name": "OHC_MCP_ALLOW_WRITE",
        "value": "false"
    })));
}

#[tokio::test]
async fn permission_governance_auto_approves_read_escalates_high_risk_and_rejects_over_budget() {
    let workspace = TempWorkspace::new("permissions");
    let script = workspace.write_script("acp_permissions.mjs", REAL_ACP_RESPONDER_JS);
    let mut spec = node_harness_spec(&script, false);
    spec.args.push("permissions".to_owned());

    let ledger = sqlite_ledger_with_limit("tenant-acme", 1_000).await;
    let budget_guard = AcpBudgetGuard::new(ledger.clone(), managed_scope("tenant-acme"));
    let policy = AcpStandingPolicy {
        tenant_id: "tenant-acme".to_owned(),
        department: "operations".to_owned(),
        auto_approve_max_micros: 800,
        allow_external_send: false,
        allow_financial_commit: false,
        allowed_read_tools: BTreeSet::from(["read_file".to_owned(), "search_crm".to_owned()]),
    };

    let config = AcpSessionLaunchConfig::new(
        workspace.path(),
        AcpSandboxTier::Tier2LocalOs,
        policy,
        "ohc_surr_govcheck445566",
        "http://127.0.0.1:18443",
    )
    .with_budget_guard(budget_guard);

    let mut session = AcpGovernedSession::spawn(&spec, config).expect("spawn ACP session");
    session.start_session().await.expect("start ACP session");
    let turn = session
        .prompt("Exercise permission governance")
        .await
        .expect("complete permission turn");
    session.shutdown().await.expect("shutdown session");

    assert_eq!(turn.permission_decisions.len(), 4);

    // 1. Safe read-only tool within standing authority and budget -> AutoApproved
    match &turn.permission_decisions[0].outcome {
        AcpPermissionOutcome::AutoApproved {
            option_id,
            reserved_micros,
            ledger_event_id,
            ..
        } => {
            assert_eq!(option_id, "allow_once");
            assert_eq!(*reserved_micros, 200);
            assert!(ledger_event_id.is_some());
        }
        other => panic!("expected AutoApproved for read_file, got {other:?}"),
    }

    // 2. High-risk external write (send_email) -> Escalated / AwaitingApproval with SHA-256
    let email_sha = match &turn.permission_decisions[1].outcome {
        AcpPermissionOutcome::AwaitingApproval {
            ticket,
            acp_decision,
            http_status,
            selected_option_id,
        } => {
            assert_eq!(acp_decision, "awaiting_approval");
            assert_eq!(*http_status, 403);
            assert_eq!(selected_option_id.as_deref(), Some("reject_once"));
            assert_eq!(ticket.reason, "external_send_requires_approval");
            assert_eq!(ticket.risk_class, AcpActionRisk::ExternalSend);
            assert_eq!(ticket.payload_sha256.len(), 64);
            ticket.payload_sha256.clone()
        }
        other => panic!("expected AwaitingApproval for send_email, got {other:?}"),
    };

    // Canonical SHA-256 must match across reordered JSON keys.
    let expected_email_sha = canonical_payload_sha256(&json!({
        "raw_input": { "body": "Hello", "subject": "Invoice", "to": "owner@example.com" },
        "estimated_cost_micros": 0,
        "acp_kind": "other",
        "tool_name": "send_email",
        "tool_call_id": "tc-email-2",
        "session_id": "acp-sess-1",
        "department": "operations",
        "tenant_id": "tenant-acme",
    }));
    assert_eq!(email_sha, expected_email_sha);

    // 3. Financial commit -> Escalated / AwaitingApproval (cannot bypass standing authority)
    match &turn.permission_decisions[2].outcome {
        AcpPermissionOutcome::AwaitingApproval {
            ticket,
            acp_decision,
            http_status,
            ..
        } => {
            assert_eq!(acp_decision, "awaiting_approval");
            assert_eq!(*http_status, 403);
            assert_eq!(ticket.reason, "financial_commit_requires_approval");
            assert_eq!(ticket.risk_class, AcpActionRisk::FinancialCommit);
            assert_eq!(ticket.payload_sha256.len(), 64);
        }
        other => panic!("expected AwaitingApproval for financial_commit, got {other:?}"),
    }

    // 4. Over-budget action (900 micros requested, only 800 remaining) -> Rejected 409
    match &turn.permission_decisions[3].outcome {
        AcpPermissionOutcome::Rejected {
            reason,
            acp_decision,
            http_status,
            selected_option_id,
            payload_sha256,
        } => {
            assert_eq!(reason, "budget_exhausted");
            assert_eq!(acp_decision, "reject");
            assert_eq!(*http_status, 409);
            assert_eq!(selected_option_id.as_deref(), Some("reject_once"));
            assert_eq!(payload_sha256.len(), 64);
        }
        other => panic!("expected Rejected 409 for over-budget action, got {other:?}"),
    }

    // Verify exact responses observed by the child harness (including -32601 for fs/write_text_file)
    let child_observed: Vec<Value> = serde_json::from_str(
        turn.final_text
            .as_deref()
            .expect("child reported observed responses"),
    )
    .expect("parse child permission log");
    assert_eq!(child_observed.len(), 5);
    assert_eq!(
        child_observed[0]["result"]["outcome"],
        json!({ "outcome": "selected", "optionId": "allow_once" })
    );
    assert_eq!(
        child_observed[1]["result"]["outcome"],
        json!({ "outcome": "selected", "optionId": "reject_once" })
    );
    assert_eq!(
        child_observed[1]["result"]["_meta"]["ohcDecision"],
        "awaiting_approval"
    );
    assert_eq!(
        child_observed[3]["result"]["_meta"]["httpStatus"],
        json!(409)
    );
    assert_eq!(child_observed[4]["error"]["code"], json!(-32601));

    // Only the single approved 200-micro action should be reserved in SQLite UsageLedger.
    let summary = ledger.summary("tenant-acme").await.expect("ledger summary");
    assert_eq!(summary.reserved_micros, 200);
    assert_eq!(summary.spent_micros, 0);
}

#[tokio::test]
async fn session_cancel_aborts_gracefully_and_force_kills_unresponsive_child_without_zombies() {
    // Part A: Graceful ACP session/cancel
    {
        let workspace = TempWorkspace::new("cancel-graceful");
        let script = workspace.write_script("acp_cancel_graceful.mjs", REAL_ACP_RESPONDER_JS);
        let mut spec = node_harness_spec(&script, false);
        spec.args.push("cancel_graceful".to_owned());

        let policy = AcpStandingPolicy::fail_closed("tenant-cancel", "ops");
        let config = AcpSessionLaunchConfig::new(
            workspace.path(),
            AcpSandboxTier::Tier2LocalOs,
            policy,
            "ohc_surr_cancelok112233",
            "http://127.0.0.1:18443",
        )
        .with_cancel_grace_period(Duration::from_secs(2));

        let mut session = AcpGovernedSession::spawn(&spec, config).expect("spawn cancel session");
        let pid = session.process_id().await.expect("child pid");
        session.start_session().await.expect("start session");

        let cancel_handle = session.cancel_handle();
        let (stream_tx, mut stream_rx) = mpsc::channel(16);
        let prompt_task = tokio::spawn(async move {
            let result = session
                .prompt_with_stream("long running work", Some(stream_tx))
                .await;
            let _ = session.shutdown().await;
            result
        });

        let first_event = tokio::time::timeout(Duration::from_secs(5), stream_rx.recv())
            .await
            .expect("wait for first chunk")
            .expect("received first chunk");
        assert_eq!(first_event.event_type, "assistant.text_chunk");

        cancel_handle.cancel();
        let turn = prompt_task
            .await
            .expect("join prompt task")
            .expect("graceful cancel turn");
        assert!(turn.cancelled);
        assert!(!turn.force_killed);
        assert_eq!(turn.stop_reason, "cancelled");
        assert_process_not_running_or_zombie(pid);
    }

    // Part B: Unresponsive child -> kill fallback + zero zombies
    {
        let workspace = TempWorkspace::new("cancel-hang");
        let script = workspace.write_script("acp_cancel_hang.mjs", REAL_ACP_RESPONDER_JS);
        let mut spec = node_harness_spec(&script, false);
        spec.args.push("cancel_hang".to_owned());

        let policy = AcpStandingPolicy::fail_closed("tenant-cancel", "ops");
        let config = AcpSessionLaunchConfig::new(
            workspace.path(),
            AcpSandboxTier::Tier2LocalOs,
            policy,
            "ohc_surr_cancelkill9988",
            "http://127.0.0.1:18443",
        )
        .with_cancel_grace_period(Duration::from_millis(150));

        let mut session = AcpGovernedSession::spawn(&spec, config).expect("spawn hang session");
        let pid = session.process_id().await.expect("child pid");
        session.start_session().await.expect("start session");

        let cancel_handle = session.cancel_handle();
        let (stream_tx, mut stream_rx) = mpsc::channel(16);
        let prompt_task = tokio::spawn(async move {
            let result = session
                .prompt_with_stream("hung work", Some(stream_tx))
                .await;
            let _ = session.shutdown().await;
            result
        });

        let _ = tokio::time::timeout(Duration::from_secs(5), stream_rx.recv())
            .await
            .expect("wait for initial hung chunk")
            .expect("received initial hung chunk");

        cancel_handle.cancel();
        let turn = prompt_task
            .await
            .expect("join hang task")
            .expect("force-killed cancel turn");
        assert!(turn.cancelled);
        assert!(turn.force_killed);
        assert_eq!(turn.stop_reason, "cancelled");
        assert_process_not_running_or_zombie(pid);
    }
}

#[tokio::test]
async fn oversized_acp_frame_over_256kib_is_rejected_and_reaps_child() {
    let workspace = TempWorkspace::new("oversized");
    let script = workspace.write_script("acp_oversized.mjs", REAL_ACP_RESPONDER_JS);
    let mut spec = node_harness_spec(&script, false);
    spec.args.push("oversized".to_owned());

    let policy = AcpStandingPolicy::fail_closed("tenant-frame", "ops");
    let config = AcpSessionLaunchConfig::new(
        workspace.path(),
        AcpSandboxTier::Tier2LocalOs,
        policy,
        "ohc_surr_frameguard1234",
        "http://127.0.0.1:18443",
    );

    let mut session = AcpGovernedSession::spawn(&spec, config).expect("spawn session");
    let pid = session.process_id().await.expect("child pid");
    session.start_session().await.expect("start session");
    let err = session
        .prompt("emit oversized frame")
        .await
        .expect_err("oversized frame must fail closed");
    assert!(
        matches!(err, HarnessAdapterError::InvalidResponse(ref msg) if msg.contains("262144")),
        "unexpected error: {err:?}"
    );
    session.shutdown().await.expect("shutdown session");
    assert_process_not_running_or_zombie(pid);
}

#[test]
fn registry_config_as_code_env_guards_and_tier_enforcement_fail_closed() {
    let bridge = AcpMcpBridgeCommand::ohc_stdio_bridge("node", "scripts/ohc-mcp-stdio.mjs");
    let registry = AcpHarnessRegistry::with_builtin_specs(Some(bridge));
    assert!(registry.get("codex").is_some());
    assert!(registry.get("opencode").is_some());
    assert!(registry.get("goose").is_some());

    // Tier 1 WASM is not permitted for full process harnesses.
    assert!(
        registry
            .resolve_for_tier("codex", AcpSandboxTier::Tier1ToolWasm)
            .is_err()
    );
    assert!(
        registry
            .resolve_for_tier("codex", AcpSandboxTier::Tier3CloudMicroVm)
            .is_ok()
    );

    // Unsupported ACP protocol version fails closed.
    let bad_json = r#"[{"harness_id":"custom","executable":"custom-acp","args":[],"protocol_version":99,"supported_sandbox_tiers":["tier2_local_os"]}]"#;
    assert!(AcpHarnessRegistry::from_json(bad_json).is_err());

    // Raw API keys are strictly rejected as surrogate tokens.
    let workspace = TempWorkspace::new("env-guard");
    for raw_secret in [
        "sk-live-secret-key-12345678",
        "sk_live_stripe_key_12345678",
        "ghp_1234567890abcdef",
        "Bearer ohc_surr_12345678",
        "arbitrary-not-a-surrogate",
    ] {
        assert!(
            validate_opaque_surrogate_token(raw_secret).is_err(),
            "raw secret should be rejected: {raw_secret}"
        );
    }
    assert!(validate_opaque_surrogate_token("ohc_surr_abcdef123456").is_ok());
    assert!(validate_opaque_surrogate_token(&Uuid::new_v4().to_string()).is_ok());

    // Blocked credential keys cannot be smuggled through extra_non_secret_env.
    let env_err = build_sanitized_env(&AcpSanitizedEnvInput {
        workspace_dir: workspace.path().to_path_buf(),
        surrogate_token: "ohc_surr_abcdef123456".to_owned(),
        sentinel_proxy_url: "http://127.0.0.1:18443".to_owned(),
        tenant_id: "tenant-1".to_owned(),
        sandbox_tier: AcpSandboxTier::Tier2LocalOs,
        extra_non_secret_env: BTreeMap::from([(
            "OPENAI_API_KEY".to_owned(),
            "not-allowed".to_owned(),
        )]),
    });
    assert!(env_err.is_err());
}

#[tokio::test]
async fn byok_mode_records_visibility_without_rebilling_and_rejects_allow_always_only() {
    let ledger = sqlite_ledger_with_limit("tenant-byok", 1_000).await;
    let scope = UsageScope {
        tenant_id: "tenant-byok".to_owned(),
        task_id: "task-byok-1".to_owned(),
        attempt_id: "attempt-byok-1".to_owned(),
        provider: "openai-byok".to_owned(),
        model: "gpt-byok".to_owned(),
        payer: PayerMode::ByokApi,
        rate_card: None,
    };
    let guard = AcpBudgetGuard::new(ledger.clone(), scope);
    let policy = AcpStandingPolicy {
        tenant_id: "tenant-byok".to_owned(),
        department: "finance".to_owned(),
        auto_approve_max_micros: 500,
        allow_external_send: false,
        allow_financial_commit: false,
        allowed_read_tools: BTreeSet::from(["read_file".to_owned()]),
    };

    // 1. BYOK read_file with allow_once is admitted with 0 reserved_micros (never rebilled).
    let req = crate::middleware::json_rpc::JsonRpcServerRequest {
        id: crate::middleware::json_rpc::JsonRpcId::from_number(10),
        method: "session/request_permission".to_owned(),
        params: json!({
            "sessionId": "sess-byok",
            "toolCall": {
                "toolCallId": "tc-byok-1",
                "toolName": "read_file",
                "kind": "read",
                "estimatedCostMicros": 250,
                "rawInput": { "path": "notes.md" }
            },
            "options": [
                { "optionId": "allow_once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "reject_once", "name": "Reject once", "kind": "reject_once" }
            ]
        }),
    };
    let decision = evaluate_permission_request(&policy, Some(&guard), "sess-byok", &req)
        .await
        .expect("evaluate BYOK permission");
    match decision.outcome {
        AcpPermissionOutcome::AutoApproved {
            reserved_micros, ..
        } => assert_eq!(reserved_micros, 0),
        other => panic!("expected AutoApproved for BYOK read_file, got {other:?}"),
    }
    assert_eq!(
        ledger.summary("tenant-byok").await.unwrap().reserved_micros,
        0
    );

    // 2. Harness offering ONLY allow_always (no allow_once) must fail closed.
    let blanket_req = crate::middleware::json_rpc::JsonRpcServerRequest {
        id: crate::middleware::json_rpc::JsonRpcId::from_number(11),
        method: "session/request_permission".to_owned(),
        params: json!({
            "sessionId": "sess-byok",
            "toolCall": {
                "toolCallId": "tc-byok-2",
                "toolName": "read_file",
                "kind": "read",
                "rawInput": { "path": "notes.md" }
            },
            "options": [
                { "optionId": "allow_always", "name": "Allow always", "kind": "allow_always" }
            ]
        }),
    };
    let blanket_decision =
        evaluate_permission_request(&policy, Some(&guard), "sess-byok", &blanket_req)
            .await
            .expect("evaluate blanket option");
    match blanket_decision.outcome {
        AcpPermissionOutcome::Rejected {
            reason,
            http_status,
            ..
        } => {
            assert_eq!(reason, "no_allow_once_option_offered");
            assert_eq!(http_status, 403);
        }
        other => panic!("expected Rejected for allow_always-only option, got {other:?}"),
    }
}

fn assert_process_not_running_or_zombie(pid: u32) {
    #[cfg(target_os = "linux")]
    {
        let proc_path = PathBuf::from(format!("/proc/{pid}"));
        let deadline = Instant::now() + Duration::from_secs(2);
        while proc_path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(
            !proc_path.exists(),
            "child process {pid} still exists (running or zombie) after cancel/shutdown"
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
    }
}
