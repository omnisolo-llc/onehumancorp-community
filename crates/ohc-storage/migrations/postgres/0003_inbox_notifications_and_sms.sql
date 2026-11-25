-- SPDX-License-Identifier: Apache-2.0
-- Unified Single-Tenant OSS Migration 0003 (PostgreSQL):
-- Unified customer inbox, triage, department dispatches, manual inbox requests,
-- actor-owned SMS verification proofs, and atomic new-order SMS admission.

CREATE TABLE IF NOT EXISTS omni_inbox_messages (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    source TEXT NOT NULL,
    original_content TEXT NOT NULL,
    translated_content TEXT NOT NULL,
    source_language TEXT,
    target_language TEXT NOT NULL,
    draft_reply TEXT,
    status TEXT NOT NULL DEFAULT 'unread',
    sender_id TEXT,
    customer_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_omni_inbox_messages_tenant_status
    ON omni_inbox_messages (tenant_id, status, created_at DESC);

CREATE TABLE IF NOT EXISTS inbox_messages (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    source TEXT,
    content TEXT,
    original_content TEXT,
    translated_from_language TEXT,
    draft_reply TEXT,
    status TEXT,
    sender_id TEXT,
    customer_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS unified_threads (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    customer_id TEXT,
    channel TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_unified_threads_tenant
    ON unified_threads (tenant_id);

CREATE TABLE IF NOT EXISTS unified_messages (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES unified_threads(id) ON DELETE CASCADE,
    sender_type TEXT NOT NULL,
    content TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_unified_messages_tenant
    ON unified_messages (tenant_id, thread_id);

CREATE TABLE IF NOT EXISTS unified_triage_actions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    thread_id TEXT NOT NULL REFERENCES unified_threads(id) ON DELETE CASCADE,
    action_type TEXT NOT NULL,
    action_payload TEXT,
    status TEXT NOT NULL DEFAULT 'pending',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS triage_items (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    customer_id TEXT,
    source TEXT,
    priority TEXT,
    context TEXT,
    status TEXT NOT NULL DEFAULT 'pending',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS triage_proposed_actions (
    id TEXT PRIMARY KEY,
    triage_item_id TEXT NOT NULL REFERENCES triage_items(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    action_type TEXT,
    payload TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS legacy_triage_decisions (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    action_id TEXT NOT NULL,
    approved BOOLEAN NOT NULL,
    edited_payload TEXT,
    receipt TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, action_id)
);

CREATE TABLE IF NOT EXISTS auto_reply_policies (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL UNIQUE REFERENCES tenants(id) ON DELETE CASCADE,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    delay_minutes INTEGER NOT NULL DEFAULT 5,
    tone_instructions TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS manual_inbox_intents (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    inbox_message_id TEXT NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0),
    PRIMARY KEY (tenant_id, inbox_message_id)
);

CREATE TABLE IF NOT EXISTS manual_inbox_requests (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    request_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    inbox_message_id TEXT NOT NULL,
    dispatch_id TEXT NOT NULL,
    source TEXT NOT NULL,
    recipient TEXT NOT NULL,
    body TEXT NOT NULL,
    provider_binding TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    intent TEXT NOT NULL CHECK (intent IN ('send', 'dismissed', 'resolved')),
    intent_revision BIGINT NOT NULL CHECK (intent_revision > 0),
    state TEXT NOT NULL CHECK (state IN ('pending', 'claimed', 'retired', 'dismissed', 'resolved')),
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, request_id),
    UNIQUE (tenant_id, dispatch_id)
);

CREATE UNIQUE INDEX IF NOT EXISTS manual_inbox_pending_owner
    ON manual_inbox_requests (tenant_id, inbox_message_id)
    WHERE state = 'pending';

CREATE OR REPLACE FUNCTION manual_inbox_request_immutable_fn()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.tenant_id IS DISTINCT FROM OLD.tenant_id
        OR NEW.request_id IS DISTINCT FROM OLD.request_id
        OR NEW.actor_id IS DISTINCT FROM OLD.actor_id
        OR NEW.inbox_message_id IS DISTINCT FROM OLD.inbox_message_id
        OR NEW.dispatch_id IS DISTINCT FROM OLD.dispatch_id
        OR NEW.source IS DISTINCT FROM OLD.source
        OR NEW.recipient IS DISTINCT FROM OLD.recipient
        OR NEW.body IS DISTINCT FROM OLD.body
        OR NEW.provider_binding IS DISTINCT FROM OLD.provider_binding
        OR NEW.payload_hash IS DISTINCT FROM OLD.payload_hash
        OR NEW.intent IS DISTINCT FROM OLD.intent
        OR NEW.intent_revision IS DISTINCT FROM OLD.intent_revision
        OR NEW.created_at IS DISTINCT FROM OLD.created_at
    THEN
        RAISE EXCEPTION 'manual request identity is immutable';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS manual_inbox_request_immutable ON manual_inbox_requests;
CREATE TRIGGER manual_inbox_request_immutable
    BEFORE UPDATE ON manual_inbox_requests
    FOR EACH ROW
    EXECUTE FUNCTION manual_inbox_request_immutable_fn();

CREATE TABLE IF NOT EXISTS department_message_dispatches (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    action_id TEXT NOT NULL,
    inbox_message_id TEXT NOT NULL,
    inbox_kind TEXT NOT NULL CHECK (inbox_kind IN ('inbox', 'omni')),
    payload_hash TEXT NOT NULL,
    provider_binding TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('unknown', 'accepted', 'rejected', 'blocked')),
    provider_message_id TEXT,
    detail TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, action_id),
    CHECK ((state = 'accepted' AND provider_message_id IS NOT NULL AND length(provider_message_id) > 0)
        OR (state <> 'accepted' AND provider_message_id IS NULL))
);

CREATE UNIQUE INDEX IF NOT EXISTS department_message_inbox_attempt_fence
    ON department_message_dispatches (tenant_id, inbox_message_id)
    WHERE state IN ('unknown', 'accepted');

CREATE TABLE IF NOT EXISTS sms_notification_preferences (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    actor_id TEXT NOT NULL,
    phone TEXT,
    verification_id TEXT,
    current_challenge_id TEXT,
    urgent_booking BOOLEAN NOT NULL DEFAULT FALSE,
    failed_payment BOOLEAN NOT NULL DEFAULT FALSE,
    new_order BOOLEAN NOT NULL DEFAULT FALSE,
    send_window BIGINT NOT NULL DEFAULT 0,
    send_count INTEGER NOT NULL DEFAULT 0,
    last_requested_at BIGINT NOT NULL DEFAULT 0,
    updated_at BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, actor_id),
    CHECK ((phone IS NULL AND verification_id IS NULL AND NOT urgent_booking AND NOT failed_payment AND NOT new_order)
        OR (phone IS NOT NULL AND verification_id IS NOT NULL)),
    CHECK (send_count >= 0)
);

CREATE TABLE IF NOT EXISTS sms_verification_challenges (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    actor_id TEXT NOT NULL,
    challenge_id TEXT NOT NULL,
    phone TEXT NOT NULL,
    code_mac TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('sending', 'accepted', 'unknown', 'rejected', 'verified', 'superseded')),
    provider_sid TEXT,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 5),
    PRIMARY KEY (tenant_id, actor_id, challenge_id),
    CHECK (expires_at > created_at),
    CHECK (state NOT IN ('accepted', 'verified') OR provider_sid IS NOT NULL),
    FOREIGN KEY (tenant_id, actor_id) REFERENCES sms_notification_preferences(tenant_id, actor_id)
);

