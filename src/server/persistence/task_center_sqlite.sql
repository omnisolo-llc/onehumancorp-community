-- Portable SQLite DDL matching src/server/migrations/1046_task_center.sql.

CREATE TABLE IF NOT EXISTS task_center_spaces (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 80),
    description TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL DEFAULT 'custom'
        CHECK (kind IN ('operations', 'client_delivery', 'cash_recovery', 'research', 'custom')),
    prompt_template TEXT NOT NULL DEFAULT '',
    department TEXT NOT NULL DEFAULT 'operations',
    reads_json TEXT NOT NULL DEFAULT '[]',
    shows_json TEXT NOT NULL DEFAULT '[]',
    cannot_json TEXT NOT NULL DEFAULT '[]',
    source_health TEXT NOT NULL DEFAULT 'unconfigured'
        CHECK (source_health IN ('live', 'offline', 'revoked', 'unconfigured')),
    pinned_artifact_ids_json TEXT NOT NULL DEFAULT '[]',
    archived BOOLEAN NOT NULL DEFAULT FALSE,
    created_at_unix BIGINT NOT NULL CHECK (created_at_unix >= 0),
    updated_at_unix BIGINT NOT NULL CHECK (updated_at_unix >= created_at_unix),
    UNIQUE (tenant_id, id)
);

CREATE TABLE IF NOT EXISTS task_center_tasks (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    actor_id TEXT NOT NULL DEFAULT '',
    space_id TEXT,
    title TEXT NOT NULL CHECK (length(trim(title)) BETWEEN 1 AND 160),
    prompt TEXT NOT NULL,
    department TEXT NOT NULL DEFAULT 'operations',
    requested_action TEXT NOT NULL DEFAULT 'run_task',
    status TEXT NOT NULL
        CHECK (status IN ('draft', 'queued', 'running', 'awaiting_approval', 'waiting_for_input', 'completed', 'failed', 'cancelled', 'outcome_unknown')),
    provenance TEXT NOT NULL
        CHECK (provenance IN ('draft', 'awaiting_approval', 'verified', 'failed', 'cancelled', 'outcome_unknown', 'unconfigured')),
    attention TEXT NOT NULL DEFAULT 'none'
        CHECK (attention IN ('none', 'permission_required', 'needs_input', 'blocked', 'completed_unread')),
    risk TEXT NOT NULL DEFAULT 'draft_for_review'
        CHECK (risk IN ('auto_execute', 'draft_for_review')),
    payer_mode TEXT NOT NULL DEFAULT 'unconfigured'
        CHECK (payer_mode IN ('managed_api', 'byok_api', 'local', 'unconfigured')),
    estimated_cost_micros BIGINT NOT NULL DEFAULT 0 CHECK (estimated_cost_micros >= 0),
    actual_cost_micros BIGINT CHECK (actual_cost_micros IS NULL OR actual_cost_micros >= 0),
    idempotency_key TEXT NOT NULL,
    request_digest TEXT NOT NULL DEFAULT '',
    receipt_id TEXT,
    usage_event_id TEXT,
    output_summary TEXT,
    error_message TEXT,
    archived BOOLEAN NOT NULL DEFAULT FALSE,
    created_at_unix BIGINT NOT NULL CHECK (created_at_unix >= 0),
    updated_at_unix BIGINT NOT NULL CHECK (updated_at_unix >= created_at_unix),
    UNIQUE (tenant_id, id),
    UNIQUE (tenant_id, idempotency_key),
    FOREIGN KEY (tenant_id, space_id) REFERENCES task_center_spaces(tenant_id, id)
);

