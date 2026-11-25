// SPDX-License-Identifier: Apache-2.0
//! Policy-backed gRPC peer interceptor and OSS gRPC router assembly.

use std::sync::Arc;

use ::ohc_core::extensions::{AuthPolicyProvider, SyncTransportProvider};
use ::server_auth::orchestration::AuthInfo;
use ::server_omnisolo::orchestration::growth_service_server::GrowthServiceServer;
use ::server_omnisolo::orchestration::hub_service_server::HubServiceServer;
use tonic::{Request, Status};

use super::grpc_services::MyHubService;

/// Tonic interceptor delegating gRPC peer authentication to [`AuthPolicyProvider`].
#[derive(Clone)]
pub struct PolicyPeerInterceptor {
    policy: Arc<dyn AuthPolicyProvider>,
}

impl PolicyPeerInterceptor {
    pub fn new(policy: Arc<dyn AuthPolicyProvider>) -> Self {
        Self { policy }
    }
}

impl tonic::service::Interceptor for PolicyPeerInterceptor {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        req.extensions_mut()
            .remove::<::ohc_core::extensions::PeerIdentity>();
        req.extensions_mut().remove::<AuthInfo>();

        let peer = self.policy.verify_grpc_peer(&req)?;
        let org_id = peer
            .workspace_id
            .clone()
            .unwrap_or_else(|| "default_tenant".to_string());
        let auth_info = AuthInfo {
            org_id,
            agent_id: peer.principal.clone(),
            spiffe_id: format!("{}://{}", peer.auth_scheme, peer.principal),
        };
        req.extensions_mut().insert(peer);
        req.extensions_mut().insert(auth_info);
        Ok(req)
    }
}

/// Mounts all single-tenant OSS gRPC services protected by [`PolicyPeerInterceptor`].
#[allow(clippy::too_many_arguments)]
pub fn build_oss_grpc_router(
    mut grpc_server: tonic::transport::Server,
    db: Arc<crate::db::DB>,
    hub: Arc<crate::hub::Hub>,
    auth_store: Arc<::server_auth::Store>,
    auth_policy: Arc<dyn AuthPolicyProvider>,
    sync_transport: Arc<dyn SyncTransportProvider>,
    dept_orchestrator: Arc<crate::orchestration::departments::orchestrator::DepartmentOrchestrator>,
    reverse_tunnel_server: crate::agents::mcp::proxy::server::ReverseTunnelServer,
) -> tonic::transport::server::Router {
    let interceptor = PolicyPeerInterceptor::new(auth_policy.clone());
    let viral_loop_tracker = Arc::new(crate::services::growth::viral_loop::ViralLoopTracker::new());
    let hub_service = MyHubService::new(
        hub.clone(),
        db.pool.clone(),
        dept_orchestrator,
        viral_loop_tracker,
    );
    let growth_service =
        crate::services::growth::service::MyGrowthService::new(db.pool.clone(), hub.clone());
    let dashboard_service =
        crate::services::dashboard::service::MyDashboardService::new(db.clone(), hub.clone());
    let inventory_sync_service =
        crate::services::inventory_sync::MyInventorySyncService::new(Some(sync_transport.clone()));

    grpc_server
        .add_service(HubServiceServer::with_interceptor(
            hub_service,
            interceptor.clone(),
        ))
        .add_service(
            ::server_omnisolo::mcp_proxy::mcp_reverse_tunnel_service_server::McpReverseTunnelServiceServer::with_interceptor(
                reverse_tunnel_server,
                interceptor.clone(),
            ),
        )
        .add_service(
            ::server_omnisolo::orchestration::auth_service_server::AuthServiceServer::new(
                ::server_auth::AuthServiceServerImpl::new(auth_store, auth_policy),
            ),
        )
        .add_service(GrowthServiceServer::with_interceptor(
            growth_service,
            interceptor.clone(),
        ))
        .add_service(
            ::server_omnisolo::app::dashboard_service_server::DashboardServiceServer::with_interceptor(
                dashboard_service,
                interceptor.clone(),
            ),
        )
        .add_service(
            ::server_omnisolo::orchestration::agent_manager_service_server::AgentManagerServiceServer::with_interceptor(
                crate::services::agent::service::MyAgentManagerService::new(hub),
                interceptor.clone(),
            ),
        )
        .add_service(
            ::server_omnisolo::app::booking_engine_service_server::BookingEngineServiceServer::with_interceptor(
                crate::services::booking::NativeBookingService {
                    sync_transport: Some(sync_transport),
                },
                interceptor.clone(),
            ),
        )
        .add_service(
            ::server_omnisolo::app::pos_service_server::PosServiceServer::with_interceptor(
                crate::services::pos::service::MyPosService::new(db),
                interceptor.clone(),
            ),
        )
        .add_service(
            ::server_omnisolo::inventory::inventory_sync_service_server::InventorySyncServiceServer::with_interceptor(
                inventory_sync_service,
                interceptor,
            ),
        )
}
