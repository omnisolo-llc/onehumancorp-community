-- SPDX-License-Identifier: Apache-2.0
-- Unified Single-Tenant OSS Migration 0004 (PostgreSQL):
-- Agent runtime registry, approval feeds, action queues, shared task graphs,
-- session context, knowledge embeddings, memory stores, event subscriptions,
-- sub-agent queue, job queue, universal ledger, and local integrations.

CREATE TABLE IF NOT EXISTS agents (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    role TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'IDLE',
    provider_type TEXT NOT NULL DEFAULT '',
    region TEXT NOT NULL DEFAULT '',
    registered_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS agent_status (
    agent_id TEXT PRIMARY KEY REFERENCES agents(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    status TEXT NOT NULL,
    last_heartbeat TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS agent_feed (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    source TEXT NOT NULL,
    priority TEXT,
    title TEXT,
    description TEXT,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    state TEXT NOT NULL DEFAULT 'PENDING_APPROVAL',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_agent_feed_tenant ON agent_feed (tenant_id, state);

CREATE TABLE IF NOT EXISTS agent_feed_items (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    event_source TEXT NOT NULL,
    context_payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    proposed_action JSONB NOT NULL DEFAULT '{}'::jsonb,
    lifecycle_state TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_agent_feed_items_tenant
    ON agent_feed_items (tenant_id, lifecycle_state);

CREATE TABLE IF NOT EXISTS agent_feed_decisions (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    action_id TEXT NOT NULL,
    origin TEXT NOT NULL,
    decision_state TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    token_id TEXT NOT NULL,
    job_id TEXT UNIQUE,
    dispatch_payload JSONB,
    dispatch_status TEXT NOT NULL CHECK (dispatch_status IN
        ('NOT_REQUESTED', 'PENDING', 'ATTEMPTING', 'DISPATCH_RETURNED', 'RECONCILIATION_REQUIRED', 'CANCELLED')),
    attempted_at TIMESTAMPTZ,
    dispatch_returned_at TIMESTAMPTZ,
    detail TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, action_id),
    CHECK (attempted_at IS NULL OR dispatch_status IN ('ATTEMPTING', 'DISPATCH_RETURNED', 'RECONCILIATION_REQUIRED'))
);

CREATE INDEX IF NOT EXISTS agent_feed_decisions_pending
    ON agent_feed_decisions (dispatch_status, updated_at);

CREATE OR REPLACE FUNCTION sync_items_to_agent_feed()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NEW;
    END IF;
    UPDATE agent_feed
    SET state = NEW.lifecycle_state,
        payload = NEW.proposed_action,
        description = COALESCE(NEW.context_payload->>'description', description),
        title = COALESCE(NEW.context_payload->>'title', title),
        priority = COALESCE(NEW.context_payload->>'priority', priority),
        updated_at = CURRENT_TIMESTAMP
    WHERE id = NEW.id AND tenant_id = NEW.tenant_id;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS trg_items_sync_agent_feed ON agent_feed_items;
CREATE TRIGGER trg_items_sync_agent_feed
    AFTER UPDATE OF lifecycle_state, context_payload, proposed_action ON agent_feed_items
    FOR EACH ROW
    EXECUTE FUNCTION sync_items_to_agent_feed();

CREATE OR REPLACE FUNCTION protect_agent_feed_dispatch_fence()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        IF OLD.attempted_at IS NOT NULL OR OLD.dispatch_status = 'RECONCILIATION_REQUIRED' THEN
            RAISE EXCEPTION 'Agent feed dispatch attempt evidence cannot be deleted' USING ERRCODE = '23514';
        END IF;
        RETURN OLD;
    END IF;
    IF NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
       OR NEW.action_id IS DISTINCT FROM OLD.action_id
       OR NEW.origin IS DISTINCT FROM OLD.origin
       OR (OLD.job_id IS NOT NULL AND NEW.job_id IS DISTINCT FROM OLD.job_id)
       OR (OLD.attempted_at IS NOT NULL AND (
           NEW.attempted_at IS DISTINCT FROM OLD.attempted_at
           OR NEW.actor_id IS DISTINCT FROM OLD.actor_id
           OR NEW.token_id IS DISTINCT FROM OLD.token_id
           OR NEW.dispatch_payload IS DISTINCT FROM OLD.dispatch_payload))
       OR (OLD.dispatch_returned_at IS NOT NULL
           AND NEW.dispatch_returned_at IS DISTINCT FROM OLD.dispatch_returned_at)
    THEN
        RAISE EXCEPTION 'Agent feed dispatch identity and attempt evidence are immutable' USING ERRCODE = '23514';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS protect_agent_feed_dispatch_fence ON agent_feed_decisions;
CREATE TRIGGER protect_agent_feed_dispatch_fence
    BEFORE UPDATE OR DELETE ON agent_feed_decisions
    FOR EACH ROW
    EXECUTE FUNCTION protect_agent_feed_dispatch_fence();

CREATE TABLE IF NOT EXISTS agent_action_requests (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    action_type TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'Pending',
    confidence_score DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    product_id TEXT,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    source TEXT,
    agent_type TEXT,
    description TEXT,
    department_type TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_agent_action_requests_tenant_status
    ON agent_action_requests (tenant_id, status);

CREATE TABLE IF NOT EXISTS action_required_queue (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    agent_id TEXT,
    action_type TEXT NOT NULL,
    priority TEXT NOT NULL DEFAULT 'normal',
    title TEXT NOT NULL,
    summary TEXT NOT NULL DEFAULT '',
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved', 'rejected', 'expired')),
    resolved_by TEXT,
    resolved_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_action_required_queue_tenant_status
    ON action_required_queue (tenant_id, status, created_at DESC);

CREATE TABLE IF NOT EXISTS agent_approvals (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    department TEXT NOT NULL,
    description TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'PENDING',
    action_risk TEXT NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS shared_tasks_v4 (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL DEFAULT 'default_tenant',
    title TEXT NOT NULL,
    description TEXT,
    status TEXT NOT NULL DEFAULT 'PENDING',
    agent_id TEXT,
    priority TEXT NOT NULL DEFAULT 'P2',
    payload JSONB,
    parent_plan_id TEXT,
    dependencies JSONB NOT NULL DEFAULT '[]'::jsonb,
    auto_dreamed BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_shared_tasks_v4_tenant_status
    ON shared_tasks_v4 (tenant_id, status);

CREATE TABLE IF NOT EXISTS shared_tasks (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL DEFAULT 'default_tenant',
    organization_id TEXT NOT NULL DEFAULT 'default_tenant',
    parent_plan_id TEXT,
    title TEXT NOT NULL DEFAULT '',
    description TEXT,
    status TEXT NOT NULL DEFAULT 'PENDING',
    assigned_agent_id TEXT,
    payload JSONB,
    dependencies JSONB NOT NULL DEFAULT '[]'::jsonb,
    auto_dreamed BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_shared_tasks_org_status
    ON shared_tasks (organization_id, status);

CREATE TABLE IF NOT EXISTS shared_tasks_decomposition (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL DEFAULT 'default_tenant',
    organization_id TEXT NOT NULL DEFAULT 'default_tenant',
    mission_id TEXT NOT NULL DEFAULT '',
    title TEXT NOT NULL DEFAULT '',
    description TEXT,
    status TEXT NOT NULL DEFAULT 'PENDING',
    assigned_agent_id TEXT,
    priority TEXT NOT NULL DEFAULT 'P2',
    payload JSONB,
    parent_plan_id TEXT,
    dependencies JSONB NOT NULL DEFAULT '[]'::jsonb,
    locked_until TIMESTAMPTZ,
    ultraplan_phase TEXT,
    deliberation_log JSONB,
    depth INTEGER,
    action_risk TEXT,
    approval_status TEXT,
    proposed_content TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS task_dependencies (
    task_id TEXT NOT NULL,
    depends_on_task_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    PRIMARY KEY (task_id, depends_on_task_id)
);

CREATE TABLE IF NOT EXISTS shared_task_dependencies (
    task_id TEXT NOT NULL,
    depends_on_task_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    organization_id TEXT NOT NULL,
    PRIMARY KEY (task_id, depends_on_task_id)
);

CREATE TABLE IF NOT EXISTS agent_session_data (
    session_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    agent_id TEXT NOT NULL,
    context_data TEXT NOT NULL,
    device_signature TEXT,
    _sync_status TEXT DEFAULT 'pending',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_accessed TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS knowledge_embeddings (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    agent_id TEXT,
    task_id TEXT,
    content TEXT NOT NULL,
    embedding BYTEA,
    source_type TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_knowledge_embeddings_tenant
    ON knowledge_embeddings (tenant_id, source_type);

CREATE TABLE IF NOT EXISTS consolidated_memory (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    agent_id TEXT,
    content TEXT NOT NULL,
    embedding BYTEA,
    source_type TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_referenced_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    reference_count INTEGER NOT NULL DEFAULT 0,
    reliability_score INTEGER NOT NULL DEFAULT 50,
    owner_override BOOLEAN NOT NULL DEFAULT FALSE,
    metadata JSONB
);

CREATE TABLE IF NOT EXISTS agent_memories (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    task_id TEXT NOT NULL,
    raw_content BYTEA NOT NULL,
    summary_embedding BYTEA,
    department TEXT,
    interaction_data JSONB NOT NULL DEFAULT '{}'::jsonb,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS autodream_memories (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    agent_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    content TEXT NOT NULL,
    embedding BYTEA,
    source_type TEXT NOT NULL,
    topic TEXT NOT NULL DEFAULT '',
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS agent_missions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    status TEXT NOT NULL,
    payload TEXT NOT NULL,
    mission_log TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS agent_event_subscriptions (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    agent_role TEXT NOT NULL,
    topic TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, agent_role, topic)
);

CREATE INDEX IF NOT EXISTS idx_agent_event_subscriptions_tenant
    ON agent_event_subscriptions (tenant_id);

CREATE TABLE IF NOT EXISTS sub_agent_queue (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    parent_task_id TEXT,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    status TEXT NOT NULL DEFAULT 'QUEUED',
    worker_id TEXT,
    scheduled_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    locked_until TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_sub_agent_queue_tenant_status
    ON sub_agent_queue (tenant_id, status, scheduled_at);

CREATE TABLE IF NOT EXISTS ohc_job_queue (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    parent_task_id TEXT,
    job_type TEXT NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    status TEXT NOT NULL DEFAULT 'PENDING',
    retry_count INTEGER NOT NULL DEFAULT 0,
    max_retries INTEGER NOT NULL DEFAULT 3,
    next_retry_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    locked_until TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS ohc_universal_ledger (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    department TEXT NOT NULL,
    event_type TEXT NOT NULL DEFAULT '',
    action_type TEXT NOT NULL DEFAULT '',
    state_change TEXT NOT NULL DEFAULT '',
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS department_tasks (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    department TEXT NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    status TEXT NOT NULL DEFAULT 'PENDING',
    locked_until TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS department_dead_letters (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    department TEXT NOT NULL,
    payload TEXT NOT NULL,
    error_message TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS state_machine_transitions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    entity_id TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    from_state TEXT NOT NULL,
    to_state TEXT NOT NULL,
    agent_id TEXT,
    reason TEXT,
    task_id TEXT,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    transitioned_at TIMESTAMPTZ,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_sm_entity
    ON state_machine_transitions (tenant_id, entity_id, entity_type);

CREATE TABLE IF NOT EXISTS incidents (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    description TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'OPEN',
    affected_orders JSONB NOT NULL DEFAULT '[]'::jsonb,
    affected_inventory JSONB NOT NULL DEFAULT '[]'::jsonb,
    resolution_plan JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS inbound_signals (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    source TEXT NOT NULL,
    raw_payload JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'PENDING'
        CHECK (status IN ('PENDING', 'PROCESSED', 'FAILED')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS daily_work_items (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    signal_id TEXT REFERENCES inbound_signals(id) ON DELETE SET NULL,
    intent TEXT NOT NULL,
    customer_info JSONB,
    suggested_actions JSONB,
    status TEXT NOT NULL DEFAULT 'PENDING'
        CHECK (status IN ('PENDING', 'APPROVED', 'DISMISSED')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS tool_integrations (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT,
    api_url TEXT,
    integration_code TEXT,
    status TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, id)
);

CREATE TABLE IF NOT EXISTS integration_credentials (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    integration_id TEXT NOT NULL,
    bot_token TEXT,
    api_token TEXT,
    from_phone TEXT,
    chat_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS applied_client_mutations (
    client_mutation_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS business_milestones (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    milestone_type TEXT NOT NULL,
    reached_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    shared_at TIMESTAMPTZ,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    UNIQUE (tenant_id, milestone_type)
);
