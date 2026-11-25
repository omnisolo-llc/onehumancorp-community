// SPDX-License-Identifier: Apache-2.0
// Copyright (c) OmniSolo LLC.

use axum::body::Body;
use axum::http::Request;
use ohc_core::extensions::{
    AuthCapabilities, AuthPolicyProvider, AuthzError, BillingPlatformProvider,
    CommunityEditionCapabilities, DeploymentRuntime, FailClosedPeerVerifier,
    InProcessLocalTransport, LocalOwnerAuthPolicy, NoOpPlatformBilling, OidcVerificationConfig,
    OidcVerificationError, OidcVerifier, PeerIdentityVerifier, QuotaDecision, SingleTenantContext,
    SingleTenantRuntime, StorageHandle, SyncTransportProvider, TenantContextProvider,
    UnconfiguredOidcVerifier, WorkspaceScope,
};

#[test]
fn single_tenant_context_fails_closed_on_invalid_config_and_mismatched_scope() {
    assert!(SingleTenantContext::new("").is_err());
    assert!(SingleTenantContext::new("   ").is_err());
    assert!(SingleTenantContext::new("system").is_err());
    assert!(SingleTenantContext::new("SYSTEM").is_err());

    let ctx = SingleTenantContext::new("workspace-solo-1").unwrap();
    assert_eq!(ctx.workspace_id(), "workspace-solo-1");

    let unauth_req = Request::builder().uri("/").body(Body::empty()).unwrap();
    assert!(matches!(
        ctx.resolve_http_scope(&unauth_req),
        Err(AuthzError::Unauthenticated(_))
    ));

    let mut empty_actor_req = Request::builder().uri("/").body(Body::empty()).unwrap();
    empty_actor_req.extensions_mut().insert(WorkspaceScope {
        workspace_id: "workspace-solo-1".to_string(),
        actor_id: "   ".to_string(),
        roles: vec!["OWNER".to_string()],
    });
    assert!(matches!(
        ctx.resolve_http_scope(&empty_actor_req),
        Err(AuthzError::Unauthenticated(_))
    ));

    let mut wrong_ws_req = Request::builder().uri("/").body(Body::empty()).unwrap();
    wrong_ws_req.extensions_mut().insert(WorkspaceScope {
        workspace_id: "workspace-other".to_string(),
        actor_id: "owner-1".to_string(),
        roles: vec!["OWNER".to_string()],
    });
    assert!(matches!(
        ctx.resolve_http_scope(&wrong_ws_req),
        Err(AuthzError::Forbidden(_))
    ));

    let mut valid_req = Request::builder().uri("/").body(Body::empty()).unwrap();
    valid_req.extensions_mut().insert(WorkspaceScope {
        workspace_id: "workspace-solo-1".to_string(),
        actor_id: "owner-1".to_string(),
        roles: vec!["OWNER".to_string()],
    });
    let resolved = ctx.resolve_http_scope(&valid_req).unwrap();
    assert_eq!(resolved.workspace_id, "workspace-solo-1");
    assert_eq!(resolved.actor_id, "owner-1");
}

#[tokio::test]
async fn local_owner_auth_policy_enforces_hmac_peer_auth_and_revocation() {
    assert!(LocalOwnerAuthPolicy::new("").is_err());
    assert!(LocalOwnerAuthPolicy::new("system").is_err());

    let unconfigured = LocalOwnerAuthPolicy::new("ws-local").unwrap();
    let req = tonic::Request::new(());
    assert_eq!(
        unconfigured.verify_grpc_peer(&req).unwrap_err().code(),
        tonic::Code::Unauthenticated
    );
    assert!(unconfigured.extra_auth_routes().is_none());
    assert!(unconfigured.oidc_verifier().is_none());
    assert_eq!(
        unconfigured.capabilities(),
        AuthCapabilities {
            local_password: true,
            api_keys: true,
            oidc_sso: false,
            mtls_peer_auth: false,
        }
    );

    // Short key must fail closed.
    assert!(
        LocalOwnerAuthPolicy::new("ws-local")
            .unwrap()
            .with_hmac_token("secret", b"too-short")
            .is_err()
    );

    let key = b"0123456789abcdef0123456789abcdef";
    let policy = LocalOwnerAuthPolicy::new("ws-local")
        .unwrap()
        .with_hmac_token("local-agent-token", key)
        .unwrap();

    let mut wrong_req = tonic::Request::new(());
    wrong_req
        .metadata_mut()
        .insert("authorization", "Bearer wrong-token".parse().unwrap());
    assert_eq!(
        policy.verify_grpc_peer(&wrong_req).unwrap_err().code(),
        tonic::Code::Unauthenticated
    );

    let mut valid_req = tonic::Request::new(());
    valid_req
        .metadata_mut()
        .insert("authorization", "Bearer local-agent-token".parse().unwrap());
    let peer = policy.verify_grpc_peer(&valid_req).unwrap();
    assert_eq!(peer.workspace_id.as_deref(), Some("ws-local"));
    assert_eq!(peer.auth_scheme, "hmac-bearer");

    let scope = WorkspaceScope {
        workspace_id: "ws-local".to_string(),
        actor_id: "owner-1".to_string(),
        roles: vec!["OWNER".to_string()],
    };
    assert!(!policy.is_jti_revoked("jti-1", &scope).await.unwrap());
    policy.revoke_jti("jti-1");
    assert!(policy.is_jti_revoked("jti-1", &scope).await.unwrap());
    assert!(policy.is_jti_revoked("   ", &scope).await.is_err());

    let wrong_scope = WorkspaceScope {
        workspace_id: "ws-other".to_string(),
        actor_id: "owner-1".to_string(),
        roles: vec![],
    };
    assert!(policy.is_jti_revoked("jti-1", &wrong_scope).await.is_err());
}

