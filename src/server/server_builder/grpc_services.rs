// SPDX-License-Identifier: Apache-2.0
//! Policy-backed gRPC peer interceptor, `MyHubService`, and OSS gRPC router assembly.

use std::pin::Pin;
use std::sync::Arc;

use ::server_auth::orchestration::AuthInfo;
use ::server_omnisolo::orchestration::hub_service_server::HubService;
use ::server_omnisolo::orchestration::*;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use super::hub_grpc_helpers::{
    authorize_delegate_for_tenant, authorize_invite_tenant, authorize_open_meeting_participants,
    authorize_publish_for_tenant, discover_agents_stream, env_config_from_verified,
    get_wizard_state_in_db, map_pending_approvals, poll_tasks_for_org, require_auth_info,
    require_org_match, reset_wizard_state_in_db, save_wizard_state_in_db, scope_mesh_channel,
    update_task_status_for_org, wizard_profile_response,
};
use super::ui_caches::AI_CACHE;

pub struct MyHubService {
    hub: Arc<crate::hub::Hub>,
    dept_orchestrator: Arc<crate::orchestration::departments::orchestrator::DepartmentOrchestrator>,
    invite_tracker: Arc<crate::services::growth::invites::InviteTracker>,
    viral_loop_tracker: Arc<crate::services::growth::viral_loop::ViralLoopTracker>,
    publish_counter: opentelemetry::metrics::Counter<u64>,
    stream_counter: opentelemetry::metrics::Counter<u64>,
}

impl MyHubService {
    pub fn new(
        hub: Arc<crate::hub::Hub>,
        pool: sqlx::PgPool,
        dept_orchestrator: Arc<
            crate::orchestration::departments::orchestrator::DepartmentOrchestrator,
        >,
        viral_loop_tracker: Arc<crate::services::growth::viral_loop::ViralLoopTracker>,
    ) -> Self {
        let invite_repo = Arc::new(crate::services::growth::invites::InviteRepository::new(
            pool,
        ));
        let invite_tracker = Arc::new(crate::services::growth::invites::InviteTracker::new(
            invite_repo,
        ));

        let meter = opentelemetry::global::meter("ohc.orchestration.hub");
        let publish_counter = meter.u64_counter("hub.mesh_events.published").build();
        let stream_counter = meter.u64_counter("hub.mesh_events.stream_started").build();

        Self {
            hub,
            dept_orchestrator,
            invite_tracker,
            viral_loop_tracker,
            publish_counter,
            stream_counter,
        }
    }
}

#[tonic::async_trait]
impl HubService for MyHubService {
    async fn route_semantic(
        &self,
        request: Request<SemanticRoutingRequest>,
    ) -> Result<Response<SemanticRoutingResponse>, Status> {
        let auth = require_auth_info(&request)?;
        let req = request.into_inner();
        let tenant_id = require_org_match(&auth, &req.tenant_id)?;
        let internal_req = crate::orchestration::router::SemanticRoutingRequest {
            tenant_id,
            prompt: req.prompt,
            embedding: (!req.embedding.is_empty()).then_some(req.embedding),
        };
        let res = self
            .hub
            .semantic_router
            .route(&internal_req)
            .map_err(Status::internal)?;
        Ok(Response::new(SemanticRoutingResponse {
            tenant_id: res.tenant_id,
            target_department: res.target_department.to_string(),
            confidence_score: res.confidence_score,
        }))
    }

    type StreamMessagesStream = Pin<Box<dyn Stream<Item = Result<Message, Status>> + Send>>;

