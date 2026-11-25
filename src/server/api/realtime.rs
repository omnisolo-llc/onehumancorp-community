use axum::{
    Json, Router,
    extract::{
        Extension, FromRequestParts,
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    },
    http::{Request, StatusCode},
    response::IntoResponse,
    routing::get,
};
use futures::{sink::SinkExt, stream::StreamExt};
use jsonwebtoken::{DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use uuid::Uuid;

const MAX_CONSUMED_JTIS: usize = 65_536;
const TICKET_TTL_SECS: i64 = 60;
const MAX_TICKET_TTL_SECS: i64 = 300;

// Bounded TTL-evicting tracker for single-use tickets (jti -> exp unix timestamp).
static CONSUMED_JTIS: std::sync::LazyLock<Arc<RwLock<HashMap<String, i64>>>> =
    std::sync::LazyLock::new(|| Arc::new(RwLock::new(HashMap::new())));

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RealtimeTicketClaims {
    pub jti: String,
    pub user_id: String,
    pub tenant_id: String,
    pub exp: i64,
}

#[derive(Debug, Deserialize)]
pub struct TicketResponse {
    pub ticket: String,
    pub expires_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct PresenceEvent {
    pub r#type: String, // e.g. "typing_start", "typing_stop", "user_presence"
    pub conversation_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PresenceBroadcast {
    pub r#type: String,
    pub tenant_id: String,
    pub conversation_id: String,
    pub user_id: String,
    pub ts: i64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action")]
pub enum RealtimeClientMessage {
    #[serde(rename = "subscribe")]
    Subscribe { conversation_id: String },
    #[serde(rename = "unsubscribe")]
    Unsubscribe { conversation_id: String },
    #[serde(rename = "presence")]
    Presence(PresenceEvent),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum RealtimeServerMessage {
    #[serde(rename = "subscribed")]
    Subscribed {
        conversation_id: String,
        status: String,
    },
    #[serde(rename = "unsubscribed")]
    Unsubscribed {
        conversation_id: String,
        status: String,
    },
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(rename = "presence")]
    Presence {
        r#type: String,
        conversation_id: String,
        user_id: String,
        ts: i64,
    },
}

fn current_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn get_jwt_secret() -> Result<Vec<u8>, String> {
    ::server_auth::resolve_configured_jwt_secret()
}

fn consume_jti(
    store: &mut HashMap<String, i64>,
    jti: &str,
    exp: i64,
    now: i64,
    max_entries: usize,
) -> Result<(), StatusCode> {
    store.retain(|_, entry_exp| *entry_exp > now);
    if store.contains_key(jti) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if store.len() >= max_entries {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    store.insert(jti.to_string(), exp);
    Ok(())
}

fn parse_strict_tenant_uuid(tenant_id: &str) -> Option<Uuid> {
    Uuid::parse_str(tenant_id.trim())
        .ok()
        .filter(|uuid| !uuid.is_nil())
}

pub async fn realtime_ticket_handler(
    Extension(claims): Extension<server_common::Claims>,
) -> impl IntoResponse {
    let Some(tenant_id) = ::server_common::auth_utils::signed_tenant_id(&claims) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "authentication required" })),
        )
            .into_response();
    };
    if claims.sub.trim().is_empty() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "authentication required" })),
        )
            .into_response();
    }

    let secret = match get_jwt_secret() {
        Ok(secret) => secret,
        Err(_) => {
            tracing::error!("realtime ticket generation failed: JWT secret not configured");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "realtime authentication unavailable" })),
            )
                .into_response();
        }
    };

    let now = current_unix_secs();
    let expires_at = now + TICKET_TTL_SECS;

    let ticket_claims = RealtimeTicketClaims {
        jti: Uuid::new_v4().to_string(),
        user_id: claims.sub.clone(),
        tenant_id,
        exp: expires_at,
    };

    let key = EncodingKey::from_secret(&secret);
    let header = Header::new(jsonwebtoken::Algorithm::HS256);
    match encode(&header, &ticket_claims, &key) {
        Ok(ticket) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ticket": ticket,
                "expires_at": expires_at
            })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to generate ticket" })),
        )
            .into_response(),
    }
}

