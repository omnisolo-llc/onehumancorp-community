-- SPDX-License-Identifier: Apache-2.0
-- Unified Single-Tenant OSS Migration 0005 (PostgreSQL):
-- ConnectionVault encrypted BYOK credentials, local micro-USD UsageLedger,
-- workflow/assistant execution receipts, assistant workspace tables,
-- and harness runtime projections.

CREATE TABLE IF NOT EXISTS ohc_provider_connections (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    revision TEXT NOT NULL,
    state TEXT NOT NULL,
    key_id TEXT NOT NULL,
    nonce_hex TEXT NOT NULL,
    ciphertext_hex TEXT NOT NULL,
    verified_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, provider)
);

CREATE TABLE IF NOT EXISTS connection_vault (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    revision TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'verification_required',
    key_id TEXT NOT NULL,
    nonce_hex TEXT NOT NULL,
    ciphertext_hex TEXT NOT NULL,
    metadata TEXT NOT NULL DEFAULT '{}',
    verified_at BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, provider)
);

-- Local micro-USD usage accounting (single-tenant portable PostgreSQL schema).
-- No spending grant or credit pool is created by schema initialization.
CREATE TABLE IF NOT EXISTS ohc_usage_accounts (
    tenant_id TEXT PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
    limit_micros BIGINT NOT NULL CHECK (limit_micros >= 0),
    spent_micros BIGINT NOT NULL DEFAULT 0 CHECK (spent_micros >= 0),
    reserved_micros BIGINT NOT NULL DEFAULT 0 CHECK (reserved_micros >= 0)
);

CREATE TABLE IF NOT EXISTS ohc_usage_records (
    tenant_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    state TEXT NOT NULL,
    reserved_micros BIGINT NOT NULL CHECK (reserved_micros >= 0),
    charged_micros BIGINT,
    provider_cost_micros BIGINT,
    receipt_json TEXT,
    receipt_digest TEXT,
    created_at TEXT NOT NULL DEFAULT (CAST(CURRENT_TIMESTAMP AS TEXT)),
    PRIMARY KEY (tenant_id, event_id),
    FOREIGN KEY (tenant_id) REFERENCES ohc_usage_accounts(tenant_id)
);

CREATE TABLE IF NOT EXISTS ohc_usage_receipts (
    tenant_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    provider_request_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    PRIMARY KEY (tenant_id, provider, provider_request_id),
    FOREIGN KEY (tenant_id, event_id) REFERENCES ohc_usage_records(tenant_id, event_id)
);

CREATE TABLE IF NOT EXISTS usage_ledger (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    event_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    payer_mode TEXT NOT NULL DEFAULT 'byok_api'
        CHECK (payer_mode IN ('byok_api', 'local', 'unconfigured')),
    input_tokens BIGINT NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens BIGINT NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cached_tokens BIGINT NOT NULL DEFAULT 0 CHECK (cached_tokens >= 0),
    reserved_micros BIGINT NOT NULL DEFAULT 0 CHECK (reserved_micros >= 0),
    charged_micros BIGINT NOT NULL DEFAULT 0 CHECK (charged_micros >= 0),
    provider_cost_micros BIGINT NOT NULL DEFAULT 0 CHECK (provider_cost_micros >= 0),
    state TEXT NOT NULL DEFAULT 'settled',
    receipt_digest TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, event_id)
);

CREATE INDEX IF NOT EXISTS idx_usage_ledger_tenant_created
    ON usage_ledger (tenant_id, created_at DESC);

CREATE TABLE IF NOT EXISTS usage_idempotency_keys (
    key TEXT NOT NULL,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (key, tenant_id)
);

CREATE INDEX IF NOT EXISTS idx_usage_idempotency_keys_tenant_id
    ON usage_idempotency_keys (tenant_id);