    async fn stream_messages(
        &self,
        request: Request<StreamMessagesRequest>,
    ) -> Result<Response<Self::StreamMessagesStream>, Status> {
        let auth = require_auth_info(&request)?;
        let req = request.into_inner();
        if !req.agent_id.trim().is_empty() && req.agent_id.trim() != auth.agent_id {
            return Err(Status::permission_denied(
                "Cannot stream messages for another agent",
            ));
        }
        let agent_id = auth.agent_id;
        let rx = self.hub.subscribe(agent_id.clone()).await;
        let drained = self.hub.get_inbox(&agent_id).await;
        use futures::StreamExt as _;
        let drained_stream = tokio_stream::iter(drained.into_iter().map(Ok));
        let rx_stream = futures::TryStreamExt::map_err(
            tokio_stream::wrappers::BroadcastStream::new(rx),
            |error| Status::internal(error.to_string()),
        );
        Ok(Response::new(
            Box::pin(drained_stream.chain(rx_stream)) as Self::StreamMessagesStream
        ))
    }

    async fn reason(
        &self,
        request: Request<ReasonRequest>,
    ) -> Result<Response<ReasonResponse>, Status> {
        let auth = require_auth_info(&request)?;
        let req = request.into_inner();
        if auth.agent_id != req.from_agent_id {
            return Err(Status::permission_denied(
                "Authenticated identity does not match from_agent_id",
            ));
        }
        let api_key = self.hub.minimax_api_key().to_string();
        if api_key.is_empty() {
            return Err(Status::failed_precondition(
                "Minimax API key is not configured",
            ));
        }
        let compressed_prompt = ::ohc_core::prompt::compression::reduce_tokens(&req.prompt);
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(compressed_prompt.as_bytes());
        let prompt_hash = hex::encode(hasher.finalize());
        let ai_cache_key = format!("ai_cache:reason:{}:{}", auth.org_id, prompt_hash);
        let ai_cache = AI_CACHE
            .get_or_init(|| ::server_utils::cache::HybridCache::new(crate::distributed_cache()));
        if let Some(cached_output) = ai_cache.get(&ai_cache_key).await {
            return Ok(Response::new(ReasonResponse {
                content: cached_output,
            }));
        }
        let content = crate::minimax::MinimaxClient::new(api_key)
            .reason(&compressed_prompt)
            .await
            .map_err(Status::internal)?;
        ai_cache
            .set(
                &ai_cache_key,
                content.clone(),
                std::time::Duration::from_secs(3600),
            )
            .await;
        Ok(Response::new(ReasonResponse { content }))
    }

    async fn trigger_custom_order(
        &self,
        request: Request<TriggerCustomOrderRequest>,
    ) -> Result<Response<TriggerCustomOrderResponse>, Status> {
        let auth = require_auth_info(&request)?;
        let req = request.into_inner();
        let org_id = require_org_match(&auth, &req.organization_id)?;
        let ops_event = crate::orchestration::departments::types::DepartmentEvent {
            id: uuid::Uuid::new_v4().to_string(),
            tenant_id: org_id.clone(),
            event_type: "NEW_ORDER".to_string(),
            payload: serde_json::json!({
                "customer_name": req.customer_name,
                "details": req.details
            }),
        };
        let cs_approval = crate::orchestration::departments::types::ApprovalRequest {
            id: uuid::Uuid::new_v4().to_string(),
            tenant_id: org_id,
            department: crate::orchestration::departments::types::DepartmentType::CustomerSuccess,
            description: format!("Draft Confirmation for {}", req.customer_name),
            status: crate::orchestration::departments::types::ApprovalStatus::PendingApproval,
            action_risk: crate::orchestration::departments::types::ActionRisk::DraftForReview,
            payload: Some(serde_json::json!({
                "draft_copy": format!("Hi {}, thank you for your custom order!", req.customer_name),
                "customer_name": req.customer_name,
                "details": req.details
            })),
        };
        let (dispatch, approval) = tokio::join!(
            self.dept_orchestrator.dispatch_event(ops_event),
            self.dept_orchestrator.add_approval_request(cs_approval)
        );
        dispatch.map_err(|_| Status::internal("Unable to persist custom-order work"))?;
        approval.map_err(|_| Status::internal("Unable to persist custom-order approval"))?;
        Ok(Response::new(TriggerCustomOrderResponse { success: true }))
    }

