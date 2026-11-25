//! Omni Assistant & OHC Task Center Declarative Spaces HTTP/SSE API (`/api/v1/task-center`).
pub mod executor;
mod handlers;
mod provider;
mod rows;
mod sse;
pub mod store;
#[cfg(test)]
mod tests;
mod types;

use self::executor::{
    ExecutorError, SseEventKind, TaskCenterEventHub, TaskCenterEventStream, TaskCenterExecutor,
};
use self::store::TaskCenterStore;
use self::types::{
    MAX_MICROS, MAX_SPACE_NAME_CHARS, PayerMode, ProvenanceState, SpaceKind, SpacePatch,
    SpaceRecord, SpaceSourceHealth, StandingPolicyRecord, StoreError, TaskCenterSummaryResponse,
    contains_unsafe_html,
};
use crate::db::DB;
use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, patch, post, put},
};
use chrono::Utc;
use serde::Deserialize;
use server_common::Claims;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone)]
pub struct TaskCenterState {
    pub store: TaskCenterStore,
    pub executor: Arc<TaskCenterExecutor>,
    pub events: Arc<TaskCenterEventHub>,
}

impl TaskCenterState {
    pub fn new(db: Arc<DB>, executor: Arc<TaskCenterExecutor>) -> Self {
        Self {
            store: TaskCenterStore::new(db),
            executor,
            events: Arc::new(TaskCenterEventHub::new()),
        }
    }
}

pub fn router<S: Clone + Send + Sync + 'static>(
    db: Arc<DB>,
    executor: Arc<TaskCenterExecutor>,
) -> Router<S> {
    router_with_state(TaskCenterState::new(db, executor))
}

pub fn router_with_state<S: Clone + Send + Sync + 'static>(state: TaskCenterState) -> Router<S> {
    Router::new()
        .route("/summary", get(get_summary))
        .route("/spaces", get(list_spaces).post(create_space))
        .route("/spaces/{id}", patch(patch_space))
        .route(
            "/tasks",
            get(handlers::list_tasks).post(handlers::create_task),
        )
        .route("/tasks/{id}", get(handlers::get_task))
        .route("/tasks/{id}/approve", post(handlers::approve_task))
        .route("/tasks/{id}/cancel", post(handlers::cancel_task))
        .route("/tasks/{id}/messages", post(handlers::post_task_message))
        .route(
            "/tasks/{id}/artifacts/{artifact_id}/pin",
            post(handlers::pin_task_artifact),
        )
        .route("/policies", get(list_policies))
        .route("/policies/{department}", put(upsert_policy))
        .route("/stream", get(stream_events))
        .with_state(state)
}

#[derive(Debug)]
pub(super) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    unconfigured: bool,
}

impl ApiError {
    pub(super) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            unconfigured: false,
        }
    }
    pub(super) fn unconfigured(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "llm_unconfigured",
            message: message.into(),
            unconfigured: true,
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Invalid(m) => Self::new(StatusCode::BAD_REQUEST, "invalid_request", m),
            StoreError::NotFound => {
                Self::new(StatusCode::NOT_FOUND, "not_found", "Resource not found")
            }
            StoreError::Conflict(m) => Self::new(StatusCode::CONFLICT, "conflict", m),
            StoreError::Database(err) => {
                tracing::error!("task_center store error: {err}");
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "service_unavailable",
                    "Task Center persistence is unavailable",
                )
            }
        }
    }
}

impl From<ExecutorError> for ApiError {
    fn from(e: ExecutorError) -> Self {
        match e {
            ExecutorError::Unconfigured(m) => Self::unconfigured(m),
            ExecutorError::BudgetExhausted(m) => {
                Self::new(StatusCode::CONFLICT, "budget_exhausted", m)
            }
            ExecutorError::BudgetUnconfigured(m) => {
                Self::new(StatusCode::CONFLICT, "budget_unconfigured", m)
            }
            ExecutorError::Invalid(m) => Self::new(StatusCode::BAD_REQUEST, "invalid_request", m),
            ExecutorError::Conflict(m) => Self::new(StatusCode::CONFLICT, "conflict", m),
            ExecutorError::LedgerUnavailable => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "Usage ledger is unavailable",
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = serde_json::json!({ "error": self.message, "code": self.code });
        if self.unconfigured {
            body["provenance"] = serde_json::json!(ProvenanceState::Unconfigured);
            body["payer_mode"] = serde_json::json!(PayerMode::Unconfigured);
        }
        (
            self.status,
            [(header::CACHE_CONTROL, "private, no-store")],
            Json(body),
        )
            .into_response()
    }
}

pub(super) fn private_json<T: serde::Serialize>(status: StatusCode, body: T) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(body),
    )
        .into_response()
}

