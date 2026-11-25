// SPDX-License-Identifier: Apache-2.0
//! HTTP auth protection guards, agent RPC gateway, help-chat, voice, and workflow handlers.

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, Request},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::sync::{Arc, OnceLock};

pub const DEFAULT_HELP_CHAT_LINKS: [&str; 8] = [
    "/help?article=getting-started-1",
    "/help?article=my-store-1",
    "/help?article=payments-1",
    "/help?article=ai-support",
    "/help?article=marketing-tools",
    "/help?article=billing-settings",
    "/api-docs",
    "/inbox",
];

#[cfg(test)]
pub(crate) fn default_help_chat_links() -> &'static [&'static str] {
    &DEFAULT_HELP_CHAT_LINKS
}

pub(crate) fn current_help_link(link: &str) -> String {
    let link = link.trim();
    if let Some(article_id) = link.strip_prefix("/help_article.html?id=") {
        let article_id = article_id.split('&').next().unwrap_or_default();
        if !article_id.is_empty()
            && article_id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            return format!("/help?article={article_id}");
        }
    }

    match link {
        "/api-docs.html" => "/api-docs".to_string(),
        "/inbox.html" => "/inbox".to_string(),
        current if DEFAULT_HELP_CHAT_LINKS.contains(&current) => current.to_string(),
        current
            if current.starts_with("/help?article=")
                && !current.contains(".html")
                && !current.contains('\r')
                && !current.contains('\n') =>
        {
            current.to_string()
        }
        _ => DEFAULT_HELP_CHAT_LINKS[0].to_string(),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HelpChatRequest {
    message: String,
}

#[derive(Clone, serde::Serialize)]
pub struct WorkflowRecord {
    pub id: String,
    pub tenant_id: String,
    pub actor_id: String,
    pub request_id: String,
    pub agent_id: Option<String>,
    pub name: String,
    pub workflow: String,
    pub task: String,
    pub model: String,
    pub provider: String,
    pub status: String,
    pub command: String,
    pub created_at: String,
    pub output: Option<String>,
    pub error: Option<String>,
}

impl From<crate::workflow_execution::receipts::Receipt> for WorkflowRecord {
    fn from(r: crate::workflow_execution::receipts::Receipt) -> Self {
        use crate::workflow_execution::receipts::StoredPhase;
        let status = match r.phase {
            StoredPhase::Queued => "queued",
            StoredPhase::Dispatching => "running",
            StoredPhase::Completed => "succeeded",
            StoredPhase::Cancelled => "canceled",
            StoredPhase::OutcomeUnknown => "outcome_unknown",
        }
        .to_string();
        Self {
            id: r.id,
            tenant_id: r.tenant_id,
            actor_id: r.actor_id,
            request_id: r.request_id,
            agent_id: r.agent_role,
            name: r.name,
            workflow: r.workflow,
            task: r.task.clone(),
            model: r.model,
            provider: r.provider,
            status,
            command: r.task,
            created_at: r.created_at.to_string(),
            output: r.output,
            error: r.error,
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateWorkflowRequest {
    name: String,
    task: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    workflow: String,
}

pub(crate) fn is_auth_bypass_path(path: &str) -> bool {
    matches!(
        path,
        "/api/v1/auth/login"
            | "/api/v1/capabilities"
            | "/health"
            | "/healthz"
            | "/readyz"
            | "/metrics"
    )
}

pub(crate) async fn protected_bearer_auth_middleware(
    axum::extract::State(store): axum::extract::State<Arc<::server_auth::Store>>,
    req: Request,
    next: Next,
) -> Response {
    if is_auth_bypass_path(req.uri().path()) {
        return next.run(req).await;
    }
    ::server_auth::strict_bearer_auth_middleware(axum::extract::State(store), req, next).await
}

/// Supply the legacy SQLx database handle to handlers that still use that
/// interface while the rest of the service migrates to the portable SeaORM
/// connection.
pub(crate) fn legacy_db_compatibility_layer(
    db: Arc<crate::db::DB>,
) -> Extension<Arc<crate::db::DB>> {
    Extension(db)
}

pub(crate) fn protect_internal_ingress(
    router: Router,
    store: Arc<::server_auth::Store>,
    tenant_ctx: &dyn ::ohc_core::extensions::TenantContextProvider,
) -> Router {
    let guarded = tenant_ctx.apply_http_guard(router);
    guarded.route_layer(axum::middleware::from_fn_with_state(
        store,
        ::server_auth::strict_bearer_auth_middleware,
    ))
}

pub(crate) async fn legacy_agent_rpc_unavailable(
    Extension(claims): Extension<::server_common::Claims>,
) -> Response {
    if ::server_common::auth_utils::signed_tenant_id(&claims).is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"Authentication required"})),
        )
            .into_response();
    }
    if !claims
        .roles
        .iter()
        .any(|role| role.eq_ignore_ascii_case("owner") || role.eq_ignore_ascii_case("admin"))
    {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error":"Owner or administrator access required"})),
        )
            .into_response();
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error":"Legacy agent RPC requires a tenant-bound runtime; no work was dispatched"})),
    )
        .into_response()
}