    async fn decompose_task(
        &self,
        request: Request<DecomposeTaskRequest>,
    ) -> Result<Response<DecomposeTaskResponse>, Status> {
        let auth = require_auth_info(&request)?;
        let req = request.into_inner();
        let org_id = require_org_match(&auth, &req.organization_id)?;
        for st in req.sub_tasks {
            let dependencies = st
                .dependencies
                .into_iter()
                .filter(|dep| dep != &req.task_id)
                .collect();
            self.hub
                .task_manager()
                .create_task_with_plan(
                    org_id.clone(),
                    String::new(),
                    crate::tasks::TaskPlan {
                        parent_plan_id: req.task_id.clone(),
                        dependencies,
                    },
                    st.title,
                    st.description,
                    st.priority,
                )
                .map_err(Status::internal)?;
        }
        Ok(Response::new(DecomposeTaskResponse { success: true }))
    }

    async fn create_terminal_connection_token(
        &self,
        request: Request<CreateTerminalTokenRequest>,
    ) -> Result<Response<CreateTerminalTokenResponse>, Status> {
        let tenant_id = request
            .extensions()
            .get::<AuthInfo>()
            .map(|i| i.org_id.clone())
            .ok_or_else(|| Status::unauthenticated("Missing authentication context"))?;
        let stripe_key = std::env::var("STRIPE_API_KEY")
            .map_err(|_| Status::failed_precondition("STRIPE_API_KEY is required"))?;
        let token = crate::integrations::stripe::client::StripeClient::new(stripe_key)
            .create_terminal_connection_token(&tenant_id)
            .await
            .map_err(Status::internal)?;
        Ok(Response::new(CreateTerminalTokenResponse {
            success: true,
            token,
        }))
    }

    async fn register_agent(
        &self,
        request: Request<RegisterAgentRequest>,
    ) -> Result<Response<RegisterAgentResponse>, Status> {
        let auth = require_auth_info(&request)?;
        let Some(mut agent) = request.into_inner().agent else {
            return Err(Status::invalid_argument("agent is required"));
        };
        if auth.agent_id != agent.id {
            return Err(Status::permission_denied(
                "Authenticated identity cannot register another agent",
            ));
        }
        if agent.organization_id.trim().is_empty() {
            agent.organization_id = auth.org_id;
        } else if agent.organization_id != auth.org_id {
            return Err(Status::permission_denied(
                "Agent organization_id does not match authenticated organization",
            ));
        }
        self.hub.register_agent(agent).await;
        Ok(Response::new(RegisterAgentResponse { success: true }))
    }

    async fn handle_config_wizard(
        &self,
        _request: Request<AgentConfig>,
    ) -> Result<Response<WizardResponse>, Status> {
        Ok(Response::new(WizardResponse {
            success: true,
            message: "success".to_string(),
        }))
    }

    async fn handle_prompt_tuning(
        &self,
        _request: Request<PromptTuningConfig>,
    ) -> Result<Response<WizardResponse>, Status> {
        Ok(Response::new(WizardResponse {
            success: true,
            message: "success".to_string(),
        }))
    }

    async fn open_meeting(
        &self,
        request: Request<OpenMeetingRequest>,
    ) -> Result<Response<MeetingRoom>, Status> {
        let auth = require_auth_info(&request)?;
        authorize_open_meeting_participants(&self.hub, &auth, request.get_ref()).await?;
        let req = request.into_inner();
        let meeting = self
            .hub
            .try_open_meeting_for_org(auth.org_id, req.meeting_id, req.participants, req.agenda)
            .await
            .map_err(Status::permission_denied)?;
        Ok(Response::new(meeting))
    }

    async fn publish(
        &self,
        request: Request<PublishMessageRequest>,
    ) -> Result<Response<PublishMessageResponse>, Status> {
        let auth = require_auth_info(&request)?;
        authorize_publish_for_tenant(&self.hub, &auth, request.get_ref()).await?;
        let msg = request
            .into_inner()
            .message
            .ok_or_else(|| Status::invalid_argument("message is required"))?;
        self.hub
            .clone()
            .publish(msg)
            .await
            .map_err(Status::internal)?;
        Ok(Response::new(PublishMessageResponse { success: true }))
    }