CREATE TABLE IF NOT EXISTS user_usage_logs (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    organization_id TEXT NOT NULL DEFAULT '',
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    feature TEXT NOT NULL DEFAULT '',
    action_type TEXT NOT NULL DEFAULT '',
    tokens_used BIGINT NOT NULL DEFAULT 0,
    computed_cost NUMERIC(10, 4) NOT NULL DEFAULT 0.0000,
    cost_micros BIGINT NOT NULL DEFAULT 0,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_user_usage_logs_user
    ON user_usage_logs (user_id);

-- Workflow execution receipts (single-tenant portable PostgreSQL schema).
CREATE TABLE IF NOT EXISTS tenant_workflow_receipts (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    tenant_id TEXT NOT NULL CHECK (length(tenant_id) BETWEEN 1 AND 512 AND btrim(tenant_id) = tenant_id AND lower(tenant_id) <> 'system'),
    actor_id TEXT NOT NULL CHECK (length(actor_id) BETWEEN 1 AND 512),
    request_id TEXT NOT NULL CHECK (length(request_id) = 36),
    fingerprint TEXT NOT NULL CHECK (length(fingerprint) = 64 AND fingerprint ~ '^[0-9a-f]{64}$'),
    payload TEXT NOT NULL CHECK (octet_length(payload) <= 200000),
    token_id TEXT NOT NULL CHECK (length(token_id) BETWEEN 1 AND 512),
    token_expires_at BIGINT NOT NULL CHECK (token_expires_at > created_at),
    session_id TEXT CHECK (session_id IS NULL OR length(session_id) BETWEEN 1 AND 512),
    phase TEXT NOT NULL CHECK (phase IN ('queued', 'dispatching', 'completed', 'cancelled', 'outcome_unknown')),
    generation BIGINT NOT NULL CHECK (generation >= 0),
    lease TEXT CHECK (lease IS NULL OR length(lease) = 36),
    lease_expires_at BIGINT CHECK (lease_expires_at IS NULL OR lease_expires_at > created_at),
    created_at BIGINT NOT NULL CHECK (created_at > 0),
    updated_at BIGINT NOT NULL CHECK (updated_at >= created_at),
    output TEXT CHECK (output IS NULL OR octet_length(output) <= 64000),
    error TEXT CHECK (error IS NULL OR error IN ('authority_lost', 'cancelled', 'execution_uncertain', 'lease_expired')),
    UNIQUE (id, tenant_id, actor_id),
    UNIQUE (tenant_id, actor_id, request_id),
    FOREIGN KEY (actor_id, tenant_id) REFERENCES users(id, tenant_id) ON DELETE CASCADE,
    CHECK ((lease IS NULL) = (lease_expires_at IS NULL)),
    CHECK ((phase = 'queued' AND generation = 0 AND lease IS NULL)
        OR (phase = 'dispatching' AND generation = 1 AND lease IS NOT NULL)
        OR (phase = 'completed' AND generation = 2 AND lease IS NOT NULL)
        OR (phase = 'outcome_unknown' AND generation = 2 AND lease IS NOT NULL)
        OR (phase = 'cancelled' AND ((generation = 1 AND lease IS NULL) OR (generation = 2 AND lease IS NOT NULL)))),
    CHECK ((phase = 'completed' AND output IS NOT NULL AND length(btrim(output)) > 0 AND error IS NULL AND updated_at < lease_expires_at)
        OR (phase IN ('queued', 'dispatching') AND output IS NULL AND error IS NULL)
        OR (phase IN ('cancelled', 'outcome_unknown') AND output IS NULL AND error IS NOT NULL))
);

CREATE OR REPLACE FUNCTION tenant_receipt_admission_fn()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.phase <> 'queued' OR NEW.generation <> 0 OR NEW.lease IS NOT NULL THEN
        RAISE EXCEPTION 'receipt_requires_queued_admission';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS tenant_receipt_admission ON tenant_workflow_receipts;
CREATE TRIGGER tenant_receipt_admission
    BEFORE INSERT ON tenant_workflow_receipts
    FOR EACH ROW
    EXECUTE FUNCTION tenant_receipt_admission_fn();

CREATE OR REPLACE FUNCTION tenant_receipt_update_fn()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.id IS DISTINCT FROM OLD.id
        OR NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
        OR NEW.actor_id IS DISTINCT FROM OLD.actor_id
        OR NEW.request_id IS DISTINCT FROM OLD.request_id
        OR NEW.fingerprint IS DISTINCT FROM OLD.fingerprint
        OR NEW.payload IS DISTINCT FROM OLD.payload
        OR NEW.token_id IS DISTINCT FROM OLD.token_id
        OR NEW.token_expires_at IS DISTINCT FROM OLD.token_expires_at
        OR NEW.session_id IS DISTINCT FROM OLD.session_id
        OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION 'receipt_admission_is_immutable';
    END IF;
    IF NOT ((OLD.phase = 'queued' AND NEW.phase IN ('dispatching', 'cancelled'))
        OR (OLD.phase = 'dispatching' AND NEW.phase IN ('completed', 'cancelled', 'outcome_unknown')))
        OR NEW.generation <> OLD.generation + 1
        OR NEW.updated_at < OLD.updated_at
        OR (OLD.lease IS NOT NULL AND (NEW.lease IS DISTINCT FROM OLD.lease OR NEW.lease_expires_at IS DISTINCT FROM OLD.lease_expires_at))
    THEN
        RAISE EXCEPTION 'receipt_transition_is_invalid';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS tenant_receipt_transition ON tenant_workflow_receipts;
CREATE TRIGGER tenant_receipt_transition
    BEFORE UPDATE ON tenant_workflow_receipts
    FOR EACH ROW
    EXECUTE FUNCTION tenant_receipt_update_fn();

CREATE OR REPLACE FUNCTION tenant_receipt_delete_fn()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF EXISTS (SELECT 1 FROM users WHERE id = OLD.actor_id AND tenant_id = OLD.tenant_id) THEN
        RAISE EXCEPTION 'receipt_cannot_be_discarded';
    END IF;
    RETURN OLD;
END;
$$;

DROP TRIGGER IF EXISTS tenant_receipt_delete ON tenant_workflow_receipts;
CREATE TRIGGER tenant_receipt_delete
    BEFORE DELETE ON tenant_workflow_receipts
    FOR EACH ROW
    EXECUTE FUNCTION tenant_receipt_delete_fn();

CREATE TABLE IF NOT EXISTS assistant_execution_tasks (
    id TEXT PRIMARY KEY NOT NULL,
    tenant_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    root_request_id TEXT NOT NULL,
    current_receipt_id TEXT NOT NULL,
    workspace TEXT NOT NULL CHECK (length(btrim(workspace)) BETWEEN 1 AND 80),
    title TEXT NOT NULL CHECK (length(btrim(title)) BETWEEN 1 AND 160),
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    created_at BIGINT NOT NULL CHECK (created_at > 0),
    updated_at BIGINT NOT NULL CHECK (updated_at >= created_at),
    UNIQUE (id, tenant_id, actor_id),
    UNIQUE (tenant_id, actor_id, root_request_id),
    FOREIGN KEY (id, tenant_id, actor_id) REFERENCES tenant_workflow_receipts(id, tenant_id, actor_id) ON DELETE CASCADE,
    FOREIGN KEY (current_receipt_id, tenant_id, actor_id) REFERENCES tenant_workflow_receipts(id, tenant_id, actor_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS assistant_execution_attempts (
    receipt_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    source_receipt_id TEXT,
    UNIQUE (task_id, source_receipt_id),
    FOREIGN KEY (task_id, tenant_id, actor_id) REFERENCES assistant_execution_tasks(id, tenant_id, actor_id) ON DELETE CASCADE,
    FOREIGN KEY (receipt_id, tenant_id, actor_id) REFERENCES tenant_workflow_receipts(id, tenant_id, actor_id) ON DELETE CASCADE,
    FOREIGN KEY (source_receipt_id, tenant_id, actor_id) REFERENCES tenant_workflow_receipts(id, tenant_id, actor_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS assistant_workspaces (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    default_work_dir TEXT,
    default_model TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS assistant_tasks (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    workspace_id TEXT NOT NULL,
    title TEXT NOT NULL,
    prompt TEXT NOT NULL,
    status TEXT NOT NULL,
    mode TEXT,
    permission_profile TEXT NOT NULL,
    model_config JSONB,
    current_step TEXT,
    archived BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS assistant_messages (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    task_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    tool_metadata JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS assistant_artifacts (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    task_id TEXT NOT NULL,
    type TEXT NOT NULL,
    filename TEXT NOT NULL,
    path TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    size BIGINT,
    preview_ref TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS assistant_connectors (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'custom',
    status TEXT NOT NULL,
    oauth BOOLEAN NOT NULL DEFAULT FALSE,
    config JSONB,
    last_error TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, name)
);

CREATE TABLE IF NOT EXISTS assistant_file_changes (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    task_id TEXT NOT NULL,
    path TEXT NOT NULL,
    change_type TEXT NOT NULL,
    summary TEXT,
    approval_status TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS assistant_memory_records (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    content TEXT NOT NULL,
    scope TEXT NOT NULL DEFAULT 'global',
    source TEXT,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS assistant_skills (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    category TEXT NOT NULL DEFAULT 'Custom',
    source TEXT NOT NULL DEFAULT 'database',
    status TEXT NOT NULL,
    version TEXT,
    description TEXT,
    config JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, name)
);

CREATE TABLE IF NOT EXISTS harness_sessions (
    session_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    project_id TEXT,
    workspace_id TEXT,
    title TEXT,
    labels JSONB NOT NULL DEFAULT '[]'::jsonb,
    tags JSONB NOT NULL DEFAULT '{}'::jsonb,
    state TEXT NOT NULL DEFAULT 'open',
    state_version BIGINT NOT NULL DEFAULT 0,
    parent_session_id TEXT,
    root_session_id TEXT NOT NULL,
    fork_source_event_id TEXT,
    active_task_id TEXT,
    retention_class TEXT,
    data_classification TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_active_at TIMESTAMPTZ,
    extensions JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX IF NOT EXISTS idx_harness_sessions_tenant
    ON harness_sessions (tenant_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS harness_tasks (
    task_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    parent_task_id TEXT,
    parent_attempt_id TEXT,
    kind TEXT NOT NULL,
    objective TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'queued',
    state_version BIGINT NOT NULL DEFAULT 0,
    dependency_task_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    owner_actor JSONB,
    input_message_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    output_message_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    artifact_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    terminal_result JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    extensions JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS harness_turns (
    turn_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    task_id TEXT NOT NULL REFERENCES harness_tasks(task_id) ON DELETE CASCADE,
    sequence BIGINT NOT NULL,
    state TEXT NOT NULL DEFAULT 'queued',
    state_version BIGINT NOT NULL DEFAULT 0,
    input_message_id TEXT,
    output_message_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    stop_reason TEXT,
    error_code TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMPTZ,
    extensions JSONB NOT NULL DEFAULT '{}'::jsonb,
    UNIQUE (task_id, sequence)
);
