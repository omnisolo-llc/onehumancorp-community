// SPDX-License-Identifier: Apache-2.0
//! Composable `OhcServerBuilder` and runtime extension registry (`ohc_core::extensions`).

use std::sync::{Arc, OnceLock};

use ::ohc_core::defaults::{
    InProcessLocalTransport, LocalOwnerAuthPolicy, NoOpPlatformBilling, SingleTenantContext,
};
use ::ohc_core::extensions::{
    AuthPolicyProvider, BillingPlatformProvider, DeploymentRuntime, EditionCapabilities,
    StorageHandle, SyncTransportProvider, TenantContextProvider,
};
use ::server_utils::cache::DistributedCacheBackend;
use omnisolo_builtin_agent::mesh::transport::{ExternalMeshTransportFactory, MeshTransport};

use super::grpc_router::build_oss_grpc_router;
use super::http_routes::{ExtraRouterFn, RouterLayerFn, build_http_router};
use super::workers::spawn_oss_background_workers;

pub type ExtraGrpcFn = Arc<
    dyn Fn(&ServerContext, tonic::transport::server::Router) -> tonic::transport::server::Router
        + Send
        + Sync,
>;
pub type ExtraBackgroundFn = Arc<dyn Fn(&ServerContext) + Send + Sync>;

static DISTRIBUTED_CACHE: OnceLock<Option<Arc<dyn DistributedCacheBackend>>> = OnceLock::new();
static SYNC_TRANSPORT: OnceLock<Arc<dyn SyncTransportProvider>> = OnceLock::new();
static TENANT_CONTEXT: OnceLock<Arc<dyn TenantContextProvider>> = OnceLock::new();
static BILLING_PLATFORM: OnceLock<Arc<dyn BillingPlatformProvider>> = OnceLock::new();
static AUTH_POLICY: OnceLock<Arc<dyn AuthPolicyProvider>> = OnceLock::new();
static MULTI_TENANT_MODE: OnceLock<bool> = OnceLock::new();
pub(crate) static BUILTIN_AGENT_SERVICE: OnceLock<
    Arc<omnisolo_builtin_agent::service::AgentServiceImpl>,
> = OnceLock::new();

/// Installs active extension implementations globally for legacy handlers and tests.
pub fn install_runtime_extensions(
    edition: &EditionCapabilities,
    tenant_ctx: Arc<dyn TenantContextProvider>,
    auth_policy_provider: Arc<dyn AuthPolicyProvider>,
    billing: Arc<dyn BillingPlatformProvider>,
    transport: Arc<dyn SyncTransportProvider>,
    cache: Option<Arc<dyn DistributedCacheBackend>>,
) {
    let _ = MULTI_TENANT_MODE.set(edition.multi_tenant);
    let _ = TENANT_CONTEXT.set(tenant_ctx);
    let _ = AUTH_POLICY.set(auth_policy_provider);
    let _ = BILLING_PLATFORM.set(billing);
    let _ = SYNC_TRANSPORT.set(transport);
    let _ = DISTRIBUTED_CACHE.set(cache);
}

pub fn configured_multi_tenant() -> Option<bool> {
    MULTI_TENANT_MODE.get().copied()
}

pub fn distributed_cache() -> Option<Arc<dyn DistributedCacheBackend>> {
    DISTRIBUTED_CACHE.get().and_then(|inner| inner.clone())
}

pub fn sync_transport() -> Arc<dyn SyncTransportProvider> {
    SYNC_TRANSPORT
        .get_or_init(|| Arc::new(InProcessLocalTransport::default()))
        .clone()
}

pub fn tenant_context() -> Arc<dyn TenantContextProvider> {
    TENANT_CONTEXT
        .get_or_init(|| {
            Arc::new(
                SingleTenantContext::new("default_tenant")
                    .expect("default single-tenant workspace id must be valid"),
            )
        })
        .clone()
}

pub fn billing_platform() -> Arc<dyn BillingPlatformProvider> {
    BILLING_PLATFORM
        .get_or_init(|| Arc::new(NoOpPlatformBilling))
        .clone()
}