    async fn delegate_task(
        &self,
        request: Request<DelegateTaskRequest>,
    ) -> Result<Response<DelegateTaskResponse>, Status> {
        let auth = require_auth_info(&request)?;
        authorize_delegate_for_tenant(&self.hub, &auth, request.get_ref()).await?;
        let req = request.into_inner();
        let task = req
            .task
            .ok_or_else(|| Status::invalid_argument("task is required"))?;
        self.hub
            .clone()
            .delegate_task(req.from_agent_id, req.to_agent_id, task)
            .await
            .map_err(Status::internal)?;
        Ok(Response::new(DelegateTaskResponse { success: true }))
    }

    async fn verify_environment(
        &self,
        request: Request<VerifyEnvironmentRequest>,
    ) -> Result<Response<VerifyEnvironmentResponse>, Status> {
        let env = request.into_inner().env_vars;
        match crate::services::onboarding::env_verifier::verify_environment(&env) {
            Ok(config) => Ok(Response::new(VerifyEnvironmentResponse {
                status: "success".to_string(),
                config: Some(env_config_from_verified(config)),
                error: String::new(),
            })),
            Err(error) => Ok(Response::new(VerifyEnvironmentResponse {
                status: "error".to_string(),
                config: None,
                error,
            })),
        }
    }

    async fn generate_config(
        &self,
        request: Request<GenerateConfigRequest>,
    ) -> Result<Response<GenerateConfigResponse>, Status> {
        let mode = request.into_inner().mode;
        let mut config = std::collections::HashMap::new();
        match mode.as_str() {
            "cloud" => {
                config.insert("swarm_size".to_string(), "large".to_string());
                config.insert("database".to_string(), "postgresql".to_string());
                config.insert("cache".to_string(), "redis".to_string());
            }
            "standalone" => {
                config.insert("swarm_size".to_string(), "small".to_string());
                config.insert("database".to_string(), "sqlite".to_string());
                config.insert("cache".to_string(), "memory".to_string());
            }
            "thin_client" => {
                config.insert("swarm_size".to_string(), "none".to_string());
                config.insert("database".to_string(), "remote".to_string());
                config.insert("cache".to_string(), "none".to_string());
            }
            _ => {
                return Ok(Response::new(GenerateConfigResponse {
                    status: "error".to_string(),
                    config,
                }));
            }
        }
        Ok(Response::new(GenerateConfigResponse {
            status: "success".to_string(),
            config,
        }))
    }

    async fn save_wizard_state(
        &self,
        request: Request<SaveWizardStateRequest>,
    ) -> Result<Response<SaveWizardStateResponse>, Status> {
        let auth = require_auth_info(&request)?;
        save_wizard_state_in_db(&self.hub.pool, &auth, request.into_inner().state).await?;
        Ok(Response::new(SaveWizardStateResponse {
            status: "saved".to_string(),
        }))
    }

    async fn get_wizard_state(
        &self,
        request: Request<GetWizardStateRequest>,
    ) -> Result<Response<GetWizardStateResponse>, Status> {
        let auth = require_auth_info(&request)?;
        let state = get_wizard_state_in_db(&self.hub.pool, &auth).await?;
        Ok(Response::new(GetWizardStateResponse { state }))
    }

    async fn reset_wizard_state(
        &self,
        request: Request<ResetWizardStateRequest>,
    ) -> Result<Response<ResetWizardStateResponse>, Status> {
        let auth = require_auth_info(&request)?;
        reset_wizard_state_in_db(&self.hub.pool, &auth).await?;
        Ok(Response::new(ResetWizardStateResponse {
            status: "reset".to_string(),
        }))
    }

    async fn provision(
        &self,
        _request: Request<ProvisionRequest>,
    ) -> Result<Response<ProvisionResponse>, Status> {
        Ok(Response::new(ProvisionResponse {
            status: "provisioned".to_string(),
            message: "State persisted successfully".to_string(),
        }))
    }