pub(crate) fn legacy_agent_rpc_router(
    store: Arc<::server_auth::Store>,
    tenant_ctx: &dyn ::ohc_core::extensions::TenantContextProvider,
) -> Router {
    protect_internal_ingress(
        Router::new().route("/rpc", axum::routing::post(legacy_agent_rpc_unavailable)),
        store,
        tenant_ctx,
    )
}

pub(crate) const AGENT_RPC_REQUEST_LIMIT_BYTES: usize = 1_048_576;
pub(crate) const AGENT_RPC_RESPONSE_LIMIT_BYTES: usize = 2_097_152;

pub(crate) fn agent_rpc_available(multitenant: bool) -> bool {
    !multitenant
}

pub(crate) fn extend_agent_rpc_body(
    body: &mut Vec<u8>,
    chunk: &[u8],
    maximum: usize,
) -> Result<(), ()> {
    if chunk.len() > maximum.saturating_sub(body.len()) {
        return Err(());
    }
    body.extend_from_slice(chunk);
    Ok(())
}

pub(crate) async fn read_limited_agent_rpc_body(
    response: reqwest::Response,
    maximum: usize,
) -> Result<Vec<u8>, ()> {
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(maximum as u64) as usize);
    let mut stream = response.bytes_stream();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
        let chunk = chunk.map_err(|_| ())?;
        extend_agent_rpc_body(&mut body, &chunk, maximum)?;
    }
    Ok(body)
}

pub(crate) fn allowed_agent_rpc_method(method: &str) -> bool {
    matches!(
        method,
        "aider_repomap"
            | "am_fetch_agent"
            | "am_publish_agent"
            | "am_search_agents"
            | "ap_create_task"
            | "ap_execute_step"
            | "ap_list_checkpoints"
            | "ap_list_steps"
            | "ap_list_tasks"
            | "ap_restore_checkpoint"
            | "get_sona_patterns"
            | "goose_mcp_execute"
            | "goose_mcp_list"
            | "record_sona_pattern"
            | "run_actor_model"
            | "run_agent"
            | "run_deerflow_orchestration"
            | "run_expert_team"
            | "run_ralph_loop"
            | "run_scalable_agents"
            | "verify_output"
    )
}

pub(crate) fn agent_rpc_url(raw: &str) -> Result<reqwest::Url, ()> {
    let mut url = reqwest::Url::parse(raw).map_err(|_| ())?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(());
    }
    url.set_path("/rpc");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

