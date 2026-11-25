// SPDX-License-Identifier: Apache-2.0
//! Dynamic runtime capabilities endpoint (`/api/v1/capabilities`).

use std::sync::Arc;

use axum::{Json, Router, extract::State, routing::get};
use ohc_core::extensions::{AuthPolicyProvider, DeploymentRuntime};
use serde::Serialize;

#[derive(Clone)]
struct CapabilitiesState {
    deployment: Arc<dyn DeploymentRuntime>,
    auth_policy: Arc<dyn AuthPolicyProvider>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FeatureCapabilitiesPayload {
    pub cloud_sync: bool,
    pub oidc: bool,
    pub saas_billing: bool,
    pub multitenant: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CapabilitiesResponsePayload {
    pub edition: String,
    pub features: FeatureCapabilitiesPayload,
}

async fn get_capabilities_handler(
    State(state): State<CapabilitiesState>,
) -> Json<CapabilitiesResponsePayload> {
    let edition = state.deployment.edition_capabilities();
    let auth = state.auth_policy.capabilities();
    Json(CapabilitiesResponsePayload {
        edition: edition.edition,
        features: FeatureCapabilitiesPayload {
            cloud_sync: edition.cloud_sync,
            oidc: edition.sso_oidc || auth.oidc_sso,
            saas_billing: edition.platform_billing,
            multitenant: edition.multi_tenant,
        },
    })
}

/// Mounts `GET /api/v1/capabilities` driven dynamically by the active
/// [`DeploymentRuntime`] and [`AuthPolicyProvider`].
pub fn capabilities_router(
    deployment: Arc<dyn DeploymentRuntime>,
    auth_policy: Arc<dyn AuthPolicyProvider>,
) -> Router {
    Router::new()
        .route("/api/v1/capabilities", get(get_capabilities_handler))
        .with_state(CapabilitiesState {
            deployment,
            auth_policy,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, http::StatusCode};
    use ohc_core::defaults::{LocalOwnerAuthPolicy, SingleTenantRuntime};
    use ohc_core::extensions::EditionCapabilities;
    use tower::ServiceExt;

    #[derive(Debug)]
    struct CustomCloudCapabilitiesRuntime;

    #[async_trait::async_trait]
    impl DeploymentRuntime for CustomCloudCapabilitiesRuntime {
        fn edition_capabilities(&self) -> EditionCapabilities {
            EditionCapabilities {
                edition: "cloud".to_string(),
                multi_tenant: true,
                sso_oidc: true,
                platform_billing: true,
                cloud_sync: true,
                collectives: true,
            }
        }

        async fn init_storage(
            &self,
        ) -> Result<ohc_core::extensions::StorageHandle, ohc_core::extensions::RuntimeError>
        {
            Err(ohc_core::extensions::RuntimeError::InvalidConfiguration(
                "not used in capabilities test".to_string(),
            ))
        }

        async fn run_migrations(
            &self,
            _storage: &ohc_core::extensions::StorageHandle,
        ) -> Result<(), ohc_core::extensions::RuntimeError> {
            Ok(())
        }

        fn mount_extra_http_routes(&self) -> Router {
            Router::new()
        }

        fn mount_extra_grpc_services(
            &self,
            router: tonic::transport::server::Router,
        ) -> tonic::transport::server::Router {
            router
        }
    }

    #[tokio::test]
    async fn capabilities_reflect_community_runtime() {
        let runtime = Arc::new(SingleTenantRuntime::new("sqlite::memory:").unwrap());
        let auth = Arc::new(LocalOwnerAuthPolicy::new("default_tenant").unwrap());
        let app = capabilities_router(runtime, auth);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/capabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1024 * 16)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["edition"], "community");
        assert_eq!(json["features"]["cloud_sync"], false);
        assert_eq!(json["features"]["oidc"], false);
        assert_eq!(json["features"]["saas_billing"], false);
        assert_eq!(json["features"]["multitenant"], false);
    }

    #[tokio::test]
    async fn capabilities_reflect_injected_cloud_runtime() {
        let runtime = Arc::new(CustomCloudCapabilitiesRuntime);
        let auth = Arc::new(LocalOwnerAuthPolicy::new("default_tenant").unwrap());
        let app = capabilities_router(runtime, auth);

        let res = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/capabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1024 * 16)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["edition"], "cloud");
        assert_eq!(json["features"]["cloud_sync"], true);
        assert_eq!(json["features"]["oidc"], true);
        assert_eq!(json["features"]["saas_billing"], true);
        assert_eq!(json["features"]["multitenant"], true);
    }
}
