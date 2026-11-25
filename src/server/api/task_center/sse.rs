//! Tenant-scoped SSE broadcast hub and frame-injection-safe event stream.
use super::types::TaskCenterSseEvent;
use chrono::Utc;
use dashmap::DashMap;
use futures_util::Stream;
use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};
use uuid::Uuid;

const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
const SSE_CHANNEL_CAPACITY: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseEventKind {
    TaskUpdated,
    StepAppended,
    ApprovalRequired,
    ArtifactCreated,
    SpaceUpdated,
    ResyncRequired,
}

impl SseEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TaskUpdated => "task_updated",
            Self::StepAppended => "step_appended",
            Self::ApprovalRequired => "approval_required",
            Self::ArtifactCreated => "artifact_created",
            Self::SpaceUpdated => "space_updated",
            Self::ResyncRequired => "resync_required",
        }
    }
}

pub fn format_sse_frame(event_type_raw: &str, event: &TaskCenterSseEvent) -> String {
    let safe = match event_type_raw {
        "task_updated" | "step_appended" | "approval_required" | "artifact_created"
        | "space_updated" | "resync_required" => event_type_raw,
        _ => "task_updated",
    };
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_string());
    format!("event: {safe}\ndata: {data}\n\n")
}

#[derive(Default)]
pub struct TaskCenterEventHub {
    channels: DashMap<String, broadcast::Sender<TaskCenterSseEvent>>,
}

impl TaskCenterEventHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&self, tenant_id: &str) -> broadcast::Receiver<TaskCenterSseEvent> {
        self.channels
            .entry(tenant_id.to_string())
            .or_insert_with(|| broadcast::channel(SSE_CHANNEL_CAPACITY).0)
            .subscribe()
    }

    pub fn publish(&self, tenant_id: &str, kind: SseEventKind, payload: serde_json::Value) {
        if let Some(tx) = self.channels.get(tenant_id) {
            let _ = tx.send(TaskCenterSseEvent {
                event_id: format!("evt-{}", Uuid::new_v4()),
                kind: kind.as_str().to_string(),
                tenant_id: tenant_id.to_string(),
                occurred_at_unix: Utc::now().timestamp(),
                payload,
            });
        }
    }
}

pub struct TaskCenterEventStream {
    rx: BroadcastStream<TaskCenterSseEvent>,
    tenant_id: String,
    keep_alive: tokio::time::Interval,
    closed: bool,
}

impl TaskCenterEventStream {
    pub fn new(rx: broadcast::Receiver<TaskCenterSseEvent>, tenant_id: String) -> Self {
        let mut keep_alive = tokio::time::interval(KEEP_ALIVE_INTERVAL);
        keep_alive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            rx: BroadcastStream::new(rx),
            tenant_id,
            keep_alive,
            closed: false,
        }
    }
}

impl Stream for TaskCenterEventStream {
    type Item = Result<String, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.closed {
            return Poll::Ready(None);
        }
        match Pin::new(&mut this.rx).poll_next(cx) {
            Poll::Ready(Some(Ok(ev))) => {
                return Poll::Ready(Some(Ok(format_sse_frame(&ev.kind, &ev))));
            }
            Poll::Pending => {}
            Poll::Ready(Some(Err(BroadcastStreamRecvError::Lagged(missed)))) => {
                let ev = TaskCenterSseEvent {
                    event_id: format!("evt-{}", Uuid::new_v4()),
                    kind: SseEventKind::ResyncRequired.as_str().to_string(),
                    tenant_id: this.tenant_id.clone(),
                    occurred_at_unix: Utc::now().timestamp(),
                    payload: serde_json::json!({ "missed": missed }),
                };
                return Poll::Ready(Some(Ok(format_sse_frame("resync_required", &ev))));
            }
            Poll::Ready(None) => {
                this.closed = true;
                return Poll::Ready(None);
            }
        }
        if this.keep_alive.poll_tick(cx).is_ready() {
            return Poll::Ready(Some(Ok(": ping\n\n".to_string())));
        }
        Poll::Pending
    }
}