pub(super) fn require_tenant(claims: &Claims) -> Result<String, ApiError> {
    if claims.exp > 0 && claims.exp <= Utc::now().timestamp() {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Session token has expired",
        ));
    }
    ::server_common::auth_utils::signed_tenant_id(claims).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Signed tenant claim is required",
        )
    })
}

pub(super) fn require_owner_or_admin(claims: &Claims) -> Result<String, ApiError> {
    let tenant = require_tenant(claims)?;
    if !claims
        .roles
        .iter()
        .any(|r| r.eq_ignore_ascii_case("owner") || r.eq_ignore_ascii_case("admin"))
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Owner or administrator role is required",
        ));
    }
    Ok(tenant)
}

pub(super) fn valid_id(value: &str, max_len: usize) -> bool {
    let t = value.trim();
    !t.is_empty()
        && t == value
        && t.len() <= max_len
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

pub(super) fn parse_idempotency_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let raw = values.next().ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "A single canonical non-nil Idempotency-Key UUID header is required",
        )
    })?;
    if values.next().is_some() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Duplicate Idempotency-Key header",
        ));
    }
    let val = raw.to_str().map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid Idempotency-Key encoding",
        )
    })?;
    let id = Uuid::parse_str(val).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Idempotency-Key must be a canonical UUID",
        )
    })?;
    if id.is_nil() || id.to_string() != val {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Idempotency-Key must be a canonical lowercase non-nil UUID",
        ));
    }
    Ok(val.to_string())
}

fn validate_manifest(
    values: Option<Vec<String>>,
    fallback: &[&str],
    field: &'static str,
) -> Result<Vec<String>, ApiError> {
    let Some(items) = values else {
        return Ok(fallback.iter().map(|s| (*s).to_string()).collect());
    };
    if items.is_empty() || items.len() > 20 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            field,
        ));
    }
    items
        .into_iter()
        .map(|item| {
            let t = item.trim();
            if t.is_empty()
                || t.chars().count() > 160
                || t.chars().any(char::is_control)
                || contains_unsafe_html(t)
            {
                Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    field,
                ))
            } else {
                Ok(t.to_string())
            }
        })
        .collect()
}

fn default_manifest(
    kind: SpaceKind,
) -> (
    &'static [&'static str],
    &'static [&'static str],
    &'static [&'static str],
) {
    const CANNOT: &[&str] = &[
        "Spend above standing authority without owner approval",
        "Send external messages without explicit approval",
        "Execute arbitrary unsandboxed scripts",
    ];
    match kind {
        SpaceKind::Operations => (
            &["Tenant tasks & steps", "Standing authority policies"],
            &["Live attention queue", "Verified task artifacts"],
            CANNOT,
        ),
        SpaceKind::ClientDelivery => (
            &["Client deliverables & drafts", "Verified connector records"],
            &["Delivery checklist", "Approval status cards"],
            CANNOT,
        ),
        SpaceKind::CashRecovery => (
            &["Usage ledger & budgets", "Invoice recovery drafts"],
            &["Budget & spend summary", "Pending financial approvals"],
            CANNOT,
        ),
        SpaceKind::Research => (
            &["Submitted tenant prompts & artifacts"],
            &["Markdown synthesis cards", "Pinned findings"],
            CANNOT,
        ),
        SpaceKind::Custom => (
            &["Scoped tenant records"],
            &["Declarative task & artifact view"],
            CANNOT,
        ),
    }
}

