-- SPDX-License-Identifier: Apache-2.0
-- Unified Single-Tenant OSS Migration 0005 (SQLite):
-- ConnectionVault encrypted BYOK credentials, local micro-USD UsageLedger,
-- workflow/assistant execution receipts, assistant workspace tables,
-- agent definition catalogue, and harness runtime projections.

CREATE TABLE IF NOT EXISTS ohc_provider_connections (
    tenant_id TEXT NOT NULL,
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
    tenant_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    revision TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'verification_required',
    key_id TEXT NOT NULL,
    nonce_hex TEXT NOT NULL,
    ciphertext_hex TEXT NOT NULL,
    metadata TEXT NOT NULL DEFAULT '{}',
    verified_at BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, provider)
);

-- Local micro-USD usage accounting (matching usage_ledger_sqlite.sql).
-- No spending grant or credit pool is created by schema initialization.
CREATE TABLE IF NOT EXISTS ohc_usage_accounts (
    tenant_id TEXT PRIMARY KEY,
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
    tenant_id TEXT NOT NULL,
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
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, event_id)
);

CREATE INDEX IF NOT EXISTS idx_usage_ledger_tenant_created
    ON usage_ledger (tenant_id, created_at DESC);

CREATE TABLE IF NOT EXISTS usage_idempotency_keys (
    key TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (key, tenant_id)
);

CREATE INDEX IF NOT EXISTS idx_usage_idempotency_keys_tenant_id
    ON usage_idempotency_keys (tenant_id);

CREATE TABLE IF NOT EXISTS user_usage_logs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT,
    organization_id TEXT NOT NULL DEFAULT '',
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    feature TEXT NOT NULL DEFAULT '',
    action_type TEXT NOT NULL DEFAULT '',
    tokens_used BIGINT NOT NULL DEFAULT 0,
    computed_cost REAL NOT NULL DEFAULT 0.0,
    cost_micros BIGINT NOT NULL DEFAULT 0,
    metadata TEXT NOT NULL DEFAULT '{}',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_user_usage_logs_user
    ON user_usage_logs (user_id);

-- Workflow execution receipts (matching tenant_execution_receipts_sqlite.sql).
CREATE TABLE IF NOT EXISTS tenant_workflow_receipts (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 36),
    tenant_id TEXT NOT NULL CHECK (length(tenant_id) BETWEEN 1 AND 512 AND trim(tenant_id) = tenant_id AND lower(tenant_id) <> 'system'),
    actor_id TEXT NOT NULL CHECK (length(actor_id) BETWEEN 1 AND 512),
    request_id TEXT NOT NULL CHECK (length(request_id) = 36),
    fingerprint TEXT NOT NULL CHECK (length(fingerprint) = 64 AND fingerprint NOT GLOB '*[^a-f0-9]*'),
    payload TEXT NOT NULL CHECK (json_valid(payload) AND json_type(payload) = 'object' AND length(CAST(payload AS BLOB)) <= 200000),
    token_id TEXT NOT NULL CHECK (length(token_id) BETWEEN 1 AND 512),
    token_expires_at BIGINT NOT NULL CHECK (token_expires_at > created_at),
    session_id TEXT CHECK (session_id IS NULL OR length(session_id) BETWEEN 1 AND 512),
    phase TEXT NOT NULL CHECK (phase IN ('queued', 'dispatching', 'completed', 'cancelled', 'outcome_unknown')),
    generation INTEGER NOT NULL CHECK (generation >= 0),
    lease TEXT CHECK (lease IS NULL OR length(lease) = 36),
    lease_expires_at BIGINT CHECK (lease_expires_at IS NULL OR lease_expires_at > created_at),
    created_at BIGINT NOT NULL CHECK (created_at > 0),
    updated_at BIGINT NOT NULL CHECK (updated_at >= created_at),
    output TEXT CHECK (output IS NULL OR length(CAST(output AS BLOB)) <= 64000),
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
    CHECK ((phase = 'completed' AND output IS NOT NULL AND length(trim(output)) > 0 AND error IS NULL AND updated_at < lease_expires_at)
        OR (phase IN ('queued', 'dispatching') AND output IS NULL AND error IS NULL)
        OR (phase IN ('cancelled', 'outcome_unknown') AND output IS NULL AND error IS NOT NULL))
);

CREATE TRIGGER IF NOT EXISTS tenant_receipt_admission BEFORE INSERT ON tenant_workflow_receipts
WHEN NEW.phase <> 'queued' OR NEW.generation <> 0 OR NEW.lease IS NOT NULL
 OR EXISTS (
     SELECT 1 FROM tenant_workflow_receipts
     WHERE id = NEW.id
        OR (tenant_id = NEW.tenant_id AND actor_id = NEW.actor_id AND request_id = NEW.request_id)
 )
BEGIN
    SELECT RAISE(ABORT, 'receipt_requires_queued_admission');
END;

CREATE TRIGGER IF NOT EXISTS tenant_receipt_immutable_identity BEFORE UPDATE ON tenant_workflow_receipts
WHEN NEW.id IS NOT OLD.id OR NEW.tenant_id IS NOT OLD.tenant_id OR NEW.actor_id IS NOT OLD.actor_id
 OR NEW.request_id IS NOT OLD.request_id OR NEW.fingerprint IS NOT OLD.fingerprint OR NEW.payload IS NOT OLD.payload
 OR NEW.token_id IS NOT OLD.token_id OR NEW.token_expires_at IS NOT OLD.token_expires_at
 OR NEW.session_id IS NOT OLD.session_id OR NEW.created_at IS NOT OLD.created_at