pub(crate) async fn proxy_agent_rpc_handler(
    Extension(claims): Extension<::server_common::Claims>,
    Json(payload): Json<serde_json::Value>,
) -> Response {
    if !agent_rpc_available(!crate::is_standalone_runtime()) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent RPC requires a tenant-partitioned runtime"
            })),
        )
            .into_response();
    }
    let Some(tenant_id) = claims
        .organization_id
        .as_deref()
        .filter(|tenant| !tenant.trim().is_empty() && *tenant != "system")
    else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "tenant context required" })),
        )
            .into_response();
    };

    let valid_envelope = payload.get("jsonrpc").and_then(|value| value.as_str()) == Some("2.0")
        && payload
            .get("method")
            .and_then(|value| value.as_str())
            .is_some_and(allowed_agent_rpc_method)
        && payload.get("id").is_some()
        && payload.get("params").is_none_or(|value| value.is_object());
    if !valid_envelope {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid RPC request" })),
        )
            .into_response();
    }

    let Some(raw_origin) = std::env::var("OMNISOLO_AGENT_URL")
        .ok()
        .filter(|origin| !origin.trim().is_empty())
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("cache-control", "private, no-store")],
            Json(serde_json::json!({ "error": "Agent runtime is not configured; no work was dispatched" })),
        )
            .into_response();
    };
    let Ok(url) = agent_rpc_url(&raw_origin) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "agent service unavailable" })),
        )
            .into_response();
    };
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("static reqwest client configuration is valid")
    });
    let mut request = client
        .post(url)
        .header("x-tenant-id", tenant_id)
        .header("x-user-id", &claims.sub)
        .json(&payload);
    if let Ok(token) = std::env::var("OMNISOLO_AGENT_TOKEN")
        && !token.trim().is_empty()
    {
        request = request.bearer_auth(token);
    }
    let Ok(upstream) = request.send().await else {
        let method = payload.get("method").and_then(|v| v.as_str()).unwrap_or("");
        if matches!(
            method,
            "am_publish_agent" | "am_search_agents" | "am_fetch_agent"
        ) {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "Marketplace outcome unavailable; no fallback was attempted. Reconcile a publication before retrying." })),
            )
                .into_response();
        }
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Agent outcome unavailable; no fallback was attempted. Reconcile work before retrying." })),
        )
            .into_response();
    };
    if matches!(upstream.status().as_u16(), 401 | 403) {
        return (
            StatusCode::BAD_GATEWAY,
            [("cache-control", "private, no-store")],
            Json(serde_json::json!({ "error": "Agent runtime authentication failed; check the server-side runtime configuration" })),
        )
            .into_response();
    }
    if upstream
        .content_length()
        .is_some_and(|length| length > AGENT_RPC_RESPONSE_LIMIT_BYTES as u64)
    {
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": "agent response too large" })),
        )
            .into_response();
    }
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let Ok(body) = read_limited_agent_rpc_body(upstream, AGENT_RPC_RESPONSE_LIMIT_BYTES).await
    else {
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": "agent response unavailable or too large" })),
        )
            .into_response();
    };
    Response::builder()
        .status(status)
        .header("content-type", "application/json; charset=utf-8")
        .header("cache-control", "private, no-store")
        .header("x-content-type-options", "nosniff")
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

