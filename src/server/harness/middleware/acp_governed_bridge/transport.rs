use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::{Mutex, mpsc};
use tokio_util::codec::{FramedRead, LinesCodec, LinesCodecError};

use super::super::harness::{HarnessAdapterError, HarnessEvent};
use super::super::json_rpc::{
    JsonRpcErrorObject, JsonRpcId, JsonRpcNotification, JsonRpcServerRequest,
};
use super::super::types::sanitize_credential_value;
use super::policy::{AcpPermissionDecision, AcpPermissionOutcome};

/// Hard upper bound for a single newline-delimited ACP JSON-RPC 2.0 frame.
pub const MAX_ACP_FRAME_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub(super) enum InboundFrame {
    Response {
        id: JsonRpcId,
        result: Result<Value, JsonRpcErrorObject>,
    },
    Notification(JsonRpcNotification),
    ServerRequest(JsonRpcServerRequest),
    TransportError(String),
}

pub(super) async fn read_stdout_loop(stdout: ChildStdout, tx: mpsc::Sender<InboundFrame>) {
    let mut lines = FramedRead::new(
        BufReader::new(stdout),
        LinesCodec::new_with_max_length(MAX_ACP_FRAME_BYTES),
    );
    while let Some(next) = lines.next().await {
        match next {
            Ok(line) if line.trim().is_empty() => continue,
            Ok(line) => match parse_acp_frame(line.trim()) {
                Ok(frame) => {
                    if tx.send(frame).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tx.send(InboundFrame::TransportError(error)).await;
                    break;
                }
            },
            Err(LinesCodecError::MaxLineLengthExceeded) => {
                let _ = tx
                    .send(InboundFrame::TransportError(format!(
                        "ACP JSON-RPC frame exceeds {MAX_ACP_FRAME_BYTES} byte limit"
                    )))
                    .await;
                break;
            }
            Err(LinesCodecError::Io(error)) => {
                let _ = tx
                    .send(InboundFrame::TransportError(format!(
                        "ACP stdout read error: {error}"
                    )))
                    .await;
                break;
            }
        }
    }
}

pub(super) async fn write_frame(
    stdin: &Mutex<ChildStdin>,
    message: &Value,
) -> Result<(), HarnessAdapterError> {
    let bytes = serde_json::to_vec(message).map_err(HarnessAdapterError::Json)?;
    if bytes.len() > MAX_ACP_FRAME_BYTES {
        return Err(HarnessAdapterError::InvalidRequest(format!(
            "outbound ACP JSON-RPC frame exceeds {MAX_ACP_FRAME_BYTES} byte limit"
        )));
    }
    let mut writer = stdin.lock().await;
    writer
        .write_all(&bytes)
        .await
        .map_err(HarnessAdapterError::Io)?;
    writer
        .write_all(b"\n")
        .await
        .map_err(HarnessAdapterError::Io)?;
    writer.flush().await.map_err(HarnessAdapterError::Io)
}

pub(super) fn permission_decision_event(
    session_id: &str,
    decision: &AcpPermissionDecision,
) -> HarnessEvent {
    let event_type = match &decision.outcome {
        AcpPermissionOutcome::AutoApproved { .. } => "governance.permission_approved",
        AcpPermissionOutcome::AwaitingApproval { .. } => "governance.permission_escalated",
        AcpPermissionOutcome::Rejected { .. } => "governance.permission_rejected",
    };
    HarnessEvent {
        event_type: event_type.to_owned(),
        durable: true,
        payload: sanitize_credential_value(&json!({
            "session_id": session_id,
            "tool_call_id": decision.tool_call_id,
            "tool_name": decision.tool_name,
            "risk_class": decision.risk_class,
            "estimated_cost_micros": decision.estimated_cost_micros,
            "outcome": decision.outcome,
        })),
        native_cursor: None,
    }
}

pub(super) fn cancelled_turn_event(session_id: &str, force_killed: bool) -> HarnessEvent {
    HarnessEvent {
        event_type: "turn.cancelled".to_owned(),
        durable: true,
        payload: json!({
            "session_id": session_id,
            "stop_reason": "cancelled",
            "force_killed": force_killed,
        }),
        native_cursor: None,
    }
}

pub(super) async fn emit_event(
    events: &mut Vec<HarnessEvent>,
    stream_tx: Option<&mpsc::Sender<HarnessEvent>>,
    event: HarnessEvent,
) {
    if let Some(tx) = stream_tx {
        let _ = tx.send(event.clone()).await;
    }
    events.push(event);
}

fn parse_acp_frame(line: &str) -> Result<InboundFrame, String> {
    let value: Value =
        serde_json::from_str(line).map_err(|err| format!("malformed JSON: {err}"))?;
    let object: &Map<String, Value> = value
        .as_object()
        .ok_or_else(|| "ACP JSON-RPC frame must be an object".to_owned())?;
    if let Some(version) = object.get("jsonrpc")
        && version.as_str() != Some("2.0")
    {
        return Err("ACP JSON-RPC frame must specify jsonrpc \"2.0\"".to_owned());
    }

    if let Some(method) = object.get("method").and_then(Value::as_str) {
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        if let Some(raw_id) = object.get("id") {
            let id = JsonRpcId::from_value(raw_id.clone()).map_err(|err| err.to_string())?;
            return Ok(InboundFrame::ServerRequest(JsonRpcServerRequest {
                id,
                method: method.to_owned(),
                params,
            }));
        }
        return Ok(InboundFrame::Notification(JsonRpcNotification {
            method: method.to_owned(),
            params,
        }));
    }

    let raw_id = object
        .get("id")
        .cloned()
        .ok_or_else(|| "ACP JSON-RPC response missing id".to_owned())?;
    let id = JsonRpcId::from_value(raw_id).map_err(|err| err.to_string())?;
    if object.contains_key("result") && object.contains_key("error") {
        return Err("ACP JSON-RPC response cannot contain both result and error".to_owned());
    }
    if let Some(result) = object.get("result") {
        return Ok(InboundFrame::Response {
            id,
            result: Ok(result.clone()),
        });
    }
    if let Some(error) = object.get("error") {
        let err_obj = error
            .as_object()
            .ok_or_else(|| "ACP JSON-RPC error must be an object".to_owned())?;
        return Ok(InboundFrame::Response {
            id,
            result: Err(JsonRpcErrorObject {
                code: err_obj
                    .get("code")
                    .and_then(Value::as_i64)
                    .unwrap_or(-32000),
                message: err_obj
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("ACP error")
                    .to_owned(),
                data: err_obj.get("data").cloned(),
            }),
        });
    }
    Err("ACP JSON-RPC response must contain result or error".to_owned())
}
