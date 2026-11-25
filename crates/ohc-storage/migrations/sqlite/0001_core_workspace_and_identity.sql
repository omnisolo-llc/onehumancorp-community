-- SPDX-License-Identifier: Apache-2.0
-- Unified Single-Tenant OSS Migration 0001 (SQLite):
-- Workspace root, local identity, RBAC, sessions, API keys, onboarding state,
-- merchant webhook idempotency, and standing/commit authority.

CREATE TABLE IF NOT EXISTS tenants (
    id TEXT PRIMARY KEY,
    owner_id TEXT,
    name TEXT NOT NULL DEFAULT '',
    industry TEXT NOT NULL DEFAULT '',
    default_currency TEXT NOT NULL DEFAULT 'USD',
    base_currency TEXT NOT NULL DEFAULT 'USD',
    enabled_currencies TEXT NOT NULL DEFAULT '["USD"]',
    locale TEXT NOT NULL DEFAULT 'en-US',
    timezone TEXT NOT NULL DEFAULT 'UTC',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL,
    email TEXT NOT NULL,
    password_hash TEXT NOT NULL DEFAULT '',
    roles TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(roles) AND json_type(roles) = 'array'),
    active BOOLEAN NOT NULL DEFAULT TRUE,
    tenant_id TEXT NOT NULL,
    marketplace_authority_key TEXT,
    marketplace_eligible BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (id, tenant_id),
    UNIQUE (username, tenant_id),
    UNIQUE (email, tenant_id)
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_users_marketplace_authority_key
    ON users (marketplace_authority_key)
    WHERE marketplace_authority_key IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS ux_users_marketplace_authority_state
    ON users (marketplace_authority_key, marketplace_eligible)
    WHERE marketplace_authority_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS roles (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    permissions TEXT NOT NULL DEFAULT '[]',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (name, tenant_id)
);