pub(crate) async fn help_chat_handler(
    db: Arc<crate::db::DB>,
    Extension(claims): Extension<::server_common::Claims>,
    Json(req): Json<HelpChatRequest>,
) -> Response {
    let Some(tenant_id) = claims.organization_id else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "authentication required" })),
        )
            .into_response();
    };
    let message = req.message.trim();
    if message.is_empty() || message.chars().count() > 2_000 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid message" })),
        )
            .into_response();
    }
    let query = message.to_lowercase();
    let mut reply = "I am your AI Help Agent! I specialize in answering questions about OmniSolo features and helping you grow your small business. Check out our Getting Started guide.".to_string();
    let mut link_title = "Read the full article →";
    let mut link_url = DEFAULT_HELP_CHAT_LINKS[0].to_string();

    let articles: Vec<crate::api::docs::HelpArticle> = match &db.store {
        crate::db::DbStore::Postgres => {
            let Ok(mut tx) = db.pool.begin().await else {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "help unavailable" }))).into_response();
            };
            if ::server_common::auth_utils::set_org_context(&mut *tx, &tenant_id)
                .await
                .is_err()
            {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "help unavailable" }))).into_response();
            }
            let result = sqlx::query(
                "SELECT category, title, desc_text, link FROM help_articles WHERE tenant_id = $1",
            )
            .bind(&tenant_id)
            .fetch_all(&mut *tx)
            .await;
            if tx.commit().await.is_err() {
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": "help unavailable" }))).into_response();
            }
            match result {
                Ok(rows) => rows
                    .into_iter()
                    .map(|row| {
                        use sqlx::Row;
                        crate::api::docs::HelpArticle {
                            category: row.get("category"),
                            title: row.get("title"),
                            desc: row.get("desc_text"),
                            link: row.get("link"),
                        }
                    })
                    .collect(),
                Err(_) => vec![],
            }
        }
        crate::db::DbStore::Sqlite(pool) => match sqlx::query(
            "SELECT category, title, desc_text as text, link FROM help_articles WHERE tenant_id = ?",
        )
        .bind(&tenant_id)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => rows
                .into_iter()
                .map(|row| {
                    use sqlx::Row;
                    crate::api::docs::HelpArticle {
                        category: row.get("category"),
                        title: row.get("title"),
                        desc: row.get("text"),
                        link: row.get("link"),
                    }
                })
                .collect(),
            Err(_) => vec![],
        },
    };

    let mut matched = false;
    for article in articles {
        let lower_title = article.title.to_lowercase();
        let lower_desc = article.desc.to_lowercase();
        if query
            .split_whitespace()
            .any(|word| word.len() > 3 && (lower_title.contains(word) || lower_desc.contains(word)))
        {
            reply = format!("Based on our help center: {}", article.desc);
            link_url = current_help_link(&article.link);
            matched = true;
            break;
        }
    }

    if !matched {
        if query.contains("getting started") {
            reply = "Welcome to OmniSolo! This is a simple app that helps you manage your small business. You can set up your store, accept payments, and hire AI helpers.".to_string();
            link_url = DEFAULT_HELP_CHAT_LINKS[0].to_string();
        } else if query.contains("store") || query.contains("product") {
            reply = "To set up your storefront, go to the 'My Store' tab and add your products. It's easy! Just upload a photo, write a simple description, and set a price.".to_string();
            link_url = DEFAULT_HELP_CHAT_LINKS[1].to_string();
        } else if query.contains("payment") {
            reply = "When a customer buys something, the money goes straight to your account. We handle all the technical details so you can focus on your business.".to_string();
            link_url = DEFAULT_HELP_CHAT_LINKS[2].to_string();
        } else if query.contains("ai agent") {
            reply = "Need a hand? Your AI Support Agent can answer customer emails and chats for you while you sleep. Just turn it on in the 'AI Agents' tab.".to_string();
            link_url = DEFAULT_HELP_CHAT_LINKS[3].to_string();
        } else if query.contains("marketing") {
            reply = "Let our AI write your social media posts! Just tell it what you want to sell, and it will give you a catchy post to share with your customers.".to_string();
            link_url = DEFAULT_HELP_CHAT_LINKS[4].to_string();
        } else if query.contains("billing") {
            reply = "Your monthly invoice shows exactly what you paid for. We keep things simple with no hidden fees.".to_string();
            link_url = DEFAULT_HELP_CHAT_LINKS[5].to_string();
        } else if query.contains("api") || query.contains("advanced") {
            reply = "If you use custom tools, you can connect them to our system here. This is for advanced users.".to_string();
            link_url = DEFAULT_HELP_CHAT_LINKS[6].to_string();
        } else if query.contains("operations") {
            reply = "I have routed your request to the Operations department.".to_string();
            link_url = DEFAULT_HELP_CHAT_LINKS[7].to_string();
            link_title = "Check your inbox for updates →";
        }
    }

    Json(serde_json::json!({
        "reply": reply,
        "link": { "url": link_url, "title": link_title }
    }))
    .into_response()
}

pub(crate) async fn api_not_found_handler(req: Request) -> impl IntoResponse {
    if req.uri().path().starts_with("/api/") {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "api_not_found" })),
        )
            .into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