pub fn auth_policy() -> Arc<dyn AuthPolicyProvider> {
    AUTH_POLICY
        .get_or_init(|| {
            Arc::new(
                LocalOwnerAuthPolicy::new("default_tenant")
                    .expect("default single-tenant workspace id must be valid"),
            )
        })
        .clone()
}

/// Shared runtime services and extensions exposed to Cloud router/worker hooks.
pub struct ServerContext {
    pub db: Arc<crate::db::DB>,
    pub hub: Arc<crate::hub::Hub>,
    pub auth_store: Arc<::server_auth::Store>,
    pub portable_database: Option<Arc<crate::persistence::AppDatabase>>,
    pub catalog_repository: Option<Arc<crate::persistence::catalog::CatalogRepository>>,
    pub mesh_transport: Arc<dyn MeshTransport>,
    pub handoff_mesh: Arc<crate::orchestration::mesh::CentrifugeNode>,
    pub dept_orchestrator:
        Arc<crate::orchestration::departments::orchestrator::DepartmentOrchestrator>,
    pub semantic_router: Arc<crate::orchestration::router::SemanticRouter>,
    pub autodream_worker: Arc<crate::autodream::AutoDreamWorker>,
    pub dynamic_workflow_manager:
        Arc<crate::orchestration::dynamic_workflows::DynamicWorkflowManager>,
    pub workflow_execution: Arc<crate::workflow_execution::WorkflowExecution>,
    pub reverse_tunnel_server: crate::agents::mcp::proxy::server::ReverseTunnelServer,
    pub tenant_ctx: Arc<dyn TenantContextProvider>,
    pub auth_policy: Arc<dyn AuthPolicyProvider>,
    pub billing_platform: Arc<dyn BillingPlatformProvider>,
    pub sync_transport: Arc<dyn SyncTransportProvider>,
    pub deployment: Arc<dyn DeploymentRuntime>,
    pub edition: EditionCapabilities,
    pub shutdown: crate::runtime_shutdown::CancellationToken,
    pub legacy_sqlx_background_enabled: bool,
}

pub(crate) fn grpc_bind_host(standalone: bool) -> &'static str {
    if standalone { "127.0.0.1" } else { "0.0.0.0" }
}

pub(crate) fn grpc_tls_config_from_pem(
    standalone: bool,
    cert: Option<Vec<u8>>,
    key: Option<Vec<u8>>,
    client_ca: Option<Vec<u8>>,
) -> Result<Option<tonic::transport::ServerTlsConfig>, String> {
    if standalone {
        return Ok(None);
    }

    let require = |name: &str, value: Option<Vec<u8>>| {
        value
            .filter(|bytes| !bytes.is_empty())
            .ok_or_else(|| format!("{name} is required and must not be empty in cloud mode"))
    };
    let cert = require("OMNISOLO_GRPC_TLS_CERT_PATH", cert)?;
    let key = require("OMNISOLO_GRPC_TLS_KEY_PATH", key)?;
    let client_ca = require("OMNISOLO_GRPC_CLIENT_CA_PATH", client_ca)?;

    Ok(Some(
        tonic::transport::ServerTlsConfig::new()
            .identity(tonic::transport::Identity::from_pem(cert, key))
            .client_ca_root(tonic::transport::Certificate::from_pem(client_ca)),
    ))
}

pub(crate) fn grpc_tls_config_from_env(
    standalone: bool,
) -> Result<Option<tonic::transport::ServerTlsConfig>, std::io::Error> {
    if standalone {
        return Ok(None);
    }

    let read = |name: &str| -> Result<Vec<u8>, std::io::Error> {
        let path = std::env::var(name).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{name} is required in cloud mode"),
            )
        })?;
        if path.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{name} must not be empty in cloud mode"),
            ));
        }
        std::fs::read(&path).map_err(|error| {
            std::io::Error::new(error.kind(), format!("failed to read {name}: {error}"))
        })
    };

    grpc_tls_config_from_pem(
        false,
        Some(read("OMNISOLO_GRPC_TLS_CERT_PATH")?),
        Some(read("OMNISOLO_GRPC_TLS_KEY_PATH")?),
        Some(read("OMNISOLO_GRPC_CLIENT_CA_PATH")?),
    )
    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