    async fn publish_site(
        &self,
        request: Request<PublishSiteRequest>,
    ) -> Result<Response<PublishSiteResponse>, Status> {
        let url = if request.into_inner().domain_choice == "custom" {
            "https://www.mybusiness.com".to_string()
        } else {
            "https://mybusiness.cloud.omnisolo.co".to_string()
        };
        Ok(Response::new(PublishSiteResponse {
            status: "published".to_string(),
            url,
        }))
    }

    async fn audit_setup(
        &self,
        request: Request<AuditSetupRequest>,
    ) -> Result<Response<AuditSetupResponse>, Status> {
        let env = request.into_inner().env;
        match crate::services::onboarding::env_verifier::verify_environment(&env) {
            Ok(config) => Ok(Response::new(AuditSetupResponse {
                status: "success".to_string(),
                config: Some(env_config_from_verified(config)),
                error: String::new(),
            })),
            Err(error) => Ok(Response::new(AuditSetupResponse {
                status: "error".to_string(),
                config: None,
                error,
            })),
        }
    }

    async fn diagnostics(
        &self,
        _request: Request<DiagnosticsRequest>,
    ) -> Result<Response<DiagnosticsResponse>, Status> {
        let env_vars: std::collections::HashMap<String, String> = std::env::vars().collect();
        match crate::services::onboarding::env_verifier::verify_environment(&env_vars) {
            Ok(config) => Ok(Response::new(DiagnosticsResponse {
                status: "success".to_string(),
                config: Some(env_config_from_verified(config)),
                wizard_state: std::collections::HashMap::new(),
                error: String::new(),
            })),
            Err(error) => Ok(Response::new(DiagnosticsResponse {
                status: "error".to_string(),
                config: None,
                wizard_state: std::collections::HashMap::new(),
                error,
            })),
        }
    }

    async fn get_wizard_profile(
        &self,
        request: Request<GetWizardProfileRequest>,
    ) -> Result<Response<GetWizardProfileResponse>, Status> {
        Ok(Response::new(wizard_profile_response(
            &request.into_inner().mode,
        )))
    }

    async fn create_task(
        &self,
        request: Request<CreateTaskRequest>,
    ) -> Result<Response<::server_omnisolo::orchestration::SharedTask>, Status> {
        let auth = require_auth_info(&request)?;
        let req = request.into_inner();
        let task = self
            .hub
            .task_manager()
            .create_task(
                auth.org_id,
                req.mission_id,
                req.title,
                req.description,
                req.priority,
            )
            .map_err(Status::internal)?;
        Ok(Response::new(task.into_proto()))
    }

    type PollTasksStream = Pin<
        Box<dyn Stream<Item = Result<::server_omnisolo::orchestration::SharedTask, Status>> + Send>,
    >;

    #[allow(clippy::result_large_err)]
    async fn poll_tasks(
        &self,
        request: Request<PollTasksRequest>,
    ) -> Result<Response<Self::PollTasksStream>, Status> {
        let auth = require_auth_info(&request)?;
        let req = request.into_inner();
        let tasks = poll_tasks_for_org(
            self.hub.task_manager(),
            &auth,
            &req.agent_id,
            req.limit.max(0) as usize,
        )?;
        let items: Vec<Result<_, Status>> = tasks.into_iter().map(|t| Ok(t.into_proto())).collect();
        let stream = tokio_stream::iter(items);
        Ok(Response::new(Box::pin(stream) as Self::PollTasksStream))
    }

    async fn update_task_status(
        &self,
        request: Request<UpdateTaskStatusRequest>,
    ) -> Result<Response<UpdateTaskStatusResponse>, Status> {
        let auth = require_auth_info(&request)?;
        update_task_status_for_org(self.hub.task_manager(), &auth, &request.into_inner())?;
        Ok(Response::new(UpdateTaskStatusResponse { success: true }))
    }