#[tokio::test]
async fn noop_platform_billing_and_oidc_fail_closed_defaults() {
    let billing = NoOpPlatformBilling;
    assert!(billing.platform_billing_routes().is_none());

    let empty_scope = WorkspaceScope {
        workspace_id: " ".to_string(),
        actor_id: "owner-1".to_string(),
        roles: vec![],
    };
    assert!(
        billing
            .check_platform_quota(&empty_scope, "chat")
            .await
            .is_err()
    );

    let valid_scope = WorkspaceScope {
        workspace_id: "ws-local".to_string(),
        actor_id: "owner-1".to_string(),
        roles: vec!["OWNER".to_string()],
    };
    assert!(
        billing
            .check_platform_quota(&valid_scope, "")
            .await
            .is_err()
    );
    let decision = billing
        .check_platform_quota(&valid_scope, "chat.completion")
        .await
        .unwrap();
    assert_eq!(decision, QuotaDecision::allow());

    let oidc = UnconfiguredOidcVerifier;
    let cfg = OidcVerificationConfig {
        issuer_url: "https://idp.example.test".to_string(),
        client_id: "client-1".to_string(),
        enabled: true,
    };
    assert_eq!(
        oidc.verify_id_token("token", &cfg).await.unwrap_err(),
        OidcVerificationError::Unavailable
    );

    let peer_verifier = FailClosedPeerVerifier::default();
    assert_eq!(
        peer_verifier
            .verify_peer(&tonic::Request::new(()))
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
}

#[tokio::test]
async fn in_process_local_transport_publishes_and_locks_with_raii_release() {
    let transport = InProcessLocalTransport::default();
    assert!(transport.extra_sync_routes().is_none());
    assert!(
        transport
            .publish_event("   ", serde_json::json!({"ok": true}))
            .await
            .is_err()
    );
    assert!(transport.acquire_lock("", 10).await.is_err());
    assert!(transport.acquire_lock("inv:1", 0).await.is_err());

    let mut rx = transport.subscribe("orders");
    transport
        .publish_event("orders", serde_json::json!({"order_id": "ord-1"}))
        .await
        .unwrap();
    let event = rx.recv().await.unwrap();
    assert_eq!(event["order_id"], "ord-1");

    let guard = transport
        .acquire_lock("inventory:sku-1", 30)
        .await
        .unwrap()
        .expect("initial lock must succeed");
    assert_eq!(guard.key(), "inventory:sku-1");
    assert!(
        transport
            .acquire_lock("inventory:sku-1", 30)
            .await
            .unwrap()
            .is_none(),
        "held lock must deny second acquisition"
    );
    drop(guard);

    let reacquired = transport
        .acquire_lock("inventory:sku-1", 30)
        .await
        .unwrap()
        .expect("lock must be re-acquirable after guard drop");
    reacquired.release();
}

#[tokio::test]
async fn community_edition_runtime_initializes_sqlite_and_advertises_oss_capabilities() {
    let caps = CommunityEditionCapabilities::capabilities();
    assert_eq!(caps.edition, "community");
    assert!(!caps.multi_tenant);
    assert!(!caps.sso_oidc);
    assert!(!caps.platform_billing);
    assert!(!caps.cloud_sync);
    assert!(!caps.collectives);

    assert!(SingleTenantRuntime::new("  ").is_err());
    let runtime = SingleTenantRuntime::new("sqlite::memory:").unwrap();
    assert_eq!(runtime.edition_capabilities(), caps);
    let storage = runtime.init_storage().await.unwrap();
    assert!(matches!(storage, StorageHandle::Sqlite(_)));
    runtime.run_migrations(&storage).await.unwrap();
}