pub(crate) async fn canonical_builder_pool(
    database: &crate::persistence::AppDatabase,
    configured_data: Option<&sqlx::PgPool>,
) -> Option<sqlx::PgPool> {
    let configured_data = configured_data?;
    match ::server_auth::commit_authority::canonical_pg_data_pool(
        database.connection(),
        configured_data,
        &[
            "builder_brand_toolboxes",
            "builder_sites",
            "builder_pages",
            "builder_blocks",
            "builder_publications",
            "builder_publication_work",
            "products",
        ],
    )
    .await
    {
        Ok(pool) => Some(pool),
        Err(error) => {
            tracing::warn!(
                error = %error,
                database_failure = std::error::Error::source(&error).is_some(),
                "builder private storage has no confirmed canonical binding; configured data retained"
            );
            None
        }
    }
}

struct ConfiguredWorkflowInference(
    Arc<omnisolo_builtin_agent::tenant_analysis::ConfiguredTextAnalysis>,
);

impl crate::workflow_execution::TextInference for ConfiguredWorkflowInference {
    fn prepare(
        &self,
        task: &str,
        policy: &crate::workflow_execution::AnalysisPolicy,
    ) -> Result<
        omnisolo_builtin_agent::tenant_analysis::PreparedTextAnalysis,
        crate::workflow_execution::AdmissionError,
    > {
        if policy.provider != self.0.provider()
            || policy.model != self.0.model()
            || policy.max_output_tokens != self.0.max_output_tokens()
        {
            return Err(crate::workflow_execution::AdmissionError::Unavailable);
        }
        self.0
            .prepare(task)
            .map_err(|_| crate::workflow_execution::AdmissionError::Unavailable)
    }

    fn infer<'a>(
        &'a self,
        input: &'a crate::workflow_execution::AdmittedAnalysis,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<crate::workflow_execution::InferenceResult, ()>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let service = self
                .0
                .bind(input.tenant_id(), input.actor_id())
                .map_err(|_| ())?;
            let result = service
                .analyze_prepared(input.prepared().ok_or(())?)
                .await?;
            let usage = result.usage;
            let cached = i64::from(usage.cache_read_input_tokens);
            let input_tokens = i64::from(usage.input_tokens);
            let output_tokens = i64::from(usage.output_tokens);
            let input_tokens = if input.policy().provider == "anthropic" {
                input_tokens.checked_add(cached).ok_or(())?
            } else {
                input_tokens
            };
            let counts = (input_tokens > 0
                && output_tokens > 0
                && cached >= 0
                && cached <= input_tokens
                && usage.cache_creation_input_tokens == 0)
                .then_some(server_harness::middleware::usage_ledger::TokenCounts {
                    input: input_tokens,
                    output: output_tokens,
                    cached_input: cached,
                });
            Ok(crate::workflow_execution::InferenceResult {
                output: result.output,
                provider_request_id: result.response_id.filter(|id| {
                    !id.trim().is_empty() && id.len() <= 255 && !id.chars().any(char::is_control)
                }),
                counts,
            })
        })
    }
}

pub(crate) fn configured_workflow_execution(
    store: Arc<::server_auth::Store>,
    database: crate::persistence::AppDatabase,
) -> Arc<crate::workflow_execution::WorkflowExecution> {
    use crate::workflow_execution::{AnalysisPolicy, WorkflowExecution};
    let configured =
        omnisolo_builtin_agent::tenant_analysis::ConfiguredTextAnalysis::from_environment()
            .ok()
            .and_then(|client| {
                let policy = AnalysisPolicy::new(
                    client.provider().into(),
                    client.model().into(),
                    client.max_output_tokens(),
                )
                .ok()?;
                let funding = crate::workflow_execution::funding::FundingContext::new(
                    crate::workflow_execution::funding::FundingPolicy::from_environment(&policy)
                        .ok()?,
                    &database,
                )
                .ok()?;
                Some((
                    policy,
                    funding,
                    Arc::new(ConfiguredWorkflowInference(Arc::new(client)))
                        as Arc<dyn crate::workflow_execution::TextInference>,
                ))
            });
    Arc::new(
        (match configured {
            Some((policy, funding, client)) => {
                WorkflowExecution::configured_funded(store, policy, client, funding)
            }
            None => WorkflowExecution::unavailable(store),
        })
        .with_receipts(database),
    )
}