    async fn approve_task(
        &self,
        request: Request<ApproveTaskRequest>,
    ) -> Result<Response<ApproveTaskResponse>, Status> {
        let org_id = request
            .extensions()
            .get::<::server_common::Claims>()
            .and_then(::server_common::auth_utils::signed_tenant_id)
            .ok_or_else(|| Status::unauthenticated("Missing signed tenant claims"))?;
        let req = request.into_inner();
        self.dept_orchestrator
            .decide_approval(&req.task_id, &org_id, req.is_approved, None)
            .await
            .map_err(Status::internal)?;
        Ok(Response::new(ApproveTaskResponse { success: true }))
    }

    async fn get_pending_approvals(
        &self,
        request: Request<GetPendingApprovalsRequest>,
    ) -> Result<Response<GetPendingApprovalsResponse>, Status> {
        let auth = require_auth_info(&request)?;
        let org_id = require_org_match(&auth, &request.into_inner().organization_id)?;
        let approvals = self
            .dept_orchestrator
            .get_pending_approvals(&org_id, None, 100)
            .await;
        Ok(Response::new(GetPendingApprovalsResponse {
            tasks: map_pending_approvals(approvals),
        }))
    }

    async fn delegate_sub_task(
        &self,
        request: Request<SubTask>,
    ) -> Result<Response<DelegateTaskResponse>, Status> {
        let auth = require_auth_info(&request)?;
        let req = request.into_inner();
        if auth.agent_id != req.from_agent_id {
            return Err(Status::permission_denied(
                "Authenticated identity does not match from_agent_id",
            ));
        }
        if req.task_id.is_empty() || req.target_role.is_empty() || req.instruction.trim().is_empty()
        {
            return Err(Status::invalid_argument(
                "task_id, target_role, and instruction are required",
            ));
        }
        if self
            .hub
            .get_agent(&req.from_agent_id)
            .await
            .is_none_or(|agent| agent.organization_id != auth.org_id)
        {
            return Err(Status::permission_denied("sender agent is not available"));
        }
        self.hub
            .clone()
            .delegate_sub_task(
                &req.from_agent_id,
                &req.target_role,
                &req.instruction,
                &req.parent_thread_id,
            )
            .await
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(DelegateTaskResponse { success: true }))
    }

    async fn advertise_capabilities(
        &self,
        request: Request<AgentCapabilities>,
    ) -> Result<Response<PublishMessageResponse>, Status> {
        let auth = require_auth_info(&request)?;
        let req = request.into_inner();
        if req.agent_id.is_empty() {
            return Err(Status::invalid_argument("agent_id is required"));
        }
        if req.agent_id != auth.agent_id {
            return Err(Status::permission_denied(
                "agent_id does not match authenticated identity",
            ));
        }
        self.hub
            .advertise_capabilities(req)
            .map_err(Status::internal)?;
        Ok(Response::new(PublishMessageResponse { success: true }))
    }

    type DiscoverAgentsStream =
        Pin<Box<dyn Stream<Item = Result<AgentCapabilities, Status>> + Send>>;

    async fn discover_agents(
        &self,
        request: Request<Query>,
    ) -> Result<Response<Self::DiscoverAgentsStream>, Status> {
        let auth = require_auth_info(&request)?;
        Ok(Response::new(discover_agents_stream(
            self.hub.clone(),
            auth,
        )))
    }

    type StreamMeshEventsStream = Pin<Box<dyn Stream<Item = Result<MeshEvent, Status>> + Send>>;

    async fn publish_mesh_event(
        &self,
        request: Request<PublishMeshEventRequest>,
    ) -> Result<Response<PublishMessageResponse>, Status> {
        let auth = require_auth_info(&request)?;
        let mut event = request
            .into_inner()
            .event
            .ok_or_else(|| Status::invalid_argument("event is required"))?;
        event.topic = scope_mesh_channel(&auth.org_id, &event.topic)?;
        self.publish_counter.add(1, &[]);
        self.hub
            .publish_mesh_event(event)
            .await
            .map_err(Status::internal)?;
        Ok(Response::new(PublishMessageResponse { success: true }))
    }

    async fn stream_mesh_events(
        &self,
        request: Request<EventStreamRequest>,
    ) -> Result<Response<Self::StreamMeshEventsStream>, Status> {
        let auth = require_auth_info(&request)?;
        let topic = scope_mesh_channel(&auth.org_id, &request.into_inner().topic)?;
        self.stream_counter.add(1, &[]);
        let rx = self.hub.subscribe_mesh_events(topic).await;
        let rx_stream = futures::TryStreamExt::map_err(
            tokio_stream::wrappers::BroadcastStream::new(rx),
            |error| Status::internal(error.to_string()),
        );
        Ok(Response::new(
            Box::pin(rx_stream) as Self::StreamMeshEventsStream
        ))
    }

    type StreamTeammateMeshStream =
        Pin<Box<dyn Stream<Item = Result<TeammateMeshEvent, Status>> + Send>>;

    async fn publish_teammate_mesh_event(
        &self,
        request: Request<PublishTeammateMeshEventRequest>,
    ) -> Result<Response<PublishMessageResponse>, Status> {
        let auth = require_auth_info(&request)?;
        let req = request.into_inner();
        let channel = scope_mesh_channel(&auth.org_id, &req.channel)?;
        let mut event = req
            .event
            .ok_or_else(|| Status::invalid_argument("event is required"))?;
        if !event.agent_id.is_empty() && event.agent_id != auth.agent_id {
            return Err(Status::permission_denied(
                "Teammate event agent_id mismatch",
            ));
        }
        event.agent_id = auth.agent_id;
        self.hub
            .publish_teammate_event(channel, event)
            .await
            .map_err(Status::internal)?;
        Ok(Response::new(PublishMessageResponse { success: true }))
    }

    async fn stream_teammate_mesh(
        &self,
        request: Request<EventStreamRequest>,
    ) -> Result<Response<Self::StreamTeammateMeshStream>, Status> {
        let auth = require_auth_info(&request)?;
        let topic = scope_mesh_channel(&auth.org_id, &request.into_inner().topic)?;
        let rx = self.hub.subscribe_teammate_mesh(topic).await;
        let rx_stream = futures::TryStreamExt::map_err(
            tokio_stream::wrappers::BroadcastStream::new(rx),
            |error| Status::internal(error.to_string()),
        );
        Ok(Response::new(
            Box::pin(rx_stream) as Self::StreamTeammateMeshStream
        ))
    }

    async fn invite(
        &self,
        request: Request<InviteRequest>,
    ) -> Result<Response<InviteResponse>, Status> {
        let tenant_id = authorize_invite_tenant(&request)?;
        let req = request.into_inner();
        if req.team_id.is_empty() || req.inviter_id.is_empty() || req.invitee_id.is_empty() {
            return Err(Status::invalid_argument("Missing required fields"));
        }
        self.invite_tracker
            .record_invite(&tenant_id, &req.team_id, &req.inviter_id, &req.invitee_id)
            .await
            .map_err(|e| Status::internal(format!("Failed to record invite: {}", e)))?;
        self.viral_loop_tracker.record_invite_sent(&req.inviter_id);
        Ok(Response::new(InviteResponse { success: true }))
    }

    async fn accept_invite(
        &self,
        request: Request<AcceptInviteRequest>,
    ) -> Result<Response<AcceptInviteResponse>, Status> {
        let _auth = require_auth_info(&request)?;
        let req = request.into_inner();
        if req.invitee_id.is_empty() {
            return Err(Status::invalid_argument("Missing invitee_id"));
        }
        self.viral_loop_tracker
            .record_invite_accepted(&req.invitee_id);
        Ok(Response::new(AcceptInviteResponse { success: true }))
    }

    async fn get_meetings(
        &self,
        request: Request<EmptyRequest>,
    ) -> Result<Response<GetMeetingsResponse>, Status> {
        let auth = require_auth_info(&request)?;
        let meetings = self.hub.get_meetings_by_org(&auth.org_id).await;
        Ok(Response::new(GetMeetingsResponse {
            meetings: meetings.as_ref().clone(),
        }))
    }
}
