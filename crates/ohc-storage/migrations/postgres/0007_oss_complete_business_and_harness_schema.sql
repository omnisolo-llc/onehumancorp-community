-- SPDX-License-Identifier: Apache-2.0
-- Unified Single-Tenant OSS Migration 0007 (PostgreSQL):
-- Complete OSS business domain, field ops, catalog, ledger, harness middleware,
-- and workspace parity columns synchronized cleanly with SQLite.

CREATE TABLE IF NOT EXISTS affiliate_ledgers (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    total_earnings NUMERIC(10, 2) DEFAULT 0.00,
    available_balance NUMERIC(10, 2) DEFAULT 0.00,
    commission_amount BIGINT DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(tenant_id, customer_id)
);

CREATE TABLE IF NOT EXISTS affiliate_links (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    code TEXT UNIQUE NOT NULL,
    discount_percentage INTEGER DEFAULT 20,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS agent_actions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    interaction_id TEXT,
    action_type TEXT NOT NULL,
    payload JSONB DEFAULT '{}',
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    _sync_status TEXT DEFAULT 'pending',
    version INTEGER DEFAULT 1
);

CREATE TABLE IF NOT EXISTS agent_definition_authorities (
    authority_key TEXT PRIMARY KEY,
    eligible BOOLEAN NOT NULL
);