CREATE TABLE IF NOT EXISTS identity_user_roles (
    user_id TEXT NOT NULL,
    role_name TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    position INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, role_name),
    FOREIGN KEY (user_id, tenant_id) REFERENCES users(id, tenant_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_identity_user_roles_tenant_user
    ON identity_user_roles (tenant_id, user_id);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    user_agent TEXT,
    ip_address TEXT,
    expires_at TIMESTAMP NOT NULL,
    revoked_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (user_id, tenant_id) REFERENCES users(id, tenant_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_sessions_tenant_user
    ON sessions (tenant_id, user_id, expires_at);

CREATE TABLE IF NOT EXISTS revoked_tokens (
    jti TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    expires_at TIMESTAMP NOT NULL,
    PRIMARY KEY (jti, tenant_id)
);

CREATE TABLE IF NOT EXISTS auth_revoked_tokens (
    jti TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    expires_at TIMESTAMP NOT NULL,
    PRIMARY KEY (jti, tenant_id)
);

CREATE TABLE IF NOT EXISTS api_keys (
    id TEXT PRIMARY KEY,
    key_hash TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    member_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    organization_id TEXT NOT NULL,
    tenant_id TEXT,
    revoked_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TIMESTAMP,
    FOREIGN KEY (member_id, tenant_id) REFERENCES users(id, tenant_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_api_keys_hash ON api_keys (key_hash);
CREATE INDEX IF NOT EXISTS idx_api_keys_tenant ON api_keys (tenant_id);

CREATE TABLE IF NOT EXISTS application_settings (
    key TEXT NOT NULL,
    tenant_id TEXT NOT NULL DEFAULT 'default_tenant',
    value TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_by TEXT,
    PRIMARY KEY (tenant_id, key)
);

CREATE TABLE IF NOT EXISTS user_configs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    config_key TEXT NOT NULL,
    config_value TEXT NOT NULL DEFAULT '{}',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, user_id, config_key)
);

CREATE TABLE IF NOT EXISTS email_verification_challenges (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL DEFAULT 'default_tenant',
    email TEXT NOT NULL,
    code_hash TEXT NOT NULL,
    source_hash TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TIMESTAMP NOT NULL,
    resend_after TIMESTAMP NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    send_count INTEGER NOT NULL DEFAULT 0,
    verified_at TIMESTAMP,
    invitation_id TEXT,
    UNIQUE (tenant_id, email)
);

CREATE TABLE IF NOT EXISTS registration_tickets (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL DEFAULT 'default_tenant',
    email TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    issued_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TIMESTAMP NOT NULL,
    consumed_at TIMESTAMP,
    invitation_id TEXT,
    consumed_by_user_id TEXT,
    consumed_by_tenant_id TEXT
);

CREATE TABLE IF NOT EXISTS registration_invitations (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL DEFAULT 'default_tenant',
    email TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TIMESTAMP NOT NULL,
    consumed_at TIMESTAMP,
    created_by TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS identity_email_claims (
    normalized_email TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL DEFAULT 'default_tenant',
    user_id TEXT NOT NULL,
    claimed_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Canonical unified onboarding_state schema across SQLite and PostgreSQL.
-- JSON merges and monotonic step updates are performed in Rust inside the DB transaction.
CREATE TABLE IF NOT EXISTS onboarding_state (
    tenant_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    current_step INTEGER NOT NULL DEFAULT 0 CHECK (current_step >= 0),
    state_json TEXT NOT NULL DEFAULT '{}'
        CHECK (json_valid(state_json) AND json_type(state_json) = 'object'),
    preparation_receipt TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1,
    PRIMARY KEY (tenant_id, user_id)
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_onboarding_one_preparation_per_tenant
    ON onboarding_state (tenant_id)
    WHERE preparation_receipt IS NOT NULL;

CREATE TABLE IF NOT EXISTS processed_webhook_events (
    event_id TEXT PRIMARY KEY,
    provider TEXT NOT NULL DEFAULT 'stripe',
    event_type TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'processed',
    processed_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_processed_webhook_events_tenant
    ON processed_webhook_events (tenant_id, processed_at);

CREATE TABLE IF NOT EXISTS stripe_payment_settlements (
    tenant_id TEXT NOT NULL,
    settlement_key TEXT NOT NULL,
    event_id TEXT NOT NULL,
    kind TEXT NOT NULL
        CHECK (kind IN ('payment_credit', 'refund_debit', 'dispute_hold', 'dispute_won', 'dispute_lost')),
    amount_cents BIGINT NOT NULL,
    currency TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, settlement_key)
);

CREATE INDEX IF NOT EXISTS idx_stripe_payment_settlements_event
    ON stripe_payment_settlements (event_id);

CREATE TABLE IF NOT EXISTS standing_authority (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    department TEXT NOT NULL DEFAULT 'operations',
    action_scope TEXT NOT NULL,
    max_amount_micros BIGINT NOT NULL DEFAULT 0 CHECK (max_amount_micros >= 0),
    allow_external_send BOOLEAN NOT NULL DEFAULT FALSE,
    allow_financial_commit BOOLEAN NOT NULL DEFAULT FALSE,
    requires_owner_approval BOOLEAN NOT NULL DEFAULT TRUE,
    active BOOLEAN NOT NULL DEFAULT TRUE,
    expires_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, actor_id, department, action_scope)
);

CREATE INDEX IF NOT EXISTS idx_standing_authority_tenant_actor
    ON standing_authority (tenant_id, actor_id, active);

CREATE TABLE IF NOT EXISTS commit_authority (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    resource_type TEXT NOT NULL,
    action_kind TEXT NOT NULL,
    approval_digest TEXT NOT NULL,
    granted_by TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'granted'
        CHECK (status IN ('granted', 'consumed', 'revoked', 'expired')),
    expires_at TIMESTAMP,
    revoked_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_commit_authority_tenant_status
    ON commit_authority (tenant_id, actor_id, status);

CREATE TABLE IF NOT EXISTS onehumancorp_schema_versions (
    id TEXT PRIMARY KEY,
    applied_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