BEGIN
    SELECT RAISE(ABORT, 'receipt_admission_is_immutable');
END;

CREATE TRIGGER IF NOT EXISTS tenant_receipt_transition BEFORE UPDATE ON tenant_workflow_receipts
WHEN NOT ((OLD.phase = 'queued' AND NEW.phase IN ('dispatching', 'cancelled'))
 OR (OLD.phase = 'dispatching' AND NEW.phase IN ('completed', 'cancelled', 'outcome_unknown')))
 OR NEW.generation <> OLD.generation + 1 OR NEW.updated_at < OLD.updated_at
 OR (OLD.lease IS NOT NULL AND (NEW.lease IS NOT OLD.lease OR NEW.lease_expires_at IS NOT OLD.lease_expires_at))
BEGIN
    SELECT RAISE(ABORT, 'receipt_transition_is_invalid');
END;

CREATE TRIGGER IF NOT EXISTS tenant_receipt_delete BEFORE DELETE ON tenant_workflow_receipts
WHEN EXISTS (SELECT 1 FROM users WHERE id = OLD.actor_id AND tenant_id = OLD.tenant_id)
BEGIN
    SELECT RAISE(ABORT, 'receipt_cannot_be_discarded');
END;

-- Assistant execution task/attempt links (matching assistant_execution_sqlite.sql).
CREATE TABLE IF NOT EXISTS assistant_execution_tasks (
    id TEXT PRIMARY KEY NOT NULL,
    tenant_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    root_request_id TEXT NOT NULL,
    current_receipt_id TEXT NOT NULL,
    workspace TEXT NOT NULL CHECK (length(trim(workspace)) BETWEEN 1 AND 80),
    title TEXT NOT NULL CHECK (length(trim(title)) BETWEEN 1 AND 160),
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
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    default_work_dir TEXT,
    default_model TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS assistant_tasks (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    title TEXT NOT NULL,
    prompt TEXT NOT NULL,
    status TEXT NOT NULL,
    mode TEXT,
    permission_profile TEXT NOT NULL,
    model_config TEXT,
    current_step TEXT,
    archived BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS assistant_messages (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    tool_metadata TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS assistant_artifacts (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    type TEXT NOT NULL,
    filename TEXT NOT NULL,
    path TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    size BIGINT,
    preview_ref TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS assistant_connectors (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT 'custom',
    status TEXT NOT NULL,
    oauth BOOLEAN NOT NULL DEFAULT FALSE,
    config TEXT,
    last_error TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, name)
);

CREATE TABLE IF NOT EXISTS assistant_file_changes (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    path TEXT NOT NULL,
    change_type TEXT NOT NULL,
    summary TEXT,
    approval_status TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS assistant_memory_records (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    content TEXT NOT NULL,
    scope TEXT NOT NULL DEFAULT 'global',
    source TEXT,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS assistant_skills (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    category TEXT NOT NULL DEFAULT 'Custom',
    source TEXT NOT NULL DEFAULT 'database',
    status TEXT NOT NULL,
    version TEXT,
    description TEXT,
    config TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, name)
);

CREATE TABLE IF NOT EXISTS harness_sessions (
    session_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    project_id TEXT,
    workspace_id TEXT,
    title TEXT,
    labels TEXT NOT NULL DEFAULT '[]',
    tags TEXT NOT NULL DEFAULT '{}',
    state TEXT NOT NULL DEFAULT 'open',
    state_version BIGINT NOT NULL DEFAULT 0,
    parent_session_id TEXT,
    root_session_id TEXT NOT NULL,
    fork_source_event_id TEXT,
    active_task_id TEXT,
    retention_class TEXT,
    data_classification TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_active_at TIMESTAMP,
    extensions TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX IF NOT EXISTS idx_harness_sessions_tenant
    ON harness_sessions (tenant_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS harness_tasks (
    task_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    parent_task_id TEXT,
    parent_attempt_id TEXT,
    kind TEXT NOT NULL,
    objective TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'queued',
    state_version BIGINT NOT NULL DEFAULT 0,
    dependency_task_ids TEXT NOT NULL DEFAULT '[]',
    owner_actor TEXT,
    input_message_ids TEXT NOT NULL DEFAULT '[]',
    output_message_ids TEXT NOT NULL DEFAULT '[]',
    artifact_ids TEXT NOT NULL DEFAULT '[]',
    terminal_result TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    started_at TIMESTAMP,
    finished_at TIMESTAMP,
    extensions TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS harness_turns (
    turn_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    task_id TEXT NOT NULL REFERENCES harness_tasks(task_id) ON DELETE CASCADE,
    sequence BIGINT NOT NULL,
    state TEXT NOT NULL DEFAULT 'queued',
    state_version BIGINT NOT NULL DEFAULT 0,
    input_message_id TEXT,
    output_message_ids TEXT NOT NULL DEFAULT '[]',
    stop_reason TEXT,
    error_code TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMP,
    extensions TEXT NOT NULL DEFAULT '{}',
    UNIQUE (task_id, sequence)
);