CREATE TABLE IF NOT EXISTS task_center_steps (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    seq INTEGER NOT NULL CHECK (seq > 0),
    kind TEXT NOT NULL
        CHECK (kind IN ('plan', 'tool_call', 'llm_turn', 'approval_gate', 'artifact', 'system')),
    title TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL
        CHECK (status IN ('draft', 'queued', 'running', 'awaiting_approval', 'waiting_for_input', 'completed', 'failed', 'cancelled', 'outcome_unknown')),
    provenance TEXT NOT NULL
        CHECK (provenance IN ('draft', 'awaiting_approval', 'verified', 'failed', 'cancelled', 'outcome_unknown', 'unconfigured')),
    tool_name TEXT,
    cost_micros BIGINT NOT NULL DEFAULT 0 CHECK (cost_micros >= 0),
    created_at_unix BIGINT NOT NULL CHECK (created_at_unix >= 0),
    UNIQUE (tenant_id, task_id, seq),
    FOREIGN KEY (tenant_id, task_id) REFERENCES task_center_tasks(tenant_id, id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS task_center_artifacts (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    space_id TEXT,
    title TEXT NOT NULL,
    format TEXT NOT NULL
        CHECK (format IN ('markdown', 'table', 'checklist', 'space_card', 'json')),
    content TEXT NOT NULL CHECK (length(CAST(content AS BLOB)) <= 65536),
    provenance TEXT NOT NULL
        CHECK (provenance IN ('draft', 'awaiting_approval', 'verified', 'failed', 'cancelled', 'outcome_unknown', 'unconfigured')),
    pinned BOOLEAN NOT NULL DEFAULT FALSE,
    created_at_unix BIGINT NOT NULL CHECK (created_at_unix >= 0),
    FOREIGN KEY (tenant_id, task_id) REFERENCES task_center_tasks(tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, space_id) REFERENCES task_center_spaces(tenant_id, id)
);

CREATE TABLE IF NOT EXISTS task_center_approvals (
    approval_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    action_kind TEXT NOT NULL,
    department TEXT NOT NULL DEFAULT 'operations',
    risk TEXT NOT NULL DEFAULT 'draft_for_review'
        CHECK (risk IN ('auto_execute', 'draft_for_review')),
    summary TEXT NOT NULL,
    payload_preview_json TEXT NOT NULL DEFAULT '{}',
    payload_sha256 TEXT NOT NULL CHECK (length(payload_sha256) = 64),
    estimated_cost_micros BIGINT NOT NULL DEFAULT 0 CHECK (estimated_cost_micros >= 0),
    standing_limit_micros BIGINT NOT NULL DEFAULT 0 CHECK (standing_limit_micros >= 0),
    status TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved', 'rejected', 'expired')),
    decided_by TEXT,
    decided_at_unix BIGINT,
    decision_reason TEXT,
    created_at_unix BIGINT NOT NULL CHECK (created_at_unix >= 0),
    UNIQUE (tenant_id, approval_id),
    FOREIGN KEY (tenant_id, task_id) REFERENCES task_center_tasks(tenant_id, id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS task_center_standing_policies (
    tenant_id TEXT NOT NULL,
    department TEXT NOT NULL,
    auto_approve_max_micros BIGINT NOT NULL DEFAULT 0 CHECK (auto_approve_max_micros >= 0),
    allow_external_send BOOLEAN NOT NULL DEFAULT FALSE,
    allow_financial_commit BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at_unix BIGINT NOT NULL CHECK (updated_at_unix >= 0),
    PRIMARY KEY (tenant_id, department)
);

CREATE INDEX IF NOT EXISTS idx_task_center_spaces_tenant_updated
    ON task_center_spaces(tenant_id, updated_at_unix DESC);
CREATE INDEX IF NOT EXISTS idx_task_center_tasks_tenant_updated
    ON task_center_tasks(tenant_id, updated_at_unix DESC);
CREATE INDEX IF NOT EXISTS idx_task_center_tasks_tenant_space
    ON task_center_tasks(tenant_id, space_id);
CREATE INDEX IF NOT EXISTS idx_task_center_steps_tenant_task_seq
    ON task_center_steps(tenant_id, task_id, seq ASC);
CREATE INDEX IF NOT EXISTS idx_task_center_artifacts_tenant_task
    ON task_center_artifacts(tenant_id, task_id, created_at_unix ASC);
CREATE UNIQUE INDEX IF NOT EXISTS ux_task_center_approvals_pending
    ON task_center_approvals(tenant_id, task_id) WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS idx_task_center_approvals_tenant_status
    ON task_center_approvals(tenant_id, status, created_at_unix DESC);
