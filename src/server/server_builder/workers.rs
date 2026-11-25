// SPDX-License-Identifier: Apache-2.0
//! OSS background worker startup loops spawned by [`super::builder::OhcServerBuilder`].

use chrono::Utc;
use std::sync::Arc;

use super::builder::{BUILTIN_AGENT_SERVICE, ServerContext};

pub(crate) async fn spawn_oss_background_workers(
    ctx: &ServerContext,
    configured_queue: Option<Arc<dyn crate::queue::TaskQueue>>,
    builtin_agent_auth: Option<omnisolo_builtin_agent::auth::AuthMode>,
    event_rx: &mut tokio::sync::mpsc::Receiver<serde_json::Value>,
) -> Result<(), Box<dyn std::error::Error>> {
    let db = &ctx.db;
    let hub = &ctx.hub;
    let dept_orchestrator = &ctx.dept_orchestrator;
    let shutdown = &ctx.shutdown;
    let legacy_sqlx_background_enabled = ctx.legacy_sqlx_background_enabled;
    let is_cloud = ctx.edition.multi_tenant;

    if legacy_sqlx_background_enabled && matches!(&db.store, crate::db::DbStore::Postgres) {
        tokio::spawn(crate::builder::publication_worker::run_publication_worker(
            db.pool.clone(),
            shutdown.watch_receiver(),
        ));
    }

    if legacy_sqlx_background_enabled {
        ctx.autodream_worker.start();
        let vector_repo = Arc::new(match &db.store {
            crate::db::DbStore::Postgres => {
                omnisolo_builtin_agent::memory_store::VectorRepository::new(db.pool.clone())
            }
            crate::db::DbStore::Sqlite(sqlite_pool) => {
                omnisolo_builtin_agent::memory_store::VectorRepository::new_sqlite(
                    sqlite_pool.clone(),
                )
            }
        });
        let cb = Arc::new(|msg: &str, _err: &str| {
            ::server_telemetry::record_error_signal(msg);
        }) as Arc<dyn Fn(&str, &str) + Send + Sync>;
        let consolidation_worker =
            Arc::new(crate::workers::memory::MemoryConsolidationWorker::new(
                vector_repo,
                std::time::Duration::from_secs(3600),
                180,
                20,
                2,
                vec![
                    "TASK_SUMMARY".to_string(),
                    "NOTES".to_string(),
                    "SESSION_DATA".to_string(),
                    "NOTE".to_string(),
                    "SUMMARY".to_string(),
                    "CS_NOTE".to_string(),
                    "AGENT_ACTION".to_string(),
                ],
                Some(cb),
            ));
        drop(consolidation_worker.spawn_background_task());
        crate::workers::subscription_retention_job::SubscriptionRetentionJob::new(db.clone())
            .start();
        crate::workers::subscription_replenishment_job::SubscriptionReplenishmentJob::new(
            db.clone(),
        )
        .start();
        crate::workers::subscription_health_job::SubscriptionHealthJob::new(db.clone()).start();
        Arc::new(
            crate::workers::subscription_replenishment_worker::SubscriptionReplenishmentWorker::new(
                db.clone(),
            ),
        )
        .start();
        crate::workers::competitor_audit::CompetitorAuditWorker::new(db.clone()).start();
        crate::workers::department_workers::PromoterWorker::new(db.clone(), hub.clone()).start();
        crate::workers::department_workers::OperationsWorker::new(db.clone()).start();
        crate::workers::department_workers::CustomerSuccessWorker::new(db.clone()).start();
        crate::workers::booking_reengagement::BookingReengagementWorker::new(db.clone()).start();
        crate::workers::booking_reengagement_job::BookingReengagementJob::new(db.clone()).start();
        Arc::new(crate::workers::customer_memory_worker::CustomerMemoryWorker::new(db.clone()))
            .start();
        Arc::new(crate::workers::message_triage_worker::MessageTriageWorker::new(db.clone()))
            .start();
        Arc::new(crate::workers::inquiry_intake_worker::InquiryIntakeWorker::new(db.clone()))
            .start();
        Arc::new(crate::workers::draft_quote_worker::DraftQuoteWorker::new(
            db.clone(),
        ))
        .start();
        Arc::new(crate::workers::deposit_follow_up_worker::DepositFollowUpWorker::new(db.clone()))
            .start();
        Arc::new(
            crate::workers::missed_lead_recovery_worker::MissedLeadRecoveryWorker::new(db.clone()),
        )
        .start();
        crate::workers::proactive_operations_worker::ProactiveOperationsWorker::new(db.clone())
            .start();
        crate::workers::proactive_analysis_job::ProactiveAnalysisWorker::new(db.clone()).start();
        crate::workers::daily_ops_routine_worker::DailyOpsRoutineWorker::new(db.clone()).start();
        if matches!(&db.store, crate::db::DbStore::Postgres) {
            crate::cart_recovery::start_cart_recovery_background_workers(Arc::new(db.pool.clone()));
            let _ = tokio::sync::watch::channel(false);
        }
        Arc::new(crate::telemetry::forecaster::Forecaster::new(
            db.pool.clone(),
        ))
        .start();
        let memory_embedding_api =
            Arc::new(crate::workers::agent_memory_pipeline::DefaultMemoryEmbeddingApi::new());
        let agent_memory_pipeline = Arc::new(
            crate::workers::agent_memory_pipeline::AgentMemoryPipeline::new(
                db.clone(),
                memory_embedding_api,
            ),
        );
        tokio::spawn(async move {
            loop {
                if let Err(e) = agent_memory_pipeline.run().await {
                    ::server_telemetry::record_error_signal("[bug] Agent Memory Pipeline error");
                    tracing::error!("Agent Memory Pipeline error: {}", e);
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
            }
        });

        let health_worker = Arc::new(
            crate::workers::subscription_health_worker::SubscriptionHealthWorker::new(db.clone())
                .with_orchestrator(dept_orchestrator.clone()),
        );
        health_worker.start_with_shutdown(shutdown.clone());
        drop(crate::workers::invoice_followup_worker::start_invoice_followup_worker(db.clone()));

        let agent_action_worker = Arc::new(
            crate::workers::agent_action_worker::AgentActionWorker::new(
                db.pool.clone(),
                String::new(),
            )
            .with_authority(ctx.auth_store.as_ref())
            .await,
        );
        agent_action_worker.start();
    }

    if !is_cloud && let crate::db::DbStore::Sqlite(pool) = &db.store {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS consolidated_memory (
                id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, agent_id TEXT,
                content TEXT NOT NULL, embedding VECTOR(1536), source_type TEXT NOT NULL,
                created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                last_referenced_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                reference_count INTEGER DEFAULT 0, reliability_score INTEGER DEFAULT 50,
                owner_override BOOLEAN DEFAULT FALSE, metadata TEXT
            );",
        )
        .execute(pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS onboarding_state (
                tenant_id TEXT NOT NULL, user_id TEXT NOT NULL,
                current_step INTEGER NOT NULL DEFAULT 0, state_json TEXT NOT NULL DEFAULT '{}',
                created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (tenant_id, user_id)
            );",
        )
        .execute(pool)
        .await?;
    }

    let ops_agent = Arc::new(tokio::sync::RwLock::new(
        crate::orchestration::departments::operations_agent::OperationsAgent::new(
            dept_orchestrator.clone(),
        ),
    ));
    let cs_agent = Arc::new(tokio::sync::RwLock::new(
        crate::orchestration::departments::customer_success_agent::CustomerSuccessAgent::new(
            dept_orchestrator.clone(),
        )
        .with_hub(hub.clone()),
    ));
    let mkt_agent = Arc::new(tokio::sync::RwLock::new(
        crate::orchestration::departments::marketing_agent::MarketingAgent::new(
            dept_orchestrator.clone(),
        ),
    ));
    let sales_agent = Arc::new(tokio::sync::RwLock::new(
        crate::orchestration::departments::sales_agent::SalesAgent::new(dept_orchestrator.clone()),
    ));
    let finance_agent = Arc::new(tokio::sync::RwLock::new(
        crate::orchestration::departments::finance_agent::FinanceAgent::new(
            dept_orchestrator.clone(),
        ),
    ));
    let legal_agent = Arc::new(tokio::sync::RwLock::new(
        crate::orchestration::departments::legal_agent::LegalAgent::new(dept_orchestrator.clone()),
    ));
    let advisory_agent = Arc::new(tokio::sync::RwLock::new(
        crate::orchestration::departments::business_advisory_agent::BusinessAdvisoryAgent::new(
            dept_orchestrator.clone(),
        ),
    ));
    let translation_agent = Arc::new(tokio::sync::RwLock::new(
        crate::orchestration::departments::translation_agent::TranslationAgent::new(
            dept_orchestrator.clone(),
        ),
    ));
    let multilingual_agent = Arc::new(tokio::sync::RwLock::new(
        crate::orchestration::departments::multilingual_agent::MultilingualAgent::new(
            dept_orchestrator.clone(),
        ),
    ));
    tokio::join!(
        dept_orchestrator.register_department(ops_agent),
        dept_orchestrator.register_department(cs_agent),
        dept_orchestrator.register_department(mkt_agent),
        dept_orchestrator.register_department(sales_agent),
        dept_orchestrator.register_department(finance_agent),
        dept_orchestrator.register_department(legal_agent),
        dept_orchestrator.register_department(advisory_agent),
        dept_orchestrator.register_department(translation_agent),
        dept_orchestrator.register_department(multilingual_agent)
    );

    let bus = Arc::new(crate::msgbus::MemoryBus::new());
    let mut products_rx = hub
        .subscribe_teammate_mesh("products_inbox".to_string())
        .await;
    let orch_clone = dept_orchestrator.clone();
    tokio::spawn(async move {
        while let Ok(event) = products_rx.recv().await {
            if matches!(
                event.action.as_str(),
                "ProductCreated"
                    | "ProductUpdated"
                    | "tenant.product.created"
                    | "tenant.product.updated"
            ) && let Ok(payload_str) = String::from_utf8(event.payload.clone())
                && let Ok(payload_json) = serde_json::from_str::<serde_json::Value>(&payload_str)
            {
                let tenant_id = payload_json
                    .get("organization_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("system")
                    .to_string();
                let event_type = if matches!(
                    event.action.as_str(),
                    "ProductCreated" | "tenant.product.created"
                ) {
                    "tenant.product.created".to_string()
                } else {
                    "tenant.product.updated".to_string()
                };
                let _ = orch_clone
                    .dispatch_event(crate::orchestration::departments::types::DepartmentEvent {
                        id: uuid::Uuid::new_v4().to_string(),
                        tenant_id,
                        event_type,
                        payload: payload_json,
                    })
                    .await;
            }
        }
    });

    crate::services::agent::department::service::DepartmentService::new(
        bus,
        dept_orchestrator.clone(),
    )
    .start()
    .await
    .expect("Failed to start DepartmentService");

    let tm_mesh = ctx.handoff_mesh.clone();
    hub.task_manager()
        .set_broadcaster(Arc::new(move |task, event_type| {
            use crate::orchestration::mesh::TeammateMesh as _;
            let task_value = serde_json::to_value(&task).unwrap_or(serde_json::Value::Null);
            let redacted_task = ::server_telemetry::redact_interface_pii(task_value);
            let Ok(payload) = serde_json::to_string(&redacted_task) else {
                ::server_telemetry::record_error_signal("[bug] Failed to serialize task");
                return;
            };
            let msg = ::server_omnisolo::orchestration::TeammateMeshEvent {
                agent_id: "system".to_string(),
                action: event_type,
                status: "ok".to_string(),
                payload: payload.into_bytes(),
                msg_id: uuid::Uuid::new_v4().to_string(),
            };
            let tm_mesh_clone = tm_mesh.clone();
            tokio::spawn(async move {
                let _ = tm_mesh_clone.publish("tasks", msg.payload).await;
            });
        }));

    let handoff_manager = crate::orchestration::handoff::HandoffManager::new(
        ctx.handoff_mesh.clone(),
        db.clone(),
        is_cloud,
    );
    if let Err(e) = handoff_manager.start_listener().await {
        ::server_telemetry::record_error_signal("[bug] Failed to start handoff listener");
        tracing::trace!("Failed to start handoff listener: {}", e);
    }

    let monitor_mesh = ctx.handoff_mesh.clone();
    let monitor_hub = hub.clone();
    tokio::spawn(async move {
        crate::orchestration::health::run_health_monitor(
            monitor_mesh,
            monitor_hub,
            is_cloud,
            std::time::Duration::from_secs(if is_cloud { 30 } else { 300 }),
        )
        .await;
    });

    if let Some(builtin_auth) = builtin_agent_auth {
        let builtin_transport = ctx.mesh_transport.clone();
        let builtin_mesh = ctx.handoff_mesh.clone();
        let builtin_shutdown = shutdown.clone();
        tokio::spawn(async move {
            let agent_id = std::env::var("OMNISOLO_AGENT_ID")
                .unwrap_or_else(|_| uuid::Uuid::new_v4().hyphenated().to_string());
            let heartbeat_transport = builtin_transport.clone();
            let heartbeat_agent_id = agent_id.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
                loop {
                    tokio::select! {
                        _ = builtin_shutdown.cancelled() => break,
                        _ = interval.tick() => {
                            let _ = heartbeat_transport
                                .register_presence(&heartbeat_agent_id, "online", 60)
                                .await;
                        }
                    }
                }
            });
            use crate::orchestration::mesh::TeammateMesh as _;
            let _health_cancel = builtin_mesh.start_health_responder().await;
            let cfg = omnisolo_builtin_agent::service::AgentConfig {
                llm_provider: std::env::var("OMNISOLO_LLM_PROVIDER").unwrap_or_default(),
                model: std::env::var("OMNISOLO_LLM_MODEL").unwrap_or_default(),
                llm_endpoint: std::env::var("OMNISOLO_LOCAL_LLM_ENDPOINT").unwrap_or_default(),
                system_prompt: ::ohc_core::prompt::compression::reduce_tokens(
                    &std::env::var("OMNISOLO_SYSTEM_PROMPT").unwrap_or_default(),
                ),
                max_tokens: std::env::var("OMNISOLO_MAX_TOKENS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .filter(|&v| v > 0 && v <= 4096)
                    .unwrap_or(2048),
                temperature: std::env::var("OMNISOLO_TEMPERATURE")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0.0),
                max_iterations: std::env::var("OMNISOLO_MAX_ITERATIONS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(100),
                max_context_messages: std::env::var("OMNISOLO_MAX_CONTEXT_MESSAGES")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(80),
            };
            let mut svc_impl =
                omnisolo_builtin_agent::service::AgentServiceImpl::new(agent_id, cfg, builtin_auth);
            svc_impl.init_memory().await;
            let svc = Arc::new(svc_impl);
            let _ = BUILTIN_AGENT_SERVICE.set(svc.clone());
            omnisolo_builtin_agent::start_builtin_agent(builtin_transport, svc).await;
        });
    }

    let omnisolo_job_queue: Arc<dyn crate::queue::TaskQueue> = match configured_queue {
        Some(q) => q,
        None => match &db.store {
            crate::db::DbStore::Postgres => {
                Arc::new(crate::queue::PostgresTaskQueue::new(db.pool.clone()))
            }
            crate::db::DbStore::Sqlite(sqlite_pool) => {
                Arc::new(crate::queue::SqliteTaskQueue::new(sqlite_pool.clone()))
            }
        },
    };
    let omnisolo_job_queue_clone = omnisolo_job_queue.clone();
    tokio::spawn(async move {
        loop {
            if let Ok(Some(job)) = omnisolo_job_queue_clone
                .dequeue(vec![
                    "sub_agent".to_string(),
                    "specialized_sub_agent".to_string(),
                    "general_sub_agent".to_string(),
                ])
                .await
            {
                let _ = omnisolo_job_queue_clone
                    .complete(&job.id, &job.tenant_id)
                    .await;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
    });

    if legacy_sqlx_background_enabled && matches!(&db.store, crate::db::DbStore::Postgres) {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let pool_outbox = db.pool.clone();
        tokio::spawn(async move {
            let _keep_tx = _shutdown_tx;
            crate::services::chat::outbox::run_chat_outbox_worker(pool_outbox, shutdown_rx).await;
        });
    }

    let hub_clone = hub.clone();
    let event_shutdown = shutdown.clone();
    // Drain event receiver without blocking shutdown.
    let (ev_tx_owned, mut ev_rx_owned) = tokio::sync::mpsc::channel(1);
    drop(ev_tx_owned);
    std::mem::swap(event_rx, &mut ev_rx_owned);
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = event_shutdown.cancelled() => break,
                maybe_event = ev_rx_owned.recv() => {
                    let Some(raw_event) = maybe_event else { break };
                    let event = hub_clone.sanitize_hub_event(raw_event);
                    hub_clone.append_recent_event(event).await;
                }
            }
        }
    });

    let cleanup_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(3600));
        interval.tick().await;
        loop {
            tokio::select! {
                _ = cleanup_shutdown.cancelled() => break,
                _ = interval.tick() => {
                    let _ = tokio::task::spawn_blocking(crate::utils::fs::cleanup_stale_temp_files).await;
                }
            }
        }
    });

    let hub_for_sched = hub.clone();
    let sched_shutdown = shutdown.clone();
    if legacy_sqlx_background_enabled {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(1));
            let mut prune_interval = tokio::time::interval(tokio::time::Duration::from_secs(300));
            loop {
                tokio::select! {
                    _ = sched_shutdown.cancelled() => break,
                    _ = prune_interval.tick() => {
                        let sip_db = crate::sip::SipDB::new(hub_for_sched.pool.clone(), "system".to_string());
                        let _ = sip_db.prune_stale_missions(chrono::Duration::days(7)).await;
                        let _ = sip_db.cleanup_stagnant_missions(chrono::Duration::minutes(5)).await;
                        let job_queue = crate::orchestration::queue::omnisolo_job_queue::OmniSoloJobQueue::new(Arc::new(hub_for_sched.pool.clone()));
                        let _ = job_queue.cleanup_stale_jobs().await;
                        let _ = omnisolo_job_queue.cleanup_stale_jobs().await;
                    }
                    _ = interval.tick() => {
                        let due = hub_for_sched.scheduler().poll_due();
                        for task in due {
                            if hub_for_sched.scheduler().mark_running(&task.organization_id, &task.id).is_err() {
                                continue;
                            }
                            let msg = ::server_omnisolo::orchestration::Message {
                                id: format!("{}-{}", task.id, Utc::now().timestamp()),
                                from_agent: "system-scheduler".to_string(),
                                to_agent: task.agent_id.clone(),
                                r#type: "task".to_string(),
                                content: format!("Scheduled Task triggered: {}.", task.name),
                                occurred_at_unix: Utc::now().timestamp(),
                                meeting_id: String::new(),
                            };
                            let ok = hub_for_sched.clone().publish(msg).await.is_ok();
                            let _ = hub_for_sched.scheduler().mark_done(&task.organization_id, &task.id, ok);
                        }
                    }
                }
            }
        });
    }

    Ok(())
}