CREATE TABLE IF NOT EXISTS agent_definition_operations (
    tenant_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    receipt TEXT NOT NULL,
    PRIMARY KEY(tenant_id,user_id,request_id),
    FOREIGN KEY(user_id,tenant_id) REFERENCES users(id,tenant_id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS agent_definitions (
    id TEXT NOT NULL,
    version BIGINT NOT NULL CHECK(version > 0),
    digest TEXT NOT NULL CHECK(length(digest) = 64),
    document TEXT NOT NULL,
    search_text TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'community' CHECK(source IN ('first_party','community')),
    authority_key TEXT,
    CHECK((source='first_party' AND authority_key IS NULL) OR (source='community' AND authority_key IS NOT NULL)),
    PRIMARY KEY(id,version)
);

CREATE TABLE IF NOT EXISTS agent_inbox (
    seq SERIAL PRIMARY KEY,
    agent_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    from_agent TEXT NOT NULL,
    to_agent TEXT NOT NULL DEFAULT '',
    type TEXT NOT NULL,
    content TEXT NOT NULL DEFAULT '',
    meeting_id TEXT NOT NULL DEFAULT '',
    occurred_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS agent_kv_store (
    tenant_id VARCHAR(255) NOT NULL,
    kv_key VARCHAR(255) NOT NULL,
    kv_value TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, kv_key)
);

CREATE TABLE IF NOT EXISTS agent_session_summaries (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    turn_index INTEGER NOT NULL,
    summary_embedding TEXT,
    raw_state TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS agent_violations (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    violation_type TEXT NOT NULL,
    details TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    _sync_status TEXT DEFAULT 'pending',
    version INTEGER DEFAULT 1
);

CREATE TABLE IF NOT EXISTS availability_ledger (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    product_id TEXT REFERENCES products(id) ON DELETE CASCADE,
    start_time TIMESTAMPTZ NOT NULL,
    end_time TIMESTAMPTZ NOT NULL,
    status TEXT NOT NULL DEFAULT 'AVAILABLE',
    booking_id TEXT REFERENCES bookings(id) ON DELETE SET NULL,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS availability_schedules (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    business_hours JSONB NOT NULL DEFAULT '{}',
    exceptions JSONB NOT NULL DEFAULT '[]',
    timezone TEXT NOT NULL DEFAULT 'UTC',
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS builder_brand_toolboxes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id UUID NOT NULL,
    name TEXT NOT NULL,
    source_description TEXT NOT NULL DEFAULT '',
    toolbox JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS builder_publications (
    publication_id UUID PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    owner_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    operation_id UUID NOT NULL,
    site_id UUID NOT NULL,
    requested_site_id UUID,
    site_version BIGINT NOT NULL CHECK (site_version > 0),
    snapshot JSONB NOT NULL,
    snapshot_sha256 TEXT NOT NULL CHECK (snapshot_sha256 ~ '^[0-9a-f]{64}$'),
    product_ids TEXT[] NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','processing','published','failed','revoked')),
    lease_token UUID,
    lease_until TIMESTAMPTZ,
    rendered_pages JSONB,
    rendered_sha256 TEXT,
    error_code TEXT,
    revoked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, owner_id, operation_id),
    UNIQUE (publication_id, tenant_id),
    UNIQUE (publication_id, tenant_id, site_id),
    UNIQUE (site_id, site_version),
    UNIQUE (site_id, publication_id),
    FOREIGN KEY (site_id, tenant_id) REFERENCES builder_sites(id, publication_tenant_id) ON DELETE CASCADE,
    CHECK (status <> 'processing' OR (lease_token IS NOT NULL AND lease_until IS NOT NULL)),
    CHECK (status <> 'published' OR (rendered_pages IS NOT NULL AND rendered_sha256 IS NOT NULL))
);

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'builder_current_publication_belongs_to_site'
    ) THEN
        ALTER TABLE builder_sites
            ADD CONSTRAINT builder_current_publication_belongs_to_site
            FOREIGN KEY (id, current_publication_id)
            REFERENCES builder_publications(site_id, publication_id)
            ON DELETE SET NULL (current_publication_id);
    END IF;
END
$$;

CREATE INDEX IF NOT EXISTS idx_builder_publications_pending
    ON builder_publications (status, lease_until, created_at);

CREATE OR REPLACE FUNCTION protect_builder_publication_snapshot()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.status = 'published' AND NEW.status NOT IN ('published', 'revoked') THEN
        RAISE EXCEPTION 'Published receipt cannot be reopened';
    END IF;
    IF OLD.status = 'revoked' AND ROW(NEW.status, NEW.revoked_at) IS DISTINCT FROM ROW(OLD.status, OLD.revoked_at) THEN
        RAISE EXCEPTION 'Publication revocation is final';
    END IF;
    IF ROW(NEW.publication_id, NEW.tenant_id, NEW.owner_id, NEW.operation_id, NEW.site_id, NEW.requested_site_id, NEW.site_version, NEW.snapshot, NEW.snapshot_sha256, NEW.product_ids, NEW.created_at)
       IS DISTINCT FROM
       ROW(OLD.publication_id, OLD.tenant_id, OLD.owner_id, OLD.operation_id, OLD.site_id, OLD.requested_site_id, OLD.site_version, OLD.snapshot, OLD.snapshot_sha256, OLD.product_ids, OLD.created_at)
    THEN
        RAISE EXCEPTION 'Publication snapshot identity is immutable';
    END IF;
    IF OLD.status IN ('published', 'revoked') AND ROW(NEW.rendered_pages, NEW.rendered_sha256) IS DISTINCT FROM ROW(OLD.rendered_pages, OLD.rendered_sha256) THEN
        RAISE EXCEPTION 'Published rendering is immutable';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS protect_builder_publication_snapshot ON builder_publications;
CREATE TRIGGER protect_builder_publication_snapshot
    BEFORE UPDATE ON builder_publications
    FOR EACH ROW
    EXECUTE FUNCTION protect_builder_publication_snapshot();

CREATE OR REPLACE FUNCTION protect_builder_publication_tenant()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.publication_tenant_id IS DISTINCT FROM OLD.publication_tenant_id
       OR (OLD.publication_tenant_id IS NOT NULL AND NEW.tenant_id IS DISTINCT FROM OLD.tenant_id)
    THEN
        RAISE EXCEPTION 'Publication site tenant binding is immutable';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS protect_builder_publication_tenant ON builder_sites;
CREATE TRIGGER protect_builder_publication_tenant
    BEFORE UPDATE ON builder_sites
    FOR EACH ROW
    EXECUTE FUNCTION protect_builder_publication_tenant();

CREATE TABLE IF NOT EXISTS calendar_integrations (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    access_token TEXT NOT NULL,
    refresh_token TEXT,
    expires_at TIMESTAMPTZ,
    sync_metadata JSONB DEFAULT '{}',
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS campaigns (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    goal TEXT NOT NULL,
    status TEXT NOT NULL,
    start_time TIMESTAMPTZ,
    end_time TIMESTAMPTZ,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS chat_contacts (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name VARCHAR(255),
    email VARCHAR(255),
    phone VARCHAR(255),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS chat_inboxes (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name VARCHAR(255) NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS checkout_sessions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    type TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'PENDING',
    amount_cents BIGINT NOT NULL,
    device_id TEXT,
    cart_payload JSONB,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS competitor_metrics (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    competitor_name TEXT NOT NULL,
    metrics_data TEXT NOT NULL,
    probed_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    _sync_status TEXT DEFAULT 'pending',
    version INTEGER DEFAULT 1
);

CREATE TABLE IF NOT EXISTS conversational_checkout_sessions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    type TEXT NOT NULL,
    amount BIGINT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    inventory_lock_id TEXT,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS conversational_intakes (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    customer_id TEXT,
    inbox_message_id TEXT,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'quote_sent', 'payment_pending', 'confirmed')),
    context TEXT,
    service_name TEXT,
    suggested_price DOUBLE PRECISION,
    suggested_time TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS currencies (
    code TEXT PRIMARY KEY,
    exchange_rate DECIMAL NOT NULL DEFAULT 1.0,
    last_updated TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS customer_profile (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS delivery_zones (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL,
    polygon TEXT,
    flat_fee_cents BIGINT NOT NULL DEFAULT 0,
    min_order_value_cents BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS distributed_locks (
    id TEXT PRIMARY KEY,
    lock_val TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE IF NOT EXISTS edge_ledger_transactions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    transaction_id TEXT NOT NULL,
    amount_cents BIGINT NOT NULL,
    currency TEXT NOT NULL DEFAULT 'USD',
    status TEXT NOT NULL DEFAULT 'PENDING',
    device_signature TEXT,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    synced_at TIMESTAMPTZ,
    UNIQUE (tenant_id, transaction_id)
);

CREATE TABLE IF NOT EXISTS entity_versions (
    tenant_id TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    current_version BIGINT NOT NULL DEFAULT 1,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, entity_type, entity_id)
);

CREATE TABLE IF NOT EXISTS epics (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    title VARCHAR(255) NOT NULL,
    status VARCHAR(50) NOT NULL DEFAULT 'PENDING' CHECK (status IN ('PENDING', 'IN_PROGRESS', 'DONE', 'FAILED')),
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS estimates (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    quote_request_id TEXT REFERENCES quote_requests(id) ON DELETE SET NULL,
    customer_id TEXT NOT NULL,
    status TEXT DEFAULT 'DRAFT',
    total_amount_cents BIGINT DEFAULT 0,
    required_deposit_cents BIGINT DEFAULT 0,
    checkout_url TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS field_mutation_receipts (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    operation TEXT NOT NULL CHECK (operation IN ('appointment','routing_job','route')),
    idempotency_key TEXT NOT NULL CHECK (octet_length(idempotency_key) BETWEEN 1 AND 128),
    actor_id TEXT NOT NULL,
    request_sha256 TEXT NOT NULL CHECK (request_sha256 ~ '^[0-9a-f]{64}$'),
    response JSONB NOT NULL CHECK (jsonb_typeof(response) = 'object'),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, operation, idempotency_key)
);

CREATE TABLE IF NOT EXISTS fulfillment_schedules (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    subscription_plan_id TEXT NOT NULL,
    fulfillment_date DATE NOT NULL,
    subscriber_count INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'PENDING',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS harness_actor_bindings (
    actor_binding_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    binding_id TEXT NOT NULL,
    generation BIGINT NOT NULL,
    canonical_actor_id TEXT NOT NULL,
    native_actor_id TEXT NOT NULL,
    actor_type TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    captured_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, binding_id, generation, canonical_actor_id)
);

CREATE TABLE IF NOT EXISTS harness_actors (
    actor_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    actor_type TEXT NOT NULL,
    display_name TEXT,
    parent_actor_id TEXT,
    delegation_lineage JSONB NOT NULL DEFAULT '[]'::jsonb,
    authority_context JSONB NOT NULL DEFAULT '{}'::jsonb,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, actor_id)
);

CREATE TABLE IF NOT EXISTS harness_bindings (
    binding_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    task_id TEXT REFERENCES harness_tasks(task_id),
    harness_id TEXT NOT NULL,
    scope TEXT NOT NULL CHECK (scope IN ('session', 'task')),
    owner_id TEXT NOT NULL,
    workspace_mutation_scope_id TEXT NOT NULL,
    access_mode TEXT NOT NULL CHECK (access_mode IN ('read_only', 'read_write')),
    native_session_id TEXT,
    state TEXT NOT NULL DEFAULT 'creating',
    generation BIGINT NOT NULL DEFAULT 1 CHECK (generation > 0),
    capability_snapshot_id TEXT,
    adapter_config_digest TEXT,
    last_imported_native_cursor TEXT,
    last_exported_durable_sequence BIGINT,
    native_checkpoint_ref TEXT,
    worker_pool TEXT,
    state_locality TEXT,
    exact_resume_eligible BOOLEAN NOT NULL DEFAULT FALSE,
    invalidation_reason TEXT,
    native_record_digest TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_used_at TIMESTAMPTZ,
    extensions JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS harness_capability_snapshots (
    snapshot_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT,
    binding_id TEXT,
    harness_id TEXT NOT NULL,
    capability_version TEXT NOT NULL,
    capabilities JSONB NOT NULL DEFAULT '[]'::jsonb,
    snapshot_digest TEXT NOT NULL,
    captured_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, snapshot_id),
    UNIQUE (tenant_id, snapshot_digest)
);

CREATE TABLE IF NOT EXISTS harness_capsule_record_batches (
    batch_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    capsule_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    record_kind TEXT NOT NULL,
    record_schema TEXT NOT NULL,
    record_version INTEGER NOT NULL DEFAULT 1,
    payload JSONB NOT NULL,
    record_digest TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, capsule_id, ordinal)
);

CREATE TABLE IF NOT EXISTS harness_compactions (
    compaction_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    turn_id TEXT,
    from_durable_sequence BIGINT NOT NULL,
    to_durable_sequence BIGINT NOT NULL,
    summary TEXT NOT NULL,
    retained_message_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    checkpoint_id TEXT,
    provenance JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS harness_content_parts (
    content_part_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    part_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    artifact_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, message_id, ordinal)
);

CREATE TABLE IF NOT EXISTS harness_descriptors (
    harness_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    adapter_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    protocol_kind TEXT NOT NULL,
    protocol_version TEXT NOT NULL,
    adapter_version TEXT NOT NULL,
    source_revision TEXT,
    schema_revision TEXT,
    capabilities JSONB NOT NULL DEFAULT '[]'::jsonb,
    worker_image TEXT,
    worker_pool TEXT,
    state_locality TEXT,
    native_extension_namespace TEXT NOT NULL,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, harness_id)
);

CREATE TABLE IF NOT EXISTS harness_error_records (
    error_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    turn_id TEXT,
    attempt_id TEXT,
    component TEXT NOT NULL,
    error_code TEXT NOT NULL,
    error_class TEXT,
    message TEXT NOT NULL,
    retryable BOOLEAN NOT NULL DEFAULT FALSE,
    uncertain_effect BOOLEAN NOT NULL DEFAULT FALSE,
    details JSONB NOT NULL DEFAULT '{}'::jsonb,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    resolved_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS harness_goals (
    goal_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    parent_goal_id TEXT,
    state TEXT NOT NULL DEFAULT 'active',
    objective TEXT NOT NULL,
    budget JSONB NOT NULL DEFAULT '{}'::jsonb,
    usage_payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS harness_handoff_loss_reports (
    report_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    operation_id TEXT,
    capsule_id TEXT,
    source_harness_id TEXT NOT NULL,
    target_harness_id TEXT NOT NULL,
    report_digest TEXT NOT NULL,
    acknowledgement_required BOOLEAN NOT NULL DEFAULT FALSE,
    acknowledgement_digest TEXT,
    entries JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    acknowledged_at TIMESTAMPTZ,
    UNIQUE (tenant_id, report_digest)
);

CREATE TABLE IF NOT EXISTS harness_inference_final_responses (
    final_response_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    state TEXT NOT NULL,
    response_schema TEXT NOT NULL,
    response_version INTEGER NOT NULL DEFAULT 1,
    response_payload JSONB,
    usage_payload JSONB,
    error_payload JSONB,
    uncertain BOOLEAN NOT NULL DEFAULT FALSE,
    completed_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, request_id)
);

CREATE TABLE IF NOT EXISTS harness_inference_requests (
    request_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT,
    task_id TEXT,
    turn_id TEXT,
    attempt_id TEXT,
    model_binding_id TEXT,
    runtime_id TEXT,
    worker_id TEXT,
    idempotency_key TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'requested',
    request_schema TEXT NOT NULL,
    request_version INTEGER NOT NULL DEFAULT 1,
    request_payload JSONB NOT NULL,
    admitted_at TIMESTAMPTZ,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    uncertain_reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, idempotency_key)
);

CREATE TABLE IF NOT EXISTS harness_inference_stream_records (
    stream_record_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    delivery_sequence BIGINT NOT NULL,
    record_type TEXT NOT NULL,
    payload_schema TEXT NOT NULL,
    payload_version INTEGER NOT NULL DEFAULT 1,
    payload JSONB NOT NULL,
    usage_payload JSONB,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, request_id, delivery_sequence)
);

CREATE TABLE IF NOT EXISTS harness_interaction_responses (
    response_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    interaction_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    response_kind TEXT NOT NULL,
    payload JSONB NOT NULL,
    actor_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, interaction_id, ordinal)
);

CREATE TABLE IF NOT EXISTS harness_interactions (
    interaction_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    turn_id TEXT,
    attempt_id TEXT,
    interaction_kind TEXT NOT NULL,
    risk TEXT NOT NULL DEFAULT 'low',
    state TEXT NOT NULL DEFAULT 'requested',
    policy_decision JSONB,
    request_payload JSONB NOT NULL,
    response_payload JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    resolved_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS harness_messages (
    message_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    turn_id TEXT,
    role TEXT NOT NULL,
    actor_id TEXT,
    origin TEXT,
    phase TEXT,
    parent_message_id TEXT,
    correlation_id TEXT,
    status TEXT NOT NULL DEFAULT 'settled',
    visible_to_user BOOLEAN NOT NULL DEFAULT TRUE,
    redaction_state TEXT,
    native_provenance JSONB NOT NULL DEFAULT '{}'::jsonb,
    extensions JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    settled_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS harness_model_capacity_leases (
    capacity_lease_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    runtime_id TEXT NOT NULL,
    worker_id TEXT,
    request_id TEXT,
    state TEXT NOT NULL DEFAULT 'active',
    generation BIGINT NOT NULL DEFAULT 1,
    reserved_requests INTEGER NOT NULL DEFAULT 1,
    reserved_tokens BIGINT,
    expires_at TIMESTAMPTZ,
    released_at TIMESTAMPTZ,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    UNIQUE (tenant_id, capacity_lease_id)
);

CREATE TABLE IF NOT EXISTS harness_model_descriptors (
    model_descriptor_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    family TEXT,
    provider TEXT NOT NULL,
    revision TEXT,
    weight_digest TEXT,
    tokenizer_digest TEXT,
    context_window BIGINT,
    max_output_tokens BIGINT,
    modalities JSONB NOT NULL DEFAULT '[]'::jsonb,
    capabilities JSONB NOT NULL DEFAULT '[]'::jsonb,
    quantization TEXT,
    license TEXT,
    provenance TEXT,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, model_descriptor_id)
);

CREATE TABLE IF NOT EXISTS harness_model_runtime_capabilities (
    capability_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    runtime_id TEXT NOT NULL,
    capability TEXT NOT NULL,
    capability_version TEXT,
    state TEXT NOT NULL DEFAULT 'advertised',
    constraints JSONB NOT NULL DEFAULT '{}'::jsonb,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    captured_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, runtime_id, capability)
);

CREATE TABLE IF NOT EXISTS harness_model_runtimes (
    runtime_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    model_descriptor JSONB NOT NULL,
    runtime_kind TEXT NOT NULL,
    serving_engine TEXT,
    api_dialect TEXT,
    api_version TEXT,
    endpoint TEXT,
    capability_discovery_endpoint TEXT,
    credential_ref TEXT,
    image TEXT,
    supported_revisions JSONB NOT NULL DEFAULT '[]'::jsonb,
    adapter_refs JSONB NOT NULL DEFAULT '[]'::jsonb,
    resource_profile JSONB NOT NULL DEFAULT '{}'::jsonb,
    placement JSONB NOT NULL DEFAULT '{}'::jsonb,
    capacity JSONB NOT NULL DEFAULT '{}'::jsonb,
    autoscaling JSONB NOT NULL DEFAULT '{}'::jsonb,
    readiness TEXT NOT NULL DEFAULT 'unknown',
    health_endpoint TEXT,
    draining BOOLEAN NOT NULL DEFAULT FALSE,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS harness_native_record_sets (
    record_set_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    attempt_id TEXT,
    binding_id TEXT,
    harness_id TEXT NOT NULL,
    native_namespace TEXT NOT NULL,
    record_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    records JSONB NOT NULL DEFAULT '[]'::jsonb,
    record_digest TEXT NOT NULL,
    captured_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, record_digest)
);

CREATE TABLE IF NOT EXISTS harness_operation_fences (
    fence_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    worker_id TEXT NOT NULL,
    harness_id TEXT NOT NULL,
    generation BIGINT NOT NULL,
    fencing_token TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'active',
    issued_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TIMESTAMPTZ,
    released_at TIMESTAMPTZ,
    UNIQUE (tenant_id, operation_id, generation),
    UNIQUE (tenant_id, fencing_token)
);

CREATE TABLE IF NOT EXISTS harness_plan_steps (
    step_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    description TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    dependency_step_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    UNIQUE (tenant_id, plan_id, ordinal)
);

CREATE TABLE IF NOT EXISTS harness_plans (
    plan_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    attempt_id TEXT,
    state TEXT NOT NULL DEFAULT 'draft',
    summary TEXT,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS harness_portable_context_checkpoints (
    checkpoint_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    turn_id TEXT,
    durable_sequence BIGINT NOT NULL,
    summary TEXT NOT NULL,
    selected_message_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    source_durable_ranges JSONB NOT NULL DEFAULT '[]'::jsonb,
    retained_ancestor_event_id TEXT,
    compaction_provenance TEXT,
    usage_payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    integrity_digest TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, integrity_digest)
);

CREATE TABLE IF NOT EXISTS harness_process_chunks (
    chunk_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    process_id TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    stream TEXT NOT NULL,
    content BYTEA NOT NULL,
    content_encoding TEXT NOT NULL DEFAULT 'raw',
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, process_id, sequence)
);

CREATE TABLE IF NOT EXISTS harness_processes (
    process_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    attempt_id TEXT,
    state TEXT NOT NULL DEFAULT 'created',
    command JSONB NOT NULL DEFAULT '[]'::jsonb,
    working_directory TEXT,
    environment JSONB NOT NULL DEFAULT '{}'::jsonb,
    process_group TEXT,
    native_process_ref TEXT,
    exit_code INTEGER,
    signal_name TEXT,
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS harness_resume_checkpoints (
    checkpoint_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    turn_id TEXT,
    attempt_id TEXT,
    binding_id TEXT,
    harness_id TEXT NOT NULL,
    native_session_id TEXT,
    native_cursor TEXT,
    durable_sequence BIGINT NOT NULL,
    exact_resume_eligible BOOLEAN NOT NULL DEFAULT FALSE,
    portable_context_checkpoint_id TEXT,
    state_payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    checkpoint_digest TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, checkpoint_digest)
);

CREATE TABLE IF NOT EXISTS harness_runtime_configs (
    snapshot_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    snapshot_digest TEXT NOT NULL UNIQUE,
    config JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS harness_todos (
    todo_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    plan_id TEXT,
    ordinal INTEGER NOT NULL,
    content TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    owner_actor_id TEXT,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, session_id, ordinal)
);

CREATE TABLE IF NOT EXISTS harness_tool_calls (
    tool_call_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    turn_id TEXT,
    attempt_id TEXT NOT NULL,
    definition_snapshot_id TEXT NOT NULL,
    raw_input JSONB NOT NULL,
    parsed_input JSONB,
    state TEXT NOT NULL DEFAULT 'pending',
    started_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    finished_at TIMESTAMPTZ,
    retry_of TEXT,
    native_provenance JSONB NOT NULL DEFAULT '{}'::jsonb,
    observed_effect_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS harness_tool_definition_snapshots (
    snapshot_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    description TEXT,
    source TEXT NOT NULL,
    input_schema JSONB NOT NULL,
    output_schema JSONB,
    annotations JSONB NOT NULL DEFAULT '{}'::jsonb,
    version TEXT NOT NULL,
    digest TEXT NOT NULL,
    captured_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, digest)
);

CREATE TABLE IF NOT EXISTS harness_tool_progress (
    progress_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    sequence BIGINT NOT NULL,
    chunk_type TEXT NOT NULL,
    content JSONB NOT NULL DEFAULT '[]'::jsonb,
    progress_percent SMALLINT,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, tool_call_id, sequence)
);

CREATE TABLE IF NOT EXISTS harness_tool_results (
    result_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    state TEXT NOT NULL,
    content JSONB NOT NULL DEFAULT '[]'::jsonb,
    structured_output JSONB,
    error_payload JSONB,
    observed_effect_ids JSONB NOT NULL DEFAULT '[]'::jsonb,
    completed_at TIMESTAMPTZ,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    UNIQUE (tenant_id, tool_call_id)
);

CREATE TABLE IF NOT EXISTS harness_usage_records (
    usage_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    turn_id TEXT,
    attempt_id TEXT,
    inference_request_id TEXT,
    category TEXT NOT NULL,
    provider TEXT,
    model_id TEXT,
    input_tokens BIGINT,
    output_tokens BIGINT,
    cached_tokens BIGINT,
    reasoning_tokens BIGINT,
    latency_ms BIGINT,
    cost_micros BIGINT,
    usage_payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS harness_workspace_descriptors (
    workspace_descriptor_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    workspace_id TEXT,
    root_path TEXT NOT NULL,
    source TEXT NOT NULL,
    mutation_policy JSONB NOT NULL DEFAULT '{}'::jsonb,
    isolation_mode TEXT,
    repository JSONB NOT NULL DEFAULT '{}'::jsonb,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS help_articles (
    id BIGSERIAL PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    category TEXT NOT NULL,
    title TEXT NOT NULL,
    desc_text TEXT NOT NULL,
    link TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS inquiries (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT,
    source TEXT NOT NULL,
    raw_message TEXT NOT NULL,
    parsed_intent TEXT,
    urgency TEXT DEFAULT 'normal',
    status TEXT NOT NULL DEFAULT 'NEW' CHECK (status IN ('NEW', 'PROCESSING', 'QUOTED', 'CLOSED')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS interaction_events (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    channel TEXT NOT NULL,
    raw_content TEXT NOT NULL,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS interactive_proposals (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT,
    status TEXT NOT NULL CHECK (status IN ('Draft', 'Sent', 'Viewed', 'Accepted', 'Paid')),
    total_amount_cents BIGINT NOT NULL DEFAULT 0,
    required_deposit_cents BIGINT NOT NULL DEFAULT 0,
    checkout_url TEXT,
    message TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS inventory_predictions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    product_id TEXT NOT NULL REFERENCES products(id) ON DELETE CASCADE,
    predicted_stockout_date TIMESTAMPTZ,
    confidence_score DOUBLE PRECISION,
    suggested_reorder_quantity INTEGER,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS job_templates (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS lead_gen_campaigns (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    budget NUMERIC(10, 2) NOT NULL CHECK (budget >= 0),
    radius_miles INTEGER NOT NULL CHECK (radius_miles > 0),
    zip_code TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS ledger_accounts (
    tenant_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    currency TEXT NOT NULL,
    balance DOUBLE PRECISION DEFAULT 0.0,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, account_id)
);

CREATE TABLE IF NOT EXISTS ledger_reserves (
    tenant_id TEXT NOT NULL,
    envelope_id TEXT NOT NULL,
    envelope_type TEXT NOT NULL,
    balance DOUBLE PRECISION DEFAULT 0.0,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, envelope_id)
);

CREATE TABLE IF NOT EXISTS ledger_transactions (
    tenant_id TEXT NOT NULL,
    tx_id TEXT NOT NULL,
    amount DOUBLE PRECISION NOT NULL,
    currency TEXT NOT NULL,
    timestamp TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, tx_id)
);

CREATE TABLE IF NOT EXISTS locations (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS loyalty_programs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    program_type TEXT NOT NULL,
    config JSONB DEFAULT '{}',
    is_active BOOLEAN DEFAULT true,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS migration_jobs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    url TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'PENDING',
    extracted_products JSONB,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS newsletter_drafts (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id TEXT NOT NULL,
    subject TEXT NOT NULL,
    body_html TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'draft',
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    sent_at TIMESTAMP WITH TIME ZONE
);

CREATE TABLE IF NOT EXISTS ohc_fx_rates (
    id TEXT PRIMARY KEY,
    from_currency TEXT NOT NULL,
    to_currency TEXT NOT NULL,
    rate DOUBLE PRECISION NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(from_currency, to_currency)
);

CREATE TABLE IF NOT EXISTS ohc_i18n_strings (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    locale TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(tenant_id, locale, key)
);

CREATE TABLE IF NOT EXISTS ohc_multi_currency_ledger (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    presentment_amount BIGINT NOT NULL,
    presentment_currency TEXT NOT NULL,
    settlement_amount BIGINT NOT NULL,
    settlement_currency TEXT NOT NULL,
    exchange_rate DOUBLE PRECISION NOT NULL,
    is_offline_sync BOOLEAN NOT NULL DEFAULT FALSE,
    safe_margin_absorbed BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS ohc_translation_preferences (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    target_languages JSONB NOT NULL DEFAULT '[]'::jsonb,
    auto_translate BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(tenant_id)
);

CREATE TABLE IF NOT EXISTS operation_intents (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    action_type TEXT NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    request_identity JSONB,
    status TEXT NOT NULL DEFAULT 'PENDING',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    retry_count INT NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS pages (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    title TEXT NOT NULL,
    content TEXT,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS payment_intents (
    tenant_id TEXT NOT NULL,
    payment_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    amount DOUBLE PRECISION NOT NULL,
    currency TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    source TEXT NOT NULL,
    stripe_payment_intent_id TEXT,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, payment_id)
);

CREATE TABLE IF NOT EXISTS pricing_heuristics (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    service_category TEXT NOT NULL,
    base_rate_cents BIGINT NOT NULL,
    materials_markup_percentage NUMERIC NOT NULL,
    instructions TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS pricing_rules (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    base_price_cents BIGINT NOT NULL,
    rules_json JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS project_milestones (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    amount_cents BIGINT NOT NULL,
    status TEXT NOT NULL DEFAULT 'draft',
    payment_link TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS proposed_bookings (
    id TEXT PRIMARY KEY DEFAULT '',
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    requested_service TEXT NOT NULL,
    proposed_time TEXT NOT NULL,
    estimated_value DOUBLE PRECISION NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS raw_materials (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    current_quantity INT DEFAULT 0,
    reorder_threshold INT DEFAULT 0,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS recovery_attempts (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT,
    source_event_id TEXT NOT NULL,
    assistant_message_id TEXT,
    status TEXT NOT NULL,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS referral_codes (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    referral_code TEXT UNIQUE NOT NULL,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS reputation_profiles (
    id TEXT PRIMARY KEY,
    tenant_id TEXT UNIQUE NOT NULL,
    average_rating DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    total_reviews INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS reviews (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    order_id TEXT,
    rating INTEGER NOT NULL CHECK (rating >= 1 AND rating <= 5),
    comment TEXT,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS reward_claims (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    discount_code TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS route_plans (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL,
    delivery_date DATE NOT NULL,
    waypoint_sequence JSONB NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS saga_executions (
    id TEXT PRIMARY KEY DEFAULT '',
    tenant_id TEXT NOT NULL,
    saga_type TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'running',
    context JSONB DEFAULT '{}',
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT valid_status CHECK (status IN ('running', 'completed', 'compensating', 'failed'))
);

CREATE TABLE IF NOT EXISTS seo_discovery_reports (
    id TEXT PRIMARY KEY DEFAULT '',
    tenant_id TEXT NOT NULL,
    month VARCHAR(255) NOT NULL,
    plain_language_summary TEXT NOT NULL,
    metrics JSONB DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS service_items (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    base_price_cents BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS service_leads (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    customer_id TEXT REFERENCES customers(id) ON DELETE SET NULL,
    description TEXT,
    images JSONB,
    source TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'new' CHECK (status IN ('new', 'estimating', 'estimated', 'booked', 'closed')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS service_routes (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    agent_id TEXT,
    route_date DATE NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS shift_summaries (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    shift_date DATE NOT NULL,
    summary_text TEXT NOT NULL,
    escalations TEXT,
    metrics JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS social_post_proposals (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    product_id TEXT NOT NULL,
    content TEXT NOT NULL,
    image_url TEXT,
    seo_alt_text TEXT,
    seo_meta_description TEXT,
    status TEXT NOT NULL,
    created_at_unix BIGINT NOT NULL,
    updated_at_unix BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS staff_profiles (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    skills JSONB DEFAULT '[]',
    work_hours JSONB DEFAULT '{}',
    current_location_lat DOUBLE PRECISION,
    current_location_lng DOUBLE PRECISION,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS staff_tasks (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    staff_id TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    description TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    priority TEXT NOT NULL DEFAULT 'medium',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS swarm_tasks (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id TEXT NOT NULL DEFAULT 'default_tenant',
    mission_id TEXT NOT NULL,
    parent_plan_id TEXT,
    dependencies JSONB NOT NULL DEFAULT '[]',
    title TEXT NOT NULL,
    description TEXT,
    priority TEXT,
    status TEXT NOT NULL DEFAULT 'PENDING',
    assigned_agent_id TEXT,
    locked_until TIMESTAMPTZ,
    payload JSONB,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    auto_dreamed BOOLEAN DEFAULT FALSE,
    _sync_status TEXT DEFAULT 'pending',
    version INTEGER DEFAULT 1
);

CREATE TABLE IF NOT EXISTS sync_conflict_queue (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    base_version BIGINT NOT NULL,
    current_version BIGINT NOT NULL,
    payload JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'PENDING',
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS sync_events (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    batch_id TEXT,
    action_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    request_identity JSONB,
    receipt_status TEXT,
    receipt_route TEXT,
    client_event_id TEXT,
    entity_type TEXT,
    entity_id TEXT,
    base_version BIGINT,
    result_version BIGINT,
    synced_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS sync_events_entity_receipts
    ON sync_events (tenant_id, entity_type, entity_id, result_version)
    WHERE receipt_status = 'acknowledged';

CREATE TABLE IF NOT EXISTS task_envelopes (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    current_department TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'PENDING',
    payload JSONB DEFAULT '{}',
    routing_history JSONB DEFAULT '[]',
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS team_invites (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    team_id TEXT NOT NULL,
    inviter_id TEXT NOT NULL,
    invitee_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'PENDING',
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS telemetry_buffer (
    id SERIAL PRIMARY KEY,
    metric_name TEXT NOT NULL,
    metric_type TEXT NOT NULL,
    value REAL NOT NULL,
    labels_json TEXT NOT NULL,
    timestamp TIMESTAMPTZ NOT NULL,
    sync_status TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS terminal_cash_receipt_items (
    tenant_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    product_id TEXT NOT NULL REFERENCES products(id),
    quantity INTEGER NOT NULL CHECK (quantity > 0),
    amount_cents BIGINT NOT NULL CHECK (amount_cents >= 0),
    lock_id TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (tenant_id, operation_id, product_id),
    FOREIGN KEY (tenant_id, operation_id) REFERENCES terminal_cash_receipts(tenant_id, operation_id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS terminal_cash_reservation_once
    ON terminal_cash_receipt_items(tenant_id, product_id, lock_id) WHERE lock_id <> '';

CREATE TABLE IF NOT EXISTS tooltips (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    id TEXT NOT NULL,
    text TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, id)
);

CREATE TABLE IF NOT EXISTS vendors (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    contact_info TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS video_tutorials (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    id INTEGER NOT NULL,
    title TEXT NOT NULL,
    duration TEXT NOT NULL,
    video_url TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, id)
);

CREATE TABLE IF NOT EXISTS walkthrough_steps (
    id BIGSERIAL PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    page TEXT NOT NULL,
    step_order INTEGER NOT NULL,
    selector TEXT NOT NULL,
    title TEXT NOT NULL,
    text TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS work_intents (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    intent_type TEXT NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    status TEXT NOT NULL DEFAULT 'PENDING',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS work_tasks (
    id TEXT PRIMARY KEY DEFAULT '',
    tenant_id TEXT NOT NULL,
    booking_id TEXT NOT NULL,
    description TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open',
    scheduled_time TIMESTAMP WITH TIME ZONE NOT NULL,
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS agent_definition_installations (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    definition_id TEXT NOT NULL,
    version BIGINT NOT NULL,
    document TEXT NOT NULL,
    UNIQUE(tenant_id,user_id,definition_id,version),
    FOREIGN KEY(user_id,tenant_id) REFERENCES users(id,tenant_id) ON DELETE CASCADE,
    FOREIGN KEY(definition_id,version) REFERENCES agent_definitions(id,version)
);

CREATE TABLE IF NOT EXISTS agent_definition_publishers (
    definition_id TEXT NOT NULL,
    version BIGINT NOT NULL,
    tenant_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    PRIMARY KEY(definition_id,version),
    FOREIGN KEY(user_id,tenant_id) REFERENCES users(id,tenant_id) ON DELETE CASCADE,
    FOREIGN KEY(definition_id,version) REFERENCES agent_definitions(id,version)
);

-- Explicit first-party definitions. These are inert templates, never runtime agents.
INSERT INTO agent_definitions(id,version,digest,document,search_text,source) VALUES('00000000-0000-4000-8000-000000000001',1,'7740d03402f2f06f3a7ad5c6fabcb524d89783be735cd65149c60ee6ec29b36f','{"id":"00000000-0000-4000-8000-000000000001","version":1,"digest":"7740d03402f2f06f3a7ad5c6fabcb524d89783be735cd65149c60ee6ec29b36f","name":"Senior Rust Developer","description":"Prepare Rust changes with tests and explain their verification.","role":"Rust Developer","system_prompt":"Help prepare Rust code changes from the reviewed task and available source. Preserve authorization boundaries, add meaningful tests, and report unverified behavior. This saved definition grants no tools or execution authority.","visibility":"public","source":"first_party"}','senior rust developer prepare rust changes with tests and explain their verification. rust developer','first_party') ON CONFLICT(id,version) DO NOTHING;
INSERT INTO agent_definitions(id,version,digest,document,search_text,source) VALUES('00000000-0000-4000-8000-000000000002',1,'a398aa94e0f030ce0d9e6b52ce2fef76363a71cad217a55678ea2775af8be03a','{"id":"00000000-0000-4000-8000-000000000002","version":1,"digest":"a398aa94e0f030ce0d9e6b52ce2fef76363a71cad217a55678ea2775af8be03a","name":"Technical Writer","description":"Prepare clear technical documents from verified source material.","role":"Technical Writer","system_prompt":"Help draft clear technical documentation from the reviewed request and authorized source material. Distinguish evidence from assumptions and preserve private information. This saved definition grants no tools or execution authority.","visibility":"public","source":"first_party"}','technical writer prepare clear technical documents from verified source material. technical writer','first_party') ON CONFLICT(id,version) DO NOTHING;

CREATE OR REPLACE FUNCTION immutable_agent_definition_record() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN RAISE EXCEPTION 'agent_definition_record_is_immutable'; END $$;
CREATE OR REPLACE FUNCTION immutable_private_agent_definition_record() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' AND NOT EXISTS(SELECT 1 FROM users WHERE id=OLD.user_id AND tenant_id=OLD.tenant_id) THEN RETURN OLD; END IF;
 RAISE EXCEPTION 'agent_definition_record_is_immutable';
END $$;
DROP TRIGGER IF EXISTS immutable_agent_definitions ON agent_definitions;
CREATE TRIGGER immutable_agent_definitions BEFORE UPDATE OR DELETE ON agent_definitions FOR EACH ROW EXECUTE FUNCTION immutable_agent_definition_record();
DROP TRIGGER IF EXISTS immutable_agent_installations ON agent_definition_installations;
CREATE TRIGGER immutable_agent_installations BEFORE UPDATE OR DELETE ON agent_definition_installations FOR EACH ROW EXECUTE FUNCTION immutable_private_agent_definition_record();
DROP TRIGGER IF EXISTS immutable_agent_operations ON agent_definition_operations;
CREATE TRIGGER immutable_agent_operations BEFORE UPDATE OR DELETE ON agent_definition_operations FOR EACH ROW EXECUTE FUNCTION immutable_private_agent_definition_record();
DROP TRIGGER IF EXISTS immutable_agent_publishers ON agent_definition_publishers;
CREATE TRIGGER immutable_agent_publishers BEFORE UPDATE OR DELETE ON agent_definition_publishers FOR EACH ROW EXECUTE FUNCTION immutable_private_agent_definition_record();
DO $$ BEGIN EXECUTE format('ALTER FUNCTION %I.immutable_private_agent_definition_record() SET search_path TO pg_catalog,%I,pg_temp',current_schema(),current_schema()); END $$;

CREATE TABLE IF NOT EXISTS appointments (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    customer_id TEXT REFERENCES customers(id) ON DELETE CASCADE,
    job_template_id TEXT REFERENCES job_templates(id) ON DELETE SET NULL,
    status TEXT NOT NULL DEFAULT 'Requested',
    scheduled_start_time TIMESTAMPTZ,
    scheduled_end_time TIMESTAMPTZ,
    location_address TEXT,
    location_lat DOUBLE PRECISION,
    location_lng DOUBLE PRECISION,
    notes TEXT,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

DROP TRIGGER IF EXISTS sync_entity_updated_at ON appointments;
CREATE TRIGGER sync_entity_updated_at BEFORE UPDATE ON appointments
FOR EACH ROW EXECUTE FUNCTION touch_sync_entity_updated_at();

CREATE TABLE IF NOT EXISTS bom_items (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    finished_good_id TEXT REFERENCES products(id) ON DELETE CASCADE,
    raw_material_id TEXT REFERENCES raw_materials(id) ON DELETE CASCADE,
    quantity_required INT DEFAULT 1,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS builder_publication_work (
    publication_id UUID PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    site_id UUID NOT NULL,
    queued BOOLEAN NOT NULL DEFAULT TRUE,
    FOREIGN KEY (publication_id, tenant_id, site_id) REFERENCES builder_publications(publication_id, tenant_id, site_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_builder_publication_work_site
    ON builder_publication_work (site_id);
CREATE INDEX IF NOT EXISTS idx_builder_publication_work_queued
    ON builder_publication_work (publication_id)
    WHERE queued;

CREATE OR REPLACE FUNCTION protect_builder_publication_routing()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF ROW(NEW.publication_id, NEW.tenant_id, NEW.site_id) IS DISTINCT FROM ROW(OLD.publication_id, OLD.tenant_id, OLD.site_id) THEN
        RAISE EXCEPTION 'Publication routing identity is immutable';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS protect_builder_publication_routing ON builder_publication_work;
CREATE TRIGGER protect_builder_publication_routing
    BEFORE UPDATE ON builder_publication_work
    FOR EACH ROW
    EXECUTE FUNCTION protect_builder_publication_routing();

CREATE TABLE IF NOT EXISTS campaign_assets (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    campaign_id TEXT REFERENCES campaigns(id) ON DELETE CASCADE,
    type TEXT NOT NULL,
    content_url TEXT NOT NULL,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS channel_executions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    campaign_id TEXT REFERENCES campaigns(id) ON DELETE CASCADE,
    channel TEXT NOT NULL,
    metrics_sent INTEGER DEFAULT 0,
    metrics_clicks INTEGER DEFAULT 0,
    metrics_conversions INTEGER DEFAULT 0,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS chat_channels (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    inbox_id TEXT NOT NULL REFERENCES chat_inboxes(id) ON DELETE CASCADE,
    channel_type VARCHAR(255) NOT NULL,
    config JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS chat_conversations (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    inbox_id TEXT NOT NULL REFERENCES chat_inboxes(id) ON DELETE CASCADE,
    contact_id TEXT NOT NULL REFERENCES chat_contacts(id) ON DELETE CASCADE,
    assignee_id TEXT,
    status VARCHAR(50) NOT NULL DEFAULT 'open',
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS conflict_queue (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    sync_event_id TEXT NOT NULL REFERENCES sync_events(id) ON DELETE CASCADE,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    action_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    base_version BIGINT NOT NULL,
    current_version BIGINT NOT NULL,
    status TEXT NOT NULL DEFAULT 'UNRESOLVED',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS customer_loyalty_accounts (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    program_id TEXT NOT NULL REFERENCES loyalty_programs(id) ON DELETE CASCADE,
    customer_id TEXT NOT NULL,
    points_balance INTEGER DEFAULT 0,
    punches INTEGER DEFAULT 0,
    tier_name TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(tenant_id, program_id, customer_id)
);

CREATE TABLE IF NOT EXISTS depletion_logs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    raw_material_id TEXT REFERENCES raw_materials(id) ON DELETE CASCADE,
    sales_event_id TEXT,
    quantity_deducted INT DEFAULT 0,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS deposit_requirements (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    estimate_id TEXT NOT NULL REFERENCES estimates(id) ON DELETE CASCADE,
    amount_cents BIGINT NOT NULL,
    percentage DECIMAL(5,2),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'paid', 'refunded', 'voided')),
    payment_intent_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS escalations (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    location_id TEXT NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
    task_id TEXT,
    summary TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'PENDING' CHECK (status IN ('PENDING', 'APPROVED', 'REJECTED', 'RESOLVED')),
    created_by TEXT NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS harness_handoff_operations (
    operation_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    task_id TEXT REFERENCES harness_tasks(task_id),
    scope TEXT NOT NULL CHECK (scope IN ('session', 'task')),
    source_binding_id TEXT NOT NULL REFERENCES harness_bindings(binding_id),
    target_binding_id TEXT REFERENCES harness_bindings(binding_id),
    target_harness_id TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'requested',
    state_version BIGINT NOT NULL DEFAULT 0,
    capsule_id TEXT,
    loss_report_digest TEXT,
    loss_ack_digest TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS harness_model_bindings (
    model_binding_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    attempt_id TEXT,
    runtime_id TEXT NOT NULL REFERENCES harness_model_runtimes(runtime_id),
    model_descriptor JSONB NOT NULL,
    realized_capabilities JSONB NOT NULL DEFAULT '[]'::jsonb,
    serving_revision TEXT,
    routing_reason TEXT,
    usage_accounting_source TEXT,
    status TEXT NOT NULL DEFAULT 'selected',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    released_at TIMESTAMPTZ,
    extensions JSONB NOT NULL DEFAULT '{}'::jsonb,
    resolved_model JSONB
);

CREATE TABLE IF NOT EXISTS harness_model_runtime_workers (
    worker_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    runtime_id TEXT NOT NULL REFERENCES harness_model_runtimes(runtime_id),
    capability_version BIGINT NOT NULL DEFAULT 0,
    capabilities JSONB NOT NULL DEFAULT '{}'::jsonb,
    capacity JSONB NOT NULL DEFAULT '{}'::jsonb,
    state TEXT NOT NULL DEFAULT 'ready',
    last_heartbeat_at TIMESTAMPTZ,
    extensions JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS interaction_event_jobs (
    job_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    interaction_event_id TEXT NOT NULL REFERENCES interaction_events(id) ON DELETE CASCADE,
    status TEXT NOT NULL DEFAULT 'pending',
    retry_count INTEGER DEFAULT 0,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS ledger_entries (
    tenant_id TEXT NOT NULL,
    entry_id TEXT NOT NULL,
    tx_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    direction TEXT NOT NULL,
    amount DOUBLE PRECISION NOT NULL,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, entry_id),
    FOREIGN KEY (tenant_id, tx_id) REFERENCES ledger_transactions(tenant_id, tx_id),
    FOREIGN KEY (tenant_id, account_id) REFERENCES ledger_accounts(tenant_id, account_id)
);

CREATE TABLE IF NOT EXISTS loyalty_rewards (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    program_id TEXT NOT NULL REFERENCES loyalty_programs(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT,
    cost_in_points INTEGER NOT NULL,
    reward_type TEXT NOT NULL,
    reward_value JSONB DEFAULT '{}',
    is_active BOOLEAN DEFAULT true,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS price_history (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    rule_id TEXT REFERENCES pricing_rules(id) ON DELETE SET NULL,
    old_price_cents BIGINT NOT NULL,
    new_price_cents BIGINT NOT NULL,
    reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS promotion_codes (
    code TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    campaign_id TEXT REFERENCES campaigns(id) ON DELETE CASCADE,
    discount_value DOUBLE PRECISION NOT NULL,
    discount_type TEXT NOT NULL,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS purchase_orders (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    vendor_id TEXT NOT NULL REFERENCES vendors(id),
    status TEXT NOT NULL DEFAULT 'DRAFT',
    total_cost DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS quote_drafts (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    inquiry_id TEXT NOT NULL REFERENCES inquiries(id) ON DELETE CASCADE,
    suggested_service TEXT NOT NULL,
    estimated_amount_cents BIGINT NOT NULL,
    suggested_time_slot TIMESTAMPTZ,
    status TEXT NOT NULL DEFAULT 'PENDING_APPROVAL' CHECK (status IN ('PENDING_APPROVAL', 'APPROVED', 'REJECTED')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS role_assignments (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    location_id TEXT NOT NULL REFERENCES locations(id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN ('Owner', 'Location Manager', 'Staff')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS saga_steps (
    id TEXT PRIMARY KEY DEFAULT '',
    saga_id TEXT NOT NULL REFERENCES saga_executions(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL,
    step_name TEXT NOT NULL,
    agent_type TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    retry_count INTEGER DEFAULT 0,
    created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP,
    CONSTRAINT valid_step_status CHECK (status IN ('pending', 'running', 'completed', 'failed', 'compensated'))
);

CREATE TABLE IF NOT EXISTS tasks (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    epic_id UUID REFERENCES epics(id),
    title VARCHAR(255) NOT NULL,
    status VARCHAR(50) NOT NULL CHECK (status IN ('PENDING', 'CLAIMED', 'DONE', 'FAILED')),
    assigned_agent VARCHAR(100),
    created_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS work_item (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL REFERENCES customer_profile(id),
    source TEXT NOT NULL,
    payload JSONB,
    status TEXT NOT NULL,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS agent_draft (
    id TEXT PRIMARY KEY,
    work_item_id TEXT NOT NULL REFERENCES work_item(id),
    response TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TIMESTAMPTZ DEFAULT NOW(),
    updated_at TIMESTAMPTZ DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS chat_messages (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL REFERENCES chat_conversations(id) ON DELETE CASCADE,
    sender_type VARCHAR(50) NOT NULL,
    sender_id TEXT,
    content TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS harness_attempts (
    attempt_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    task_id TEXT NOT NULL REFERENCES harness_tasks(task_id) ON DELETE CASCADE,
    turn_id TEXT REFERENCES harness_turns(turn_id),
    parent_attempt_id TEXT,
    kind TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    state_version BIGINT NOT NULL DEFAULT 0,
    harness_id TEXT NOT NULL,
    model_runtime_id TEXT REFERENCES harness_model_runtimes(runtime_id),
    runtime_config_snapshot_id TEXT REFERENCES harness_runtime_configs(snapshot_id),
    model_binding_id TEXT REFERENCES harness_model_bindings(model_binding_id),
    worker_id TEXT,
    started_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    finished_at TIMESTAMPTZ,
    metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
    extensions JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS harness_capsules (
    capsule_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    handoff_id TEXT REFERENCES harness_handoff_operations(operation_id),
    schema_version INTEGER NOT NULL,
    minimum_reader_version INTEGER NOT NULL,
    producer TEXT NOT NULL,
    compiler_version INTEGER NOT NULL,
    source_binding_id TEXT,
    target_harness_id TEXT NOT NULL,
    from_durable_sequence BIGINT NOT NULL,
    to_durable_sequence BIGINT NOT NULL,
    branch_id TEXT,
    head_event_id TEXT,
    manifest JSONB NOT NULL,
    manifest_digest TEXT NOT NULL,
    record_digest TEXT NOT NULL,
    loss_report_digest TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, manifest_digest)
);

CREATE TABLE IF NOT EXISTS job_locations (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    service_route_id TEXT NOT NULL REFERENCES service_routes(id) ON DELETE CASCADE,
    appointment_id TEXT NOT NULL REFERENCES appointments(id) ON DELETE CASCADE,
    sequence_order INTEGER NOT NULL,
    estimated_travel_time_mins INTEGER,
    distance_to_next_km DOUBLE PRECISION,
    status TEXT NOT NULL DEFAULT 'pending',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(service_route_id, sequence_order)
);

CREATE TABLE IF NOT EXISTS loyalty_transactions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    account_id TEXT NOT NULL REFERENCES customer_loyalty_accounts(id) ON DELETE CASCADE,
    transaction_type TEXT NOT NULL,
    amount INTEGER NOT NULL,
    reason TEXT,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS harness_capsule_loss_entries (
    capsule_id TEXT NOT NULL REFERENCES harness_capsules(capsule_id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL,
    source_path TEXT NOT NULL,
    reason TEXT NOT NULL,
    severity TEXT NOT NULL CHECK (severity IN ('info', 'warning', 'required', 'unsafe')),
    target_representation TEXT,
    acknowledgement_required BOOLEAN NOT NULL DEFAULT FALSE,
    capability TEXT,
    PRIMARY KEY (capsule_id, ordinal)
);

CREATE TABLE IF NOT EXISTS harness_events (
    event_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    task_id TEXT REFERENCES harness_tasks(task_id),
    turn_id TEXT REFERENCES harness_turns(turn_id),
    source_attempt_id TEXT REFERENCES harness_attempts(attempt_id),
    ingest_attempt_id TEXT REFERENCES harness_attempts(attempt_id),
    actor_id TEXT,
    worker_id TEXT,
    harness_id TEXT,
    binding_id TEXT REFERENCES harness_bindings(binding_id),
    binding_generation BIGINT,
    lease_id TEXT,
    lease_generation BIGINT,
    fencing_token TEXT,
    durable_sequence BIGINT,
    delivery_stream_id TEXT,
    delivery_sequence BIGINT,
    aggregate_id TEXT,
    aggregate_sequence BIGINT,
    branch_id TEXT,
    event_type TEXT NOT NULL,
    payload_schema TEXT NOT NULL,
    payload_version INTEGER NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL,
    ingested_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    correlation_id TEXT,
    causation_id TEXT,
    idempotency_key TEXT,
    durability TEXT NOT NULL CHECK (durability IN ('durable', 'transient')),
    replay_requirement TEXT NOT NULL CHECK (replay_requirement IN ('required', 'ignorable')),
    visibility TEXT,
    data_classification TEXT,
    native_provenance JSONB NOT NULL DEFAULT '{}'::jsonb,
    payload JSONB NOT NULL,
    extensions JSONB NOT NULL DEFAULT '{}'::jsonb,
    UNIQUE (session_id, event_id)
);

CREATE TABLE IF NOT EXISTS harness_inference_admissions (
    admission_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    task_id TEXT REFERENCES harness_tasks(task_id),
    turn_id TEXT REFERENCES harness_turns(turn_id),
    attempt_id TEXT REFERENCES harness_attempts(attempt_id),
    model_binding_id TEXT NOT NULL REFERENCES harness_model_bindings(model_binding_id),
    runtime_id TEXT NOT NULL REFERENCES harness_model_runtimes(runtime_id),
    worker_id TEXT NOT NULL REFERENCES harness_model_runtime_workers(worker_id),
    capacity_lease_id TEXT NOT NULL,
    capacity_generation BIGINT NOT NULL,
    fencing_token TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'queued',
    state_version BIGINT NOT NULL DEFAULT 0,
    final_response JSONB,
    usage JSONB,
    uncertain_reason TEXT,
    admitted_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    UNIQUE (tenant_id, request_id)
);

CREATE TABLE IF NOT EXISTS harness_leases (
    lease_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL REFERENCES harness_attempts(attempt_id) ON DELETE CASCADE,
    worker_id TEXT,
    generation BIGINT NOT NULL CHECK (generation > 0),
    fencing_token TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'active',
    issued_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TIMESTAMPTZ,
    released_at TIMESTAMPTZ,
    UNIQUE (attempt_id, generation),
    UNIQUE (lease_id, generation)
);

CREATE TABLE IF NOT EXISTS harness_native_records (
    native_record_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    attempt_id TEXT REFERENCES harness_attempts(attempt_id),
    harness_id TEXT NOT NULL,
    adapter_version TEXT NOT NULL,
    native_schema TEXT NOT NULL,
    record_kind TEXT NOT NULL,
    native_identity TEXT,
    native_cursor TEXT,
    ordinal BIGINT,
    payload_digest TEXT NOT NULL,
    object_storage_ref TEXT,
    captured_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    extensions JSONB NOT NULL DEFAULT '{}'::jsonb
);

CREATE TABLE IF NOT EXISTS harness_artifacts (
    artifact_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    media_type TEXT,
    byte_length BIGINT NOT NULL CHECK (byte_length >= 0),
    sha256 TEXT NOT NULL,
    authorized_storage_ref TEXT,
    producer_event_id TEXT REFERENCES harness_events(event_id),
    retention_class TEXT,
    provenance JSONB NOT NULL DEFAULT '{}'::jsonb,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, sha256)
);

CREATE TABLE IF NOT EXISTS harness_command_inbox (
    command_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    task_id TEXT REFERENCES harness_tasks(task_id),
    turn_id TEXT REFERENCES harness_turns(turn_id),
    attempt_id TEXT REFERENCES harness_attempts(attempt_id),
    lease_id TEXT,
    lease_generation BIGINT,
    fencing_token TEXT,
    idempotency_key TEXT NOT NULL,
    command_type TEXT NOT NULL,
    payload_schema TEXT NOT NULL,
    payload JSONB NOT NULL,
    status TEXT NOT NULL DEFAULT 'admitted',
    outcome_event_id TEXT REFERENCES harness_events(event_id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    settled_at TIMESTAMPTZ,
    UNIQUE (session_id, idempotency_key)
);

CREATE TABLE IF NOT EXISTS harness_event_branch_heads (
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    branch_id TEXT NOT NULL,
    head_event_id TEXT NOT NULL,
    expected_head_event_id TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (session_id, branch_id),
    FOREIGN KEY (session_id, head_event_id) REFERENCES harness_events(session_id, event_id)
);

CREATE TABLE IF NOT EXISTS harness_event_parents (
    session_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    parent_event_id TEXT NOT NULL,
    PRIMARY KEY (session_id, event_id, parent_event_id),
    CHECK (event_id <> parent_event_id),
    FOREIGN KEY (session_id, event_id) REFERENCES harness_events(session_id, event_id) ON DELETE CASCADE,
    FOREIGN KEY (session_id, parent_event_id) REFERENCES harness_events(session_id, event_id) ON DELETE RESTRICT
);

CREATE TABLE IF NOT EXISTS harness_command_outbox (
    outbox_id BIGSERIAL PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    task_id TEXT REFERENCES harness_tasks(task_id),
    turn_id TEXT REFERENCES harness_turns(turn_id),
    attempt_id TEXT REFERENCES harness_attempts(attempt_id),
    command_id TEXT REFERENCES harness_command_inbox(command_id),
    event_id TEXT REFERENCES harness_events(event_id),
    idempotency_key TEXT NOT NULL,
    payload JSONB NOT NULL,
    published_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (session_id, idempotency_key)
);

CREATE TABLE IF NOT EXISTS harness_workspace_snapshots (
    snapshot_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES harness_sessions(session_id) ON DELETE CASCADE,
    parent_snapshot_id TEXT,
    durable_sequence BIGINT,
    tree_digest TEXT NOT NULL,
    archive_artifact_id TEXT REFERENCES harness_artifacts(artifact_id),
    git_identity JSONB NOT NULL DEFAULT '{}'::jsonb,
    patch_manifest JSONB NOT NULL DEFAULT '{}'::jsonb,
    completeness TEXT NOT NULL DEFAULT 'complete',
    creator_attempt_id TEXT REFERENCES harness_attempts(attempt_id),
    reason TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, tree_digest)
);

CREATE INDEX IF NOT EXISTS harness_attempts_task_idx ON harness_attempts (tenant_id, task_id, state, started_at DESC);
CREATE UNIQUE INDEX IF NOT EXISTS harness_one_active_writable_binding ON harness_bindings (session_id, workspace_mutation_scope_id) WHERE state = 'active' AND access_mode = 'read_write';
CREATE UNIQUE INDEX IF NOT EXISTS harness_durable_event_sequence_idx ON harness_events (tenant_id, session_id, durable_sequence) WHERE durable_sequence IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS harness_delivery_sequence_idx ON harness_events (tenant_id, delivery_stream_id, delivery_sequence) WHERE delivery_stream_id IS NOT NULL AND delivery_sequence IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS harness_event_idempotency_idx ON harness_events (session_id, idempotency_key) WHERE idempotency_key IS NOT NULL;

ALTER TABLE agent_memories ADD COLUMN IF NOT EXISTS business_id TEXT;
ALTER TABLE autodream_memories ADD COLUMN IF NOT EXISTS created_at TIMESTAMPTZ DEFAULT CURRENT_TIMESTAMP;
ALTER TABLE invoice_line_items ADD COLUMN IF NOT EXISTS amount_cents BIGINT;
ALTER TABLE invoice_line_items ADD COLUMN IF NOT EXISTS unit_price_cents BIGINT;
ALTER TABLE invoices ADD COLUMN IF NOT EXISTS split_partner_id TEXT;
ALTER TABLE orders ADD COLUMN IF NOT EXISTS base_currency TEXT DEFAULT 'USD';
ALTER TABLE orders ADD COLUMN IF NOT EXISTS customer_notes TEXT;
ALTER TABLE orders ADD COLUMN IF NOT EXISTS exchange_rate DOUBLE PRECISION DEFAULT 1.0;
ALTER TABLE orders ADD COLUMN IF NOT EXISTS notes TEXT;
ALTER TABLE orders ADD COLUMN IF NOT EXISTS payment_source TEXT;
ALTER TABLE orders ADD COLUMN IF NOT EXISTS transaction_currency TEXT DEFAULT 'USD';
ALTER TABLE orders ADD COLUMN IF NOT EXISTS translated_notes TEXT;
ALTER TABLE payment_events ADD COLUMN IF NOT EXISTS base_currency TEXT DEFAULT 'USD';
ALTER TABLE payment_events ADD COLUMN IF NOT EXISTS exchange_rate DOUBLE PRECISION DEFAULT 1.0;
ALTER TABLE payment_events ADD COLUMN IF NOT EXISTS transaction_currency TEXT DEFAULT 'USD';
ALTER TABLE pos_offline_transactions ADD COLUMN IF NOT EXISTS _sync_status TEXT;
ALTER TABLE pos_offline_transactions ADD COLUMN IF NOT EXISTS request_identity JSONB;
ALTER TABLE pos_offline_transactions ADD COLUMN IF NOT EXISTS request_reconciliation JSONB;
ALTER TABLE pos_offline_transactions ADD COLUMN IF NOT EXISTS request_status TEXT;
ALTER TABLE pos_offline_transactions ADD COLUMN IF NOT EXISTS terminal_id TEXT;
ALTER TABLE pos_terminal_sessions ADD COLUMN IF NOT EXISTS pending_reconciliation JSONB NOT NULL DEFAULT '[]'::jsonb;
ALTER TABLE products ADD COLUMN IF NOT EXISTS pn_counter_p BIGINT;
ALTER TABLE products ADD COLUMN IF NOT EXISTS pn_counter_n BIGINT;
ALTER TABLE products ADD COLUMN IF NOT EXISTS seo_title TEXT;
ALTER TABLE products ADD COLUMN IF NOT EXISTS seo_description TEXT;
ALTER TABLE products ADD COLUMN IF NOT EXISTS seo_schema_json JSONB;
ALTER TABLE project_tasks ADD COLUMN IF NOT EXISTS milestone_id TEXT;
ALTER TABLE quote_line_items ADD COLUMN IF NOT EXISTS service_item_id UUID;
ALTER TABLE quotes ADD COLUMN IF NOT EXISTS calendar_event_id TEXT;
ALTER TABLE quotes ADD COLUMN IF NOT EXISTS checkout_url TEXT;
ALTER TABLE quotes ADD COLUMN IF NOT EXISTS deposit_payment_intent_id TEXT;
ALTER TABLE quotes ADD COLUMN IF NOT EXISTS linked_invoice_id TEXT;
ALTER TABLE quotes ADD COLUMN IF NOT EXISTS required_deposit BIGINT;
ALTER TABLE quotes ADD COLUMN IF NOT EXISTS total_amount BIGINT;
ALTER TABLE services ADD COLUMN IF NOT EXISTS deposit_required NUMERIC DEFAULT 0;
ALTER TABLE services ADD COLUMN IF NOT EXISTS seo_title TEXT;
ALTER TABLE services ADD COLUMN IF NOT EXISTS seo_description TEXT;
ALTER TABLE services ADD COLUMN IF NOT EXISTS seo_schema_json JSONB;
ALTER TABLE shared_tasks ADD COLUMN IF NOT EXISTS action_risk TEXT;
ALTER TABLE shared_tasks ADD COLUMN IF NOT EXISTS approval_status TEXT;
ALTER TABLE shared_tasks ADD COLUMN IF NOT EXISTS deliberation_log JSONB DEFAULT '[]'::jsonb;
ALTER TABLE shared_tasks ADD COLUMN IF NOT EXISTS depth INTEGER;
ALTER TABLE shared_tasks ADD COLUMN IF NOT EXISTS proposed_content TEXT;
ALTER TABLE shared_tasks ADD COLUMN IF NOT EXISTS ultraplan_phase TEXT;
ALTER TABLE shared_tasks_v4 ADD COLUMN IF NOT EXISTS deliberation_log JSONB DEFAULT '[]'::jsonb;
ALTER TABLE shared_tasks_v4 ADD COLUMN IF NOT EXISTS depth INTEGER;
ALTER TABLE shared_tasks_v4 ADD COLUMN IF NOT EXISTS organization_id TEXT;
ALTER TABLE shared_tasks_v4 ADD COLUMN IF NOT EXISTS ultraplan_phase TEXT;
ALTER TABLE subscribers ADD COLUMN IF NOT EXISTS last_activity_date TIMESTAMPTZ;
ALTER TABLE subscription_plans ADD COLUMN IF NOT EXISTS discount_percentage DOUBLE PRECISION;
ALTER TABLE subscription_plans ADD COLUMN IF NOT EXISTS interval TEXT;
ALTER TABLE subscription_plans ADD COLUMN IF NOT EXISTS interval_count INTEGER NOT NULL DEFAULT 1;
ALTER TABLE tenants ADD COLUMN IF NOT EXISTS language_preference TEXT DEFAULT 'en';
ALTER TABLE tenants ADD COLUMN IF NOT EXISTS owner_email TEXT;
ALTER TABLE tenants ADD COLUMN IF NOT EXISTS preferred_language TEXT DEFAULT 'en';

CREATE INDEX IF NOT EXISTS idx_quotes_id_text_tenant
    ON quotes ((id::text), tenant_id);
CREATE INDEX IF NOT EXISTS idx_customers_id_text_tenant
    ON customers ((id::text), tenant_id);
CREATE INDEX IF NOT EXISTS idx_quote_line_items_quote_id_text_tenant
    ON quote_line_items ((quote_id::text), tenant_id);