pub async fn realtime_ws_handler(req: Request<axum::body::Body>) -> impl IntoResponse {
    let Some(middleware_claims) = req.extensions().get::<server_common::Claims>().cloned() else {
        return (StatusCode::UNAUTHORIZED, "Authentication required").into_response();
    };
    let Some(expected_tenant_id) =
        ::server_common::auth_utils::signed_tenant_id(&middleware_claims)
    else {
        return (StatusCode::UNAUTHORIZED, "Invalid tenant context").into_response();
    };

    // Protocol header: Sec-WebSocket-Protocol
    let subprotocol = req
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok());

    let ticket = subprotocol.and_then(|sub| {
        sub.split(',')
            .map(|s| s.trim())
            .find(|s| s.starts_with("ohc-rt-ticket-"))
            .map(|s| s.strip_prefix("ohc-rt-ticket-").unwrap_or(s).to_string())
    });

    let Some(ticket_str) = ticket else {
        return (
            StatusCode::UNAUTHORIZED,
            "Missing single-use ticket in WebSocket subprotocol",
        )
            .into_response();
    };

    let secret = match get_jwt_secret() {
        Ok(secret) => secret,
        Err(_) => {
            tracing::error!("realtime ws auth failed: JWT secret not configured");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Realtime authentication unavailable",
            )
                .into_response();
        }
    };

    let key = DecodingKey::from_secret(&secret);
    let mut validation = Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = true;

    let claims: RealtimeTicketClaims =
        match decode::<RealtimeTicketClaims>(&ticket_str, &key, &validation) {
            Ok(token_data) => token_data.claims,
            Err(_) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    "Invalid or expired ticket signature",
                )
                    .into_response();
            }
        };

    let now = current_unix_secs();
    if claims.jti.trim().is_empty()
        || claims.user_id != middleware_claims.sub
        || claims.tenant_id != expected_tenant_id
        || claims.exp <= now
        || claims.exp > now + MAX_TICKET_TTL_SECS
    {
        return (
            StatusCode::UNAUTHORIZED,
            "Ticket does not match authenticated session",
        )
            .into_response();
    }

    // Authenticate the single-use ticket before Axum validates the upgrade
    // headers, so missing or invalid credentials consistently fail with 401.
    let (mut parts, _) = req.into_parts();
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };

    // Verify single-use of jti with bounded TTL eviction
    {
        let mut consumed = CONSUMED_JTIS.write().unwrap_or_else(|e| e.into_inner());
        match consume_jti(
            &mut consumed,
            &claims.jti,
            claims.exp,
            now,
            MAX_CONSUMED_JTIS,
        ) {
            Ok(()) => {}
            Err(StatusCode::UNAUTHORIZED) => {
                return (StatusCode::UNAUTHORIZED, "Ticket has already been consumed")
                    .into_response();
            }
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Realtime ticket capacity exhausted",
                )
                    .into_response();
            }
        }
    }

    let returned_protocol = format!("ohc-rt-ticket-{}", ticket_str);
    ws.protocols(vec![returned_protocol])
        .on_upgrade(move |socket| handle_realtime_socket(socket, claims))
}

// Global broadcast channel for presence and routing across connected sessions
static REALTIME_BROADCAST: std::sync::OnceLock<broadcast::Sender<String>> =
    std::sync::OnceLock::new();

fn get_realtime_broadcast_tx() -> &'static broadcast::Sender<String> {
    REALTIME_BROADCAST.get_or_init(|| {
        let (tx, _) = broadcast::channel(8192);
        tx
    })
}