pub(crate) async fn list_workflows_handler(
    Extension(execution): Extension<Arc<crate::workflow_execution::WorkflowExecution>>,
    Query(query): Query<crate::workflow_execution::receipts::ReceiptQuery>,
    headers: axum::http::HeaderMap,
    claims: Option<Extension<::server_common::Claims>>,
) -> impl IntoResponse {
    let Some(claims) = claims else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"Authentication required"})),
        );
    };
    match execution.list_receipts(&claims.0, &headers, &query).await {
        Ok(receipts) => {
            let mut bytes = 256usize;
            let mut workflows = Vec::new();
            let mut last_cursor = None;
            let mut next_cursor = None;
            for receipt in receipts {
                let cursor = format!("{}:{}", receipt.created_at, receipt.id);
                let value = serde_json::json!(WorkflowRecord::from(receipt));
                let size = serde_json::to_vec(&value)
                    .expect("workflow record serializes")
                    .len()
                    + 1;
                if workflows.len() >= query.limit.unwrap_or(20) as usize || bytes + size > 1_048_576
                {
                    if workflows.is_empty() {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(
                                serde_json::json!({"error":"Workflow receipt exceeds the supported response bound"}),
                            ),
                        );
                    }
                    next_cursor = last_cursor;
                    break;
                }
                bytes += size;
                workflows.push(value);
                last_cursor = Some(cursor);
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({"workflows":workflows,"next_cursor":next_cursor})),
            )
        }
        Err(error) => (
            error.status(),
            Json(serde_json::json!({"error":error.message()})),
        ),
    }
}

pub(crate) async fn create_workflow_handler(
    Extension(execution): Extension<Arc<crate::workflow_execution::WorkflowExecution>>,
    headers: axum::http::HeaderMap,
    claims: Option<Extension<::server_common::Claims>>,
    Json(payload): Json<CreateWorkflowRequest>,
) -> impl IntoResponse {
    let Some(claims) = claims else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"Authentication required"})),
        );
    };
    let request_id = match crate::workflow_execution::dispatch::request_id(&headers) {
        Ok(id) => id,
        Err(error) => {
            return (
                error.status(),
                Json(serde_json::json!({"error":error.message()})),
            );
        }
    };
    let request = crate::workflow_execution::receipts::RequestMetadata {
        request_id,
        name: payload.name.trim().into(),
        workflow: if payload.workflow.trim().is_empty() {
            "expert_task".into()
        } else {
            payload.workflow.trim().into()
        },
        requested_model: payload.model,
        agent_role: None,
    };
    let result = async {
        let reservation = execution
            .prepare(&claims.0, &headers, &payload.task, request)
            .await?;
        execution.dispatch(reservation, None).await
    }
    .await;
    match result {
        Ok(receipt) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"workflow":WorkflowRecord::from(receipt)})),
        ),
        Err(error) => (
            error.status(),
            Json(serde_json::json!({"error":error.message()})),
        ),
    }
}

pub(crate) async fn workflow_receipt_handler(
    Extension(execution): Extension<Arc<crate::workflow_execution::WorkflowExecution>>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    claims: Option<Extension<::server_common::Claims>>,
) -> impl IntoResponse {
    let Some(claims) = claims else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"Authentication required"})),
        );
    };
    match execution.get_receipt(&claims.0, &headers, &id).await {
        Ok(receipt) => (
            StatusCode::OK,
            Json(serde_json::json!({"workflow":WorkflowRecord::from(receipt)})),
        ),
        Err(error) => (
            error.status(),
            Json(serde_json::json!({"error":error.message()})),
        ),
    }
}

pub(crate) async fn workflow_request_receipt_handler(
    Extension(execution): Extension<Arc<crate::workflow_execution::WorkflowExecution>>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    claims: Option<Extension<::server_common::Claims>>,
) -> impl IntoResponse {
    let Some(claims) = claims else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"Authentication required"})),
        );
    };
    match execution
        .receipt_by_request_id(&claims.0, &headers, &id)
        .await
    {
        Ok(receipt) => (
            StatusCode::OK,
            Json(serde_json::json!({"workflow":WorkflowRecord::from(receipt)})),
        ),
        Err(error) => (
            error.status(),
            Json(serde_json::json!({"error":error.message()})),
        ),
    }
}

pub(crate) async fn cancel_workflow_handler(
    Extension(execution): Extension<Arc<crate::workflow_execution::WorkflowExecution>>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    claims: Option<Extension<::server_common::Claims>>,
) -> impl IntoResponse {
    let Some(claims) = claims else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"Authentication required"})),
        );
    };
    match execution.cancel_receipt(&claims.0, &headers, &id).await {
        Ok(receipt) => (
            StatusCode::OK,
            Json(serde_json::json!({"workflow":WorkflowRecord::from(receipt)})),
        ),
        Err(error) => (
            error.status(),
            Json(serde_json::json!({"error":error.message()})),
        ),
    }
}