pub(super) fn normalize_space_health(space: &mut SpaceRecord, llm_configured: bool) {
    if matches!(
        space.source_health,
        SpaceSourceHealth::Live | SpaceSourceHealth::Unconfigured
    ) {
        space.source_health = if llm_configured {
            SpaceSourceHealth::Live
        } else {
            SpaceSourceHealth::Unconfigured
        };
    }
}

#[derive(Default, Deserialize)]
struct SpacesQuery {
    #[serde(default)]
    include_archived: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateSpaceRequest {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    kind: SpaceKind,
    #[serde(default)]
    prompt_template: String,
    #[serde(default)]
    department: Option<String>,
    #[serde(default)]
    reads: Option<Vec<String>>,
    #[serde(default)]
    shows: Option<Vec<String>>,
    #[serde(default)]
    cannot: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchSpaceRequest {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    artifact_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpsertPolicyRequest {
    auto_approve_max_micros: i64,
    #[serde(default)]
    allow_external_send: bool,
    #[serde(default)]
    allow_financial_commit: bool,
}

async fn get_summary(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
) -> Result<Response, ApiError> {
    let tenant_id = require_tenant(&claims)?;
    let runtime = state.executor.runtime_info(&tenant_id).await;
    let budget = state.executor.budget_info(&tenant_id).await;
    let mut spaces = state.store.list_spaces(&tenant_id, true).await?;
    for s in &mut spaces {
        normalize_space_health(s, runtime.llm_configured);
    }
    let tasks = state.store.list_tasks(&tenant_id).await?;
    let standing_policies = state.store.list_standing_policies(&tenant_id).await?;
    Ok(private_json(
        StatusCode::OK,
        TaskCenterSummaryResponse {
            spaces,
            tasks,
            standing_policies,
            runtime,
            budget,
        },
    ))
}

async fn list_spaces(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
    Query(q): Query<SpacesQuery>,
) -> Result<Response, ApiError> {
    let tenant_id = require_tenant(&claims)?;
    let runtime = state.executor.runtime_info(&tenant_id).await;
    let mut spaces = state
        .store
        .list_spaces(&tenant_id, q.include_archived.unwrap_or(true))
        .await?;
    for s in &mut spaces {
        normalize_space_health(s, runtime.llm_configured);
    }
    Ok(private_json(
        StatusCode::OK,
        serde_json::json!({ "spaces": spaces }),
    ))
}

async fn create_space(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
    Json(req): Json<CreateSpaceRequest>,
) -> Result<Response, ApiError> {
    let tenant_id = require_tenant(&claims)?;
    let name = req.name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_SPACE_NAME_CHARS
        || name.chars().any(char::is_control)
        || contains_unsafe_html(name)
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Space name must be 1..=80 safe characters",
        ));
    }
    let desc = req.description.trim();
    let tpl = req.prompt_template.trim();
    if desc.chars().count() > 1000
        || desc.contains('\0')
        || contains_unsafe_html(desc)
        || tpl.chars().count() > 4000
        || tpl.contains('\0')
        || contains_unsafe_html(tpl)
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid space description or prompt_template",
        ));
    }
    let department = req
        .department
        .unwrap_or_else(|| "operations".into())
        .trim()
        .to_ascii_lowercase();
    if !valid_id(&department, 64) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid department identifier",
        ));
    }
    let (dr, ds, dc) = default_manifest(req.kind);
    let reads = validate_manifest(req.reads, dr, "Invalid space reads manifest")?;
    let shows = validate_manifest(req.shows, ds, "Invalid space shows manifest")?;
    let cannot = validate_manifest(req.cannot, dc, "Invalid space cannot manifest")?;
    let runtime = state.executor.runtime_info(&tenant_id).await;
    let now = Utc::now().timestamp();
    let space = SpaceRecord {
        id: format!("spc-{}", Uuid::new_v4()),
        name: name.into(),
        description: desc.into(),
        kind: req.kind,
        prompt_template: tpl.into(),
        department,
        reads,
        shows,
        cannot,
        source_health: if runtime.llm_configured {
            SpaceSourceHealth::Live
        } else {
            SpaceSourceHealth::Unconfigured
        },
        pinned_artifact_ids: vec![],
        archived: false,
        created_at_unix: now,
        updated_at_unix: now,
    };
    let saved = state.store.create_space(&tenant_id, &space).await?;
    state.events.publish(
        &tenant_id,
        SseEventKind::SpaceUpdated,
        serde_json::to_value(&saved).unwrap_or_default(),
    );
    Ok(private_json(StatusCode::CREATED, saved))
}