/// Composable OneHumanCorp server builder shared by open-source standalone and
/// proprietary Cloud binaries without duplicating routing, storage, or worker wiring.
pub struct OhcServerBuilder {
    deployment: Arc<dyn DeploymentRuntime>,
    tenant_ctx: Arc<dyn TenantContextProvider>,
    auth_policy: Arc<dyn AuthPolicyProvider>,
    billing_platform: Arc<dyn BillingPlatformProvider>,
    sync_transport: Arc<dyn SyncTransportProvider>,
    distributed_cache: Option<Arc<dyn DistributedCacheBackend>>,
    mesh_url: Option<String>,
    mesh_factory: Option<Arc<dyn ExternalMeshTransportFactory>>,
    extra_task_queue: Option<Arc<dyn crate::queue::TaskQueue>>,
    extra_protected_http_routes: Option<ExtraRouterFn>,
    extra_tier_layer: Option<RouterLayerFn>,
    extra_public_http_routes: Option<ExtraRouterFn>,
    extra_grpc_services: Option<ExtraGrpcFn>,
    extra_background: Option<ExtraBackgroundFn>,
}

impl OhcServerBuilder {
    pub fn new(
        deployment: Arc<dyn DeploymentRuntime>,
        tenant_ctx: Arc<dyn TenantContextProvider>,
        auth_policy: Arc<dyn AuthPolicyProvider>,
        billing_platform: Arc<dyn BillingPlatformProvider>,
        sync_transport: Arc<dyn SyncTransportProvider>,
    ) -> Self {
        Self {
            deployment,
            tenant_ctx,
            auth_policy,
            billing_platform,
            sync_transport,
            distributed_cache: None,
            mesh_url: None,
            mesh_factory: None,
            extra_task_queue: None,
            extra_protected_http_routes: None,
            extra_tier_layer: None,
            extra_public_http_routes: None,
            extra_grpc_services: None,
            extra_background: None,
        }
    }

    pub fn with_distributed_cache(
        mut self,
        cache: Option<Arc<dyn DistributedCacheBackend>>,
    ) -> Self {
        self.distributed_cache = cache;
        self
    }

    pub fn with_mesh_transport(
        mut self,
        mesh_url: Option<String>,
        factory: Option<Arc<dyn ExternalMeshTransportFactory>>,
    ) -> Self {
        self.mesh_url = mesh_url;
        self.mesh_factory = factory;
        self
    }

    pub fn with_task_queue(mut self, queue: Arc<dyn crate::queue::TaskQueue>) -> Self {
        self.extra_task_queue = Some(queue);
        self
    }

    pub fn with_extra_protected_http_routes(mut self, f: ExtraRouterFn) -> Self {
        self.extra_protected_http_routes = Some(f);
        self
    }

    pub fn with_extra_tier_layer(mut self, f: RouterLayerFn) -> Self {
        self.extra_tier_layer = Some(f);
        self
    }

    pub fn with_extra_public_http_routes(mut self, f: ExtraRouterFn) -> Self {
        self.extra_public_http_routes = Some(f);
        self
    }

    pub fn with_extra_grpc_services(mut self, f: ExtraGrpcFn) -> Self {
        self.extra_grpc_services = Some(f);
        self
    }

    pub fn with_extra_background(mut self, f: ExtraBackgroundFn) -> Self {
        self.extra_background = Some(f);
        self
    }

    async fn resolve_database(&self) -> Result<Arc<crate::db::DB>, Box<dyn std::error::Error>> {
        let env_configured = std::env::var("OMNISOLO_DATABASE_URL").is_ok()
            || std::env::var("DATABASE_URL").is_ok()
            || std::env::var("OMNISOLO_SQLITE_KEY").is_ok();
        let db = if env_configured {
            let db = Arc::new(crate::db::DB::new().await?);
            let handle = match &db.store {
                crate::db::DbStore::Postgres => StorageHandle::Postgres(db.pool.clone()),
                crate::db::DbStore::Sqlite(pool) => StorageHandle::Sqlite(pool.clone()),
            };
            self.deployment
                .run_migrations(&handle)
                .await
                .map_err(std::io::Error::other)?;
            db
        } else {
            let handle = self
                .deployment
                .init_storage()
                .await
                .map_err(std::io::Error::other)?;
            self.deployment
                .run_migrations(&handle)
                .await
                .map_err(std::io::Error::other)?;
            Arc::new(crate::db::DB::from_storage_handle(&handle)?)
        };
        db.run_migrations().await?;
        Ok(db)
    }