CREATE TABLE IF NOT EXISTS sms_notification_events (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    event_id TEXT NOT NULL,
    event_type TEXT NOT NULL CHECK (event_type IN ('urgent_booking', 'failed_payment', 'new_order')),
    message TEXT NOT NULL,
    message_hash TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('prepared', 'no_recipients', 'provider_accepted', 'no_eligible_recipients')),
    next_attempt_at BIGINT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, event_id, event_type)
);

CREATE TABLE IF NOT EXISTS sms_notification_dispatches (
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    actor_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    event_type TEXT NOT NULL CHECK (event_type IN ('urgent_booking', 'failed_payment', 'new_order')),
    phone TEXT NOT NULL,
    verification_id TEXT NOT NULL,
    message_hash TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('prepared', 'sending', 'accepted', 'unknown', 'rejected', 'cancelled')),
    provider_sid TEXT,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, actor_id, event_id, event_type),
    CHECK (state != 'accepted' OR provider_sid IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS sms_order_outbox_pending
    ON sms_notification_events (event_type, status, next_attempt_at, created_at, tenant_id, event_id);

CREATE OR REPLACE FUNCTION admit_order_sms()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF NEW.tenant_id IS NULL OR length(NEW.tenant_id) NOT BETWEEN 1 AND 512
        OR btrim(NEW.tenant_id) <> NEW.tenant_id OR lower(NEW.tenant_id) = 'system'
        OR length(NEW.id) NOT BETWEEN 1 AND 255 OR btrim(NEW.id) <> NEW.id
        OR NEW.id ~ '[[:cntrl:]]'
    THEN
        RETURN NEW;
    END IF;
    IF EXISTS (
        SELECT 1 FROM sms_notification_events e
        WHERE e.tenant_id = NEW.tenant_id AND e.event_id = NEW.id AND e.event_type = 'new_order'
    ) THEN
        RETURN NEW;
    END IF;
    INSERT INTO sms_notification_events (
        tenant_id, event_id, event_type, message, message_hash, status, created_at
    )
    VALUES (
        NEW.tenant_id,
        NEW.id,
        'new_order',
        'A new order has been saved. Open OmniSolo to review it.',
        'ea077c658f64495269b8e6b40d71658395387f2470d40038bdac298f01abcebd',
        'prepared',
        extract(epoch FROM clock_timestamp())::bigint
    );
    INSERT INTO sms_notification_dispatches (
        tenant_id, actor_id, event_id, event_type, phone, verification_id, message_hash, state, created_at
    )
    SELECT DISTINCT
        p.tenant_id,
        p.actor_id,
        NEW.id,
        'new_order',
        p.phone,
        p.verification_id,
        'ea077c658f64495269b8e6b40d71658395387f2470d40038bdac298f01abcebd',
        'prepared',
        extract(epoch FROM transaction_timestamp())::bigint
    FROM sms_notification_preferences p
    JOIN users u ON u.id = p.actor_id AND u.tenant_id = p.tenant_id
    JOIN identity_user_roles r ON r.user_id = u.id AND r.tenant_id = u.tenant_id
    JOIN sms_verification_challenges c ON c.tenant_id = p.tenant_id AND c.actor_id = p.actor_id
        AND c.challenge_id = p.verification_id AND c.phone = p.phone
    WHERE p.tenant_id = NEW.tenant_id
      AND u.active = TRUE
      AND p.new_order = TRUE
      AND c.state = 'verified'
      AND lower(r.role_name) IN ('owner', 'admin');
    UPDATE sms_notification_events
    SET status = 'no_recipients'
    WHERE tenant_id = NEW.tenant_id
      AND event_id = NEW.id
      AND event_type = 'new_order'
      AND NOT EXISTS (
          SELECT 1 FROM sms_notification_dispatches d
          WHERE d.tenant_id = NEW.tenant_id AND d.event_id = NEW.id AND d.event_type = 'new_order'
      );
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS orders_admit_sms ON orders;
CREATE TRIGGER orders_admit_sms
    AFTER INSERT ON orders
    FOR EACH ROW
    EXECUTE FUNCTION admit_order_sms();