async fn patch_space(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
    Path(space_id): Path<String>,
    Json(req): Json<PatchSpaceRequest>,
) -> Result<Response, ApiError> {
    let tenant_id = require_tenant(&claims)?;
    let patch = match req.action.as_str() {
        "rename" => {
            let name = req
                .name
                .as_deref()
                .map(str::trim)
                .filter(|n| {
                    !n.is_empty()
                        && n.chars().count() <= MAX_SPACE_NAME_CHARS
                        && !n.chars().any(char::is_control)
                        && !contains_unsafe_html(n)
                })
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "rename requires valid name (1..=80 chars)",
                    )
                })?;
            SpacePatch::Rename(name.into())
        }
        "archive" => SpacePatch::Archive,
        "unarchive" => SpacePatch::Unarchive,
        "pin_artifact" => {
            SpacePatch::PinArtifact(req.artifact_id.filter(|id| valid_id(id, 128)).ok_or_else(
                || {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "valid artifact_id required",
                    )
                },
            )?)
        }
        "unpin_artifact" => {
            SpacePatch::UnpinArtifact(req.artifact_id.filter(|id| valid_id(id, 128)).ok_or_else(
                || {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        "valid artifact_id required",
                    )
                },
            )?)
        }
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Unsupported space action",
            ));
        }
    };
    let mut updated = state
        .store
        .patch_space(&tenant_id, &space_id, patch, Utc::now().timestamp())
        .await?;
    normalize_space_health(
        &mut updated,
        state.executor.runtime_info(&tenant_id).await.llm_configured,
    );
    state.events.publish(
        &tenant_id,
        SseEventKind::SpaceUpdated,
        serde_json::to_value(&updated).unwrap_or_default(),
    );
    Ok(private_json(StatusCode::OK, updated))
}

async fn list_policies(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
) -> Result<Response, ApiError> {
    let tenant_id = require_tenant(&claims)?;
    let standing_policies = state.store.list_standing_policies(&tenant_id).await?;
    Ok(private_json(
        StatusCode::OK,
        serde_json::json!({ "standing_policies": standing_policies }),
    ))
}

async fn upsert_policy(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
    Path(department): Path<String>,
    Json(req): Json<UpsertPolicyRequest>,
) -> Result<Response, ApiError> {
    let tenant_id = require_owner_or_admin(&claims)?;
    let dept = department.trim().to_ascii_lowercase();
    if !valid_id(&dept, 64) || !(0..=MAX_MICROS).contains(&req.auto_approve_max_micros) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid department or auto_approve_max_micros",
        ));
    }
    let saved = state
        .store
        .upsert_standing_policy(
            &tenant_id,
            &StandingPolicyRecord {
                department: dept,
                auto_approve_max_micros: req.auto_approve_max_micros,
                allow_external_send: req.allow_external_send,
                allow_financial_commit: req.allow_financial_commit,
                updated_at_unix: Utc::now().timestamp(),
            },
        )
        .await?;
    Ok(private_json(StatusCode::OK, saved))
}

async fn stream_events(
    State(state): State<TaskCenterState>,
    Extension(claims): Extension<Claims>,
) -> Result<Response, ApiError> {
    let tenant_id = require_tenant(&claims)?;
    let expires_in = claims.exp.saturating_sub(Utc::now().timestamp()).max(0);
    if expires_in == 0 {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Session token has expired",
        ));
    }
    let stream = futures_util::StreamExt::take_until(
        TaskCenterEventStream::new(state.events.subscribe(&tenant_id), tenant_id),
        tokio::time::sleep(Duration::from_secs(expires_in as u64)),
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "text/event-stream".parse().expect("valid content-type"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        "private, no-store, no-cache"
            .parse()
            .expect("valid cache-control"),
    );
    headers.insert(
        header::CONNECTION,
        "keep-alive".parse().expect("valid connection"),
    );
    Ok((headers, axum::body::Body::from_stream(stream)).into_response())
}