    pub async fn serve_with_graceful_shutdown(
        self,
        shutdown: crate::runtime_shutdown::CancellationToken,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let edition = self.deployment.edition_capabilities();
        let is_cloud = edition.multi_tenant;
        let standalone = !is_cloud;

        install_runtime_extensions(
            &edition,
            self.tenant_ctx.clone(),
            self.auth_policy.clone(),
            self.billing_platform.clone(),
            self.sync_transport.clone(),
            self.distributed_cache.clone(),
        );

        let grpc_tls_config = grpc_tls_config_from_env(standalone)?;
        let builtin_agent_auth = if standalone {
            Some(
                omnisolo_builtin_agent::auth::auth_mode_from_env().map_err(|error| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("invalid builtin agent authentication configuration: {error}"),
                    )
                })?,
            )
        } else {
            None
        };

        let db = self.resolve_database().await?;
        let portable_database = match &db.store {
            crate::db::DbStore::Sqlite(pool) => Some(Arc::new(
                crate::persistence::AppDatabase::from_storage_handle(&StorageHandle::Sqlite(
                    pool.clone(),
                )),
            )),
            crate::db::DbStore::Postgres => {
                if let Some(pg_pool) = db.postgres_pool() {
                    Some(Arc::new(
                        crate::persistence::AppDatabase::from_storage_handle(
                            &StorageHandle::Postgres(pg_pool.clone()),
                        ),
                    ))
                } else {
                    match crate::persistence::commands::connect_from_environment().await {
                        Ok(database) => Some(Arc::new(database)),
                        Err(error) if standalone => {
                            tracing::warn!(
                                "portable database is unavailable in standalone mode: {error}"
                            );
                            None
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        };
        if let Some(database) = portable_database.as_ref() {
            crate::persistence::migration::migrate(database).await?;
        }
        let catalog_repository = portable_database.as_ref().map(|database| {
            Arc::new(crate::persistence::catalog::CatalogRepository::new(
                database.as_ref().clone(),
            ))
        });
        let legacy_sqlx_background_enabled = true;

        let grpc_port = std::env::var("OMNISOLO_GRPC_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(8081);
        let addr = format!("{}:{}", grpc_bind_host(standalone), grpc_port).parse()?;
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(100);
        let hub_inner = crate::hub::AppContext::new_with_extensions(
            event_tx,
            db.pool.clone(),
            self.tenant_ctx.clone(),
            self.auth_policy.clone(),
            self.billing_platform.clone(),
            self.sync_transport.clone(),
            self.distributed_cache.clone(),
        );
        hub_inner.set_db(db.clone());
        let hub = Arc::new(hub_inner);

        let mesh_factory_ref = self.mesh_factory.as_deref();
        let mesh_transport =
            omnisolo_builtin_agent::mesh::transport::create_transport_for_startup_with_factory(
                self.mesh_url.as_deref(),
                is_cloud,
                mesh_factory_ref,
            )
            .await
            .map_err(std::io::Error::other)?;

        let handoff_mesh = Arc::new(crate::orchestration::mesh::CentrifugeNode::new(
            mesh_transport.clone(),
        ));
        let dept_orchestrator = Arc::new(
            crate::orchestration::departments::orchestrator::DepartmentOrchestrator::new(
                db.clone(),
                handoff_mesh.clone(),
            ),
        );
        let semantic_router = Arc::new(crate::orchestration::router::SemanticRouter::new());
        let autodream_worker = Arc::new(crate::autodream::AutoDreamWorker::new(db.clone()));

        let auth_database = portable_database
            .as_ref()
            .ok_or_else(|| std::io::Error::other("portable authentication database unavailable"))?;
        let auth_repo = Arc::new(crate::auth::seaorm_store::SeaOrmAuthRepository::new(
            auth_database.connection().clone(),
        ));
        let http_auth_store = Arc::new(crate::auth::Store::with_portable_repo(auth_repo));
        http_auth_store.register_active_jwt_secret();

        let dynamic_workflow_queue: Arc<dyn crate::queue::TaskQueue> = match &db.store {
            crate::db::DbStore::Postgres => {
                Arc::new(crate::queue::PostgresTaskQueue::new(db.pool.clone()))
            }
            crate::db::DbStore::Sqlite(sqlite_pool) => {
                let queue = crate::queue::SqliteTaskQueue::new(sqlite_pool.clone());
                queue.init().await?;
                Arc::new(queue)
            }
        };
        let dynamic_workflow_state_dir = std::env::var("OMNISOLO_DYNAMIC_WORKFLOW_STATE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from(".omnisolo/dynamic-workflows"));
        let dynamic_workflow_manager = Arc::new(
            crate::orchestration::dynamic_workflows::DynamicWorkflowManager::with_state_dir(
                dynamic_workflow_queue,
                dynamic_workflow_state_dir,
            ),
        );
        let workflow_execution =
            configured_workflow_execution(http_auth_store.clone(), auth_database.as_ref().clone());
        let reverse_tunnel_server =
            crate::agents::mcp::proxy::server::ReverseTunnelServer::new(Arc::new(db.pool.clone()));

        let ctx = ServerContext {
            db: db.clone(),
            hub: hub.clone(),
            auth_store: http_auth_store.clone(),
            portable_database: portable_database.clone(),
            catalog_repository,
            mesh_transport: mesh_transport.clone(),
            handoff_mesh: handoff_mesh.clone(),
            dept_orchestrator: dept_orchestrator.clone(),
            semantic_router,
            autodream_worker: autodream_worker.clone(),
            dynamic_workflow_manager,
            workflow_execution,
            reverse_tunnel_server: reverse_tunnel_server.clone(),
            tenant_ctx: self.tenant_ctx,
            auth_policy: self.auth_policy.clone(),
            billing_platform: self.billing_platform,
            sync_transport: self.sync_transport.clone(),
            deployment: self.deployment.clone(),
            edition: edition.clone(),
            shutdown: shutdown.clone(),
            legacy_sqlx_background_enabled,
        };

        spawn_oss_background_workers(
            &ctx,
            self.extra_task_queue,
            builtin_agent_auth,
            &mut event_rx,
        )
        .await?;

        if let Some(bg_fn) = self.extra_background {
            bg_fn(&ctx);
        }

        let app = build_http_router(
            &ctx,
            self.extra_protected_http_routes.as_ref(),
            self.extra_tier_layer.as_ref(),
            self.extra_public_http_routes.as_ref(),
        )
        .await?;

        let (listener, mesh_addr) = crate::runtime_shutdown::bind_http_listener(standalone).await?;
        tracing::info!("Server listening on {}", addr);

        let mut grpc_server = tonic::transport::Server::builder();
        if let Some(tls_config) = grpc_tls_config {
            grpc_server = grpc_server.tls_config(tls_config)?;
        }
        let mut grpc_router = build_oss_grpc_router(
            grpc_server,
            ctx.db.clone(),
            ctx.hub.clone(),
            ctx.auth_store.clone(),
            ctx.auth_policy.clone(),
            ctx.sync_transport.clone(),
            ctx.dept_orchestrator.clone(),
            reverse_tunnel_server,
        );
        grpc_router = ctx.deployment.mount_extra_grpc_services(grpc_router);
        if let Some(extra_grpc) = self.extra_grpc_services {
            grpc_router = extra_grpc(&ctx, grpc_router);
        }

        let grpc_shutdown = shutdown.clone();
        let grpc_future = grpc_router.serve_with_shutdown(addr, async move {
            grpc_shutdown.cancelled().await;
        });

        crate::runtime_shutdown::serve_http_and_grpc(
            listener,
            mesh_addr,
            app,
            grpc_future,
            shutdown,
        )
        .await?;
        Ok(())
    }
}