async fn handle_realtime_socket(socket: WebSocket, claims: RealtimeTicketClaims) {
    let (mut sender, mut receiver) = socket.split();
    let (ws_tx, mut ws_rx) = tokio::sync::mpsc::channel::<String>(512);

    let tx = get_realtime_broadcast_tx();
    let mut broadcast_rx = tx.subscribe();

    let tenant_id = claims.tenant_id.clone();
    let user_id = claims.user_id.clone();

    // Set of conversations subscribed by this connection session
    let subscribed_conversations = Arc::new(RwLock::new(HashSet::new()));

    // Task to forward websocket messages from inner queue to client
    let send_task = tokio::spawn(async move {
        while let Some(msg) = ws_rx.recv().await {
            if sender.send(WsMessage::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Task to receive global broadcast events and forward if matching tenant/conv subscription
    let subscribed_convs_clone = subscribed_conversations.clone();
    let ws_tx_clone = ws_tx.clone();
    let user_id_ps = user_id.clone();
    let tenant_id_ps = tenant_id.clone();
    let broadcast_task = tokio::spawn(async move {
        loop {
            match broadcast_rx.recv().await {
                Ok(raw) => {
                    if let Ok(event) = serde_json::from_str::<PresenceBroadcast>(&raw) {
                        if event.tenant_id != tenant_id_ps {
                            continue;
                        }
                        let subscribed = {
                            let convs = subscribed_convs_clone
                                .read()
                                .unwrap_or_else(|e| e.into_inner());
                            convs.contains(&event.conversation_id)
                        };
                        if subscribed && event.user_id != user_id_ps {
                            let server_msg = RealtimeServerMessage::Presence {
                                r#type: event.r#type,
                                conversation_id: event.conversation_id,
                                user_id: event.user_id,
                                ts: event.ts,
                            };
                            if let Ok(serialized) = serde_json::to_string(&server_msg) {
                                let _ = ws_tx_clone.send(serialized).await;
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Task to handle inbound commands from the client
    let ws_tx_recv = ws_tx.clone();
    let tenant_id_recv = tenant_id.clone();
    let user_id_recv = claims.user_id.clone();
    let pool_opt = crate::db::get_pool_opt();

    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                WsMessage::Text(text) => {
                    if let Ok(client_msg) = serde_json::from_str::<RealtimeClientMessage>(&text) {
                        match client_msg {
                            RealtimeClientMessage::Subscribe { conversation_id } => {
                                // Multi-tenant dynamic check: Validate conversation belongs to the tenant.
                                let mut valid = true;
                                if !crate::is_standalone_runtime() {
                                    if let (Some(pool), Ok(parsed_uuid), Some(parsed_tenant_uuid)) = (
                                        pool_opt.as_ref(),
                                        Uuid::parse_str(&conversation_id),
                                        parse_strict_tenant_uuid(&tenant_id_recv),
                                    ) {
                                        let exists_result: Result<Option<Uuid>, sqlx::Error> =
                                            async {
                                                let mut tx = pool.begin().await?;
                                                ::server_common::auth_utils::set_org_context(
                                                    &mut *tx,
                                                    &tenant_id_recv,
                                                )
                                                .await?;
                                                let row = sqlx::query_scalar(
                                                    "SELECT id FROM chat_conversations WHERE id = $1 AND tenant_id = $2",
                                                )
                                                .bind(parsed_uuid)
                                                .bind(parsed_tenant_uuid)
                                                .fetch_optional(&mut *tx)
                                                .await?;
                                                let _ = tx.rollback().await;
                                                Ok(row)
                                            }
                                            .await;

                                        if let Ok(None) | Err(_) = exists_result {
                                            valid = false;
                                        }
                                    } else {
                                        // Fail closed on missing DB, invalid conversation UUID, or non-UUID/nil tenant ID
                                        valid = false;
                                    }
                                }

                                if !valid {
                                    let err_msg = RealtimeServerMessage::Error {
                                        message:
                                            "Subscription denied: Unauthorized conversation access"
                                                .to_string(),
                                    };
                                    let _ = ws_tx_recv
                                        .send(serde_json::to_string(&err_msg).unwrap_or_default())
                                        .await;
                                    continue;
                                }

                                {
                                    let mut convs = subscribed_conversations
                                        .write()
                                        .unwrap_or_else(|e| e.into_inner());
                                    convs.insert(conversation_id.clone());
                                }

                                let response = RealtimeServerMessage::Subscribed {
                                    conversation_id,
                                    status: "ok".to_string(),
                                };
                                let _ = ws_tx_recv
                                    .send(serde_json::to_string(&response).unwrap_or_default())
                                    .await;
                            }
                            RealtimeClientMessage::Unsubscribe { conversation_id } => {
                                {
                                    let mut convs = subscribed_conversations
                                        .write()
                                        .unwrap_or_else(|e| e.into_inner());
                                    convs.remove(&conversation_id);
                                }
                                let response = RealtimeServerMessage::Unsubscribed {
                                    conversation_id,
                                    status: "ok".to_string(),
                                };
                                let _ = ws_tx_recv
                                    .send(serde_json::to_string(&response).unwrap_or_default())
                                    .await;
                            }
                            RealtimeClientMessage::Presence(event) => {
                                // Validate subscription before broadcasting to prevent spoofing
                                let subscribed = {
                                    let convs = subscribed_conversations
                                        .read()
                                        .unwrap_or_else(|e| e.into_inner());
                                    convs.contains(&event.conversation_id)
                                };

                                if subscribed {
                                    let broadcast_payload = PresenceBroadcast {
                                        r#type: event.r#type,
                                        tenant_id: tenant_id_recv.clone(),
                                        conversation_id: event.conversation_id,
                                        user_id: user_id_recv.clone(),
                                        ts: chrono::Utc::now().timestamp_millis(),
                                    };
                                    if let Ok(serialized) =
                                        serde_json::to_string(&broadcast_payload)
                                    {
                                        let _ = tx.send(serialized);
                                    }
                                }
                            }
                        }
                    }
                }
                WsMessage::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = send_task => {}
        _ = broadcast_task => {}
        _ = recv_task => {}
    };
}

pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new().route("/api/v1/realtime/ws", get(realtime_ws_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tower::ServiceExt;

    fn test_session_claims(user_id: &str, tenant_id: &str) -> server_common::Claims {
        server_common::Claims {
            sub: user_id.to_string(),
            username: "maya".to_string(),
            email: "maya@baker.com".to_string(),
            roles: vec![],
            organization_id: Some(tenant_id.to_string()),
            session_id: None,
            iat: 0,
            exp: current_unix_secs() + 3600,
            jti: "0".to_string(),
        }
    }

    #[test]
    fn test_ticket_claims_serialization() {
        let claims = RealtimeTicketClaims {
            jti: "test-jti".to_string(),
            user_id: "test-user".to_string(),
            tenant_id: "test-tenant".to_string(),
            exp: 1234567890,
        };
        let serialized = serde_json::to_string(&claims).unwrap();
        let deserialized: RealtimeTicketClaims = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.jti, "test-jti");
        assert_eq!(deserialized.user_id, "test-user");
        assert_eq!(deserialized.tenant_id, "test-tenant");
        assert_eq!(deserialized.exp, 1234567890);
    }

    #[test]
    fn strict_tenant_uuid_rejects_non_uuid_and_nil() {
        assert!(parse_strict_tenant_uuid("not-a-uuid").is_none());
        assert!(parse_strict_tenant_uuid("").is_none());
        assert!(parse_strict_tenant_uuid(&Uuid::nil().to_string()).is_none());
        let valid = Uuid::new_v4();
        assert_eq!(parse_strict_tenant_uuid(&valid.to_string()), Some(valid));
    }

    #[test]
    fn consumed_jti_cache_evicts_expired_and_bounds_capacity() {
        let mut store = HashMap::new();
        assert_eq!(consume_jti(&mut store, "jti-1", 100, 90, 2), Ok(()));
        assert_eq!(
            consume_jti(&mut store, "jti-1", 100, 95, 2),
            Err(StatusCode::UNAUTHORIZED)
        );
        assert_eq!(consume_jti(&mut store, "jti-2", 110, 95, 2), Ok(()));
        // Capacity full while both are unexpired -> fail closed with 503
        assert_eq!(
            consume_jti(&mut store, "jti-3", 120, 95, 2),
            Err(StatusCode::SERVICE_UNAVAILABLE)
        );
        // Once jti-1 expires at t=100, it is evicted and jti-3 is accepted
        assert_eq!(consume_jti(&mut store, "jti-3", 120, 100, 2), Ok(()));
        assert!(!store.contains_key("jti-1"));
        assert!(store.contains_key("jti-3"));
    }

    static REALTIME_ENV_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    #[tokio::test]
    async fn realtime_fails_closed_when_secret_unconfigured_and_supports_secret_file() {
        let _env_guard = REALTIME_ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let secret_file = dir.path().join("jwt-secret");
        let file_secret = "file-backed-realtime-secret-at-least-32-bytes";
        std::fs::write(&secret_file, file_secret).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&secret_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        // Unconfigured secret (with missing standalone file) must fail closed with 503
        let empty_home = tempfile::tempdir().unwrap();
        temp_env::async_with_vars(
            [
                ("JWT_SECRET", None::<&str>),
                ("JWT_SECRET_FILE", None::<&str>),
                ("OMNISOLO_JWT_SECRET", None::<&str>),
                ("OMNISOLO_JWT_SECRET_FILE", None::<&str>),
                (
                    "OMNISOLO_DATA_DIR",
                    Some(empty_home.path().to_str().unwrap()),
                ),
                ("HOME", Some(empty_home.path().to_str().unwrap())),
            ],
            async {
                let claims = test_session_claims("user-123", "tenant-abc");
                let app = Router::new()
                    .route(
                        "/api/v1/auth/realtime-ticket",
                        post(realtime_ticket_handler),
                    )
                    .layer(Extension(claims));
                let res = app
                    .oneshot(
                        Request::post("/api/v1/auth/realtime-ticket")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
            },
        )
        .await;

        // JWT_SECRET_FILE loads cleanly and signs valid tickets
        temp_env::async_with_vars(
            [
                ("JWT_SECRET", None::<&str>),
                ("JWT_SECRET_FILE", Some(secret_file.to_str().unwrap())),
                ("OMNISOLO_JWT_SECRET", None::<&str>),
                ("OMNISOLO_JWT_SECRET_FILE", None::<&str>),
            ],
            async {
                assert_eq!(get_jwt_secret().unwrap(), file_secret.as_bytes());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn test_realtime_ticket_handler_success() {
        let _env_guard = REALTIME_ENV_LOCK.lock().await;
        let claims = test_session_claims("user-123", "tenant-abc");
        let secret = "realtime-test-secret-with-at-least-32-bytes";

        temp_env::async_with_vars(
            [
                ("JWT_SECRET", Some(secret)),
                ("JWT_SECRET_FILE", None::<&str>),
            ],
            async {
                let app = Router::new()
                    .route(
                        "/api/v1/auth/realtime-ticket",
                        post(realtime_ticket_handler),
                    )
                    .layer(Extension(claims));

                let response = app
                    .oneshot(
                        Request::builder()
                            .uri("/api/v1/auth/realtime-ticket")
                            .method("POST")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();

                assert_eq!(response.status(), StatusCode::OK);
                let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let parsed: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
                assert!(parsed.get("ticket").is_some());
                assert!(parsed.get("expires_at").is_some());
            },
        )
        .await;
    }

    #[tokio::test]
    async fn test_realtime_ws_unauthorized_and_session_mismatch_cases() {
        let _env_guard = REALTIME_ENV_LOCK.lock().await;
        let secret = "realtime-ws-auth-secret-at-least-32-bytes";
        temp_env::async_with_vars(
            [
                ("JWT_SECRET", Some(secret)),
                ("JWT_SECRET_FILE", None::<&str>),
            ],
            async {
                // 0. Missing middleware Claims must be rejected even with a signed ticket
                let ticket_for_a = encode(
                    &Header::new(jsonwebtoken::Algorithm::HS256),
                    &RealtimeTicketClaims {
                        jti: Uuid::new_v4().to_string(),
                        user_id: "user-123".to_string(),
                        tenant_id: "tenant-abc".to_string(),
                        exp: current_unix_secs() + 60,
                    },
                    &EncodingKey::from_secret(secret.as_bytes()),
                )
                .unwrap();

                let no_claims_app =
                    Router::new().route("/api/v1/realtime/ws", get(realtime_ws_handler));
                let res0 = no_claims_app
                    .oneshot(
                        Request::builder()
                            .uri("/api/v1/realtime/ws")
                            .method("GET")
                            .header(
                                "sec-websocket-protocol",
                                format!("ohc-rt-ticket-{ticket_for_a}"),
                            )
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(res0.status(), StatusCode::UNAUTHORIZED);

                let app = Router::new()
                    .route("/api/v1/realtime/ws", get(realtime_ws_handler))
                    .layer(Extension(test_session_claims("user-123", "tenant-abc")));

                // 1. Missing header
                let res1 = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri("/api/v1/realtime/ws")
                            .method("GET")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(res1.status(), StatusCode::UNAUTHORIZED);

                // 2. Invalid ticket string
                let res2 = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .uri("/api/v1/realtime/ws")
                            .method("GET")
                            .header("sec-websocket-protocol", "ohc-rt-ticket-invalidtoken")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(res2.status(), StatusCode::UNAUTHORIZED);

                // 3. Cross-tenant ticket signed with valid secret must be rejected against middleware Claims
                let cross_tenant_ticket = encode(
                    &Header::new(jsonwebtoken::Algorithm::HS256),
                    &RealtimeTicketClaims {
                        jti: Uuid::new_v4().to_string(),
                        user_id: "user-123".to_string(),
                        tenant_id: "tenant-victim".to_string(),
                        exp: current_unix_secs() + 60,
                    },
                    &EncodingKey::from_secret(secret.as_bytes()),
                )
                .unwrap();
                let res3 = app
                    .oneshot(
                        Request::builder()
                            .uri("/api/v1/realtime/ws")
                            .method("GET")
                            .header(
                                "sec-websocket-protocol",
                                format!("ohc-rt-ticket-{cross_tenant_ticket}"),
                            )
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(res3.status(), StatusCode::UNAUTHORIZED);
            },
        )
        .await;
    }

    #[tokio::test]
    async fn test_realtime_ws_consumed_ticket_rejections() {
        let _env_guard = REALTIME_ENV_LOCK.lock().await;
        let secret = "realtime-consumed-secret-at-least-32-bytes";
        temp_env::async_with_vars(
            [
                ("JWT_SECRET", Some(secret)),
                ("JWT_SECRET_FILE", None::<&str>),
            ],
            async {
                let claims = RealtimeTicketClaims {
                    jti: Uuid::new_v4().to_string(),
                    user_id: "user-123".to_string(),
                    tenant_id: "tenant-abc".to_string(),
                    exp: current_unix_secs() + 60,
                };

                let key = EncodingKey::from_secret(secret.as_bytes());
                let header = Header::new(jsonwebtoken::Algorithm::HS256);
                let ticket_str = encode(&header, &claims, &key).unwrap();

                let app = Router::new()
                    .route("/api/v1/realtime/ws", get(realtime_ws_handler))
                    .layer(Extension(test_session_claims("user-123", "tenant-abc")));
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                    .await
                    .unwrap();
                let address = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    axum::serve(listener, app).await.unwrap();
                });

                let upgrade_request = || {
                    let mut request = format!("ws://{address}/api/v1/realtime/ws")
                        .into_client_request()
                        .unwrap();
                    request.headers_mut().insert(
                        "sec-websocket-protocol",
                        format!("ohc-rt-ticket-{ticket_str}").parse().unwrap(),
                    );
                    request
                };

                let (socket, res1) = tokio_tungstenite::connect_async(upgrade_request())
                    .await
                    .unwrap();

                assert_eq!(res1.status(), StatusCode::SWITCHING_PROTOCOLS);
                drop(socket);

                // Second attempt with the SAME consumed ticket
                let error = tokio_tungstenite::connect_async(upgrade_request())
                    .await
                    .expect_err("a consumed ticket must not establish another WebSocket");

                match error {
                    tokio_tungstenite::tungstenite::Error::Http(response) => {
                        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
                    }
                    other => panic!("expected an HTTP authentication rejection, got {other}"),
                }
                server.abort();
            },
        )
        .await;
    }
}
