use ::server_omnisolo::inventory::inventory_sync_service_server::InventorySyncService;
use ::server_omnisolo::inventory::{
    CommitInventoryRequest, CommitInventoryResponse, ReserveInventoryRequest,
    ReserveInventoryResponse,
};
use std::sync::Arc;
use tonic::{Request, Response, Status};

pub struct MyInventorySyncService {
    sync_transport: Option<Arc<dyn ::ohc_core::extensions::SyncTransportProvider>>,
}

impl MyInventorySyncService {
    pub fn new(
        sync_transport: Option<Arc<dyn ::ohc_core::extensions::SyncTransportProvider>>,
    ) -> Self {
        Self { sync_transport }
    }
}

#[tonic::async_trait]
impl InventorySyncService for MyInventorySyncService {
    async fn reserve_inventory(
        &self,
        request: Request<ReserveInventoryRequest>,
    ) -> Result<Response<ReserveInventoryResponse>, Status> {
        let (tenant_id, _) = ::server_auth::orchestration::extract_grpc_workspace(&request)?;

        let req = request.into_inner();
        let service =
            crate::services::inventory::InventoryService::new(self.sync_transport.clone());

        match service
            .reserve_inventory(&tenant_id, &req.product_id, req.quantity, req.ttl_seconds)
            .await
        {
            Ok(result) => Ok(Response::new(ReserveInventoryResponse {
                success: result.success,
                lock_id: result.lock_id,
                error_message: result.error_message,
            })),
            Err(e) => Err(Status::internal(e)),
        }
    }

    async fn commit_inventory(
        &self,
        request: Request<CommitInventoryRequest>,
    ) -> Result<Response<CommitInventoryResponse>, Status> {
        let (tenant_id, _) = ::server_auth::orchestration::extract_grpc_workspace(&request)?;

        let req = request.into_inner();
        let service =
            crate::services::inventory::InventoryService::new(self.sync_transport.clone());

        match service
            .commit_inventory(&tenant_id, &req.product_id, req.quantity, &req.lock_id)
            .await
        {
            Ok(result) => Ok(Response::new(CommitInventoryResponse {
                success: result.success,
                error_message: result.error_message,
            })),
            Err(e) => Err(Status::internal(e)),
        }
    }
}
