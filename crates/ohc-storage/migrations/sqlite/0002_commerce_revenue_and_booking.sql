-- SPDX-License-Identifier: Apache-2.0
-- Unified Single-Tenant OSS Migration 0002 (SQLite):
-- Solo-owner commerce, CRM, proposals, quotes, invoices, bookings, catalog,
-- orders, customer product subscriptions, fulfillment, storefront sites,
-- referrals, POS/offline terminal, projects, and staff operations.

CREATE TABLE IF NOT EXISTS customers (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    email TEXT,
    phone TEXT,
    name TEXT NOT NULL DEFAULT '',
    preferences TEXT NOT NULL DEFAULT '{}',
    profile_summary TEXT,
    embedding BLOB,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_customers_tenant ON customers (tenant_id, email);

CREATE TABLE IF NOT EXISTS customer_identities (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    channel TEXT NOT NULL,
    channel_identity TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, channel, channel_identity)
);

CREATE TABLE IF NOT EXISTS customer_timeline (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    event_type TEXT NOT NULL,
    source TEXT NOT NULL,
    content TEXT NOT NULL,
    metadata TEXT NOT NULL DEFAULT '{}',
    embedding BLOB,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_customer_timeline_tenant_customer
    ON customer_timeline (tenant_id, customer_id);

CREATE TABLE IF NOT EXISTS customer_memory_context (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    context_graph TEXT NOT NULL DEFAULT '{}',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, customer_id)
);

CREATE TABLE IF NOT EXISTS customer360 (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL REFERENCES customers(id) ON DELETE CASCADE,
    email TEXT,
    phone TEXT,
    mood TEXT,
    preferences TEXT NOT NULL DEFAULT '{}',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_customer360_tenant_customer
    ON customer360 (tenant_id, customer_id);

CREATE TABLE IF NOT EXISTS services (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT,
    title TEXT NOT NULL DEFAULT '',
    description TEXT,
    price REAL,
    price_cents BIGINT NOT NULL DEFAULT 0,
    currency TEXT NOT NULL DEFAULT 'USD',
    duration_minutes INTEGER NOT NULL DEFAULT 60,
    requires_deposit BOOLEAN NOT NULL DEFAULT FALSE,
    deposit_amount_cents BIGINT NOT NULL DEFAULT 0,
    active BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_services_tenant ON services (tenant_id);

CREATE TABLE IF NOT EXISTS products (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT,
    title TEXT NOT NULL DEFAULT '',
    description TEXT,
    type TEXT,
    price REAL NOT NULL DEFAULT 0.0,
    price_cents BIGINT NOT NULL DEFAULT 0,
    currency TEXT NOT NULL DEFAULT 'USD',
    fulfillment_strategy TEXT,
    metadata TEXT NOT NULL DEFAULT '{}',
    inventory_count INTEGER NOT NULL DEFAULT 0,
    locked_quantity INTEGER NOT NULL DEFAULT 0,
    available_quantity INTEGER NOT NULL DEFAULT 0,
    is_sold_out BOOLEAN NOT NULL DEFAULT FALSE,
    supplier_name TEXT,
    supplier_contact TEXT,
    is_subscribable BOOLEAN NOT NULL DEFAULT FALSE,
    subscription_frequency TEXT,
    subscription_discount_percent INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_products_tenant ON products (tenant_id);

CREATE TABLE IF NOT EXISTS product_variants (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    product_id TEXT NOT NULL REFERENCES products(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    sku TEXT,
    price_modifier BIGINT NOT NULL DEFAULT 0,
    inventory_count INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_product_variants_product
    ON product_variants (tenant_id, product_id);

CREATE TABLE IF NOT EXISTS inventory_levels (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    product_id TEXT NOT NULL REFERENCES products(id) ON DELETE CASCADE,
    variant_id TEXT REFERENCES product_variants(id) ON DELETE SET NULL,
    location TEXT NOT NULL DEFAULT 'in-store',
    location_id TEXT,
    quantity INTEGER NOT NULL DEFAULT 0,
    available_count INTEGER NOT NULL DEFAULT 0,
    committed_count INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS inventory_transactions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    inventory_level_id TEXT NOT NULL REFERENCES inventory_levels(id) ON DELETE CASCADE,
    type TEXT NOT NULL,
    quantity_change INTEGER NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS inventory_adjustment_receipts (
    tenant_id TEXT NOT NULL,
    client_mutation_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    request_identity TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, client_mutation_id)
);

CREATE TABLE IF NOT EXISTS proposals (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT,
    status TEXT NOT NULL DEFAULT 'DRAFT',
    total_amount_cents BIGINT NOT NULL DEFAULT 0,
    required_deposit_cents BIGINT NOT NULL DEFAULT 0,
    checkout_url TEXT,
    stripe_checkout_session_id TEXT,
    project_scope TEXT,
    milestones TEXT,
    valid_until TIMESTAMP,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_proposals_tenant ON proposals (tenant_id);
CREATE INDEX IF NOT EXISTS idx_proposals_stripe_checkout_session_id
    ON proposals (stripe_checkout_session_id)
    WHERE stripe_checkout_session_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS proposal_line_items (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    proposal_id TEXT NOT NULL REFERENCES proposals(id) ON DELETE CASCADE,
    description TEXT NOT NULL,
    unit_price_cents BIGINT NOT NULL,
    quantity INTEGER NOT NULL DEFAULT 1,
    is_optional BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_proposal_line_items_proposal
    ON proposal_line_items (tenant_id, proposal_id);

CREATE TABLE IF NOT EXISTS quotes (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    service_id TEXT,
    proposed_slot_id TEXT,
    total_amount_cents BIGINT NOT NULL DEFAULT 0,
    required_deposit_cents BIGINT NOT NULL DEFAULT 0,
    currency TEXT NOT NULL DEFAULT 'USD',
    status TEXT NOT NULL DEFAULT 'Draft',
    stripe_payment_link TEXT,
    acceptance_receipt TEXT,
    valid_until TIMESTAMP,
    last_follow_up_at TIMESTAMP,
    follow_up_count INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_quotes_tenant ON quotes (tenant_id, status);

CREATE TABLE IF NOT EXISTS quote_line_items (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    quote_id TEXT NOT NULL REFERENCES quotes(id) ON DELETE CASCADE,
    description TEXT NOT NULL,
    unit_price_cents BIGINT NOT NULL,
    quantity INTEGER NOT NULL DEFAULT 1,
    is_optional BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS quote_requests (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT,
    status TEXT NOT NULL DEFAULT 'NEW'
        CHECK (status IN ('NEW', 'TRIAGED', 'ESTIMATING', 'PROPOSAL_DRAFTED', 'CLOSED')),
    source TEXT NOT NULL,
    message TEXT NOT NULL,
    images TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS invoices (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    client_id TEXT NOT NULL,
    customer_id TEXT,
    client_name TEXT NOT NULL DEFAULT '',
    proposal_id TEXT REFERENCES proposals(id) ON DELETE SET NULL,
    quote_id TEXT REFERENCES quotes(id) ON DELETE SET NULL,
    status TEXT NOT NULL DEFAULT 'draft',
    due_date TIMESTAMP,
    currency TEXT NOT NULL DEFAULT 'USD',
    base_currency TEXT NOT NULL DEFAULT 'USD',
    transaction_currency TEXT NOT NULL DEFAULT 'USD',
    exchange_rate REAL NOT NULL DEFAULT 1.0,
    total_amount REAL NOT NULL DEFAULT 0.0,
    total_amount_cents BIGINT,
    amount_paid_cents BIGINT NOT NULL DEFAULT 0,
    amount_refunded_cents BIGINT NOT NULL DEFAULT 0,
    payment_status TEXT NOT NULL DEFAULT 'unpaid',
    view_count INTEGER NOT NULL DEFAULT 0,
    dispute_status TEXT,
    stripe_invoice_id TEXT,
    stripe_payment_link TEXT,
    stripe_checkout_session_id TEXT,
    tax_nexus TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_invoices_tenant_due ON invoices (tenant_id, due_date, id);
CREATE INDEX IF NOT EXISTS idx_invoices_stripe_checkout_session_id
    ON invoices (stripe_checkout_session_id)
    WHERE stripe_checkout_session_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_invoices_proposal_id
    ON invoices (proposal_id)
    WHERE proposal_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS invoice_line_items (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    invoice_id TEXT NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
    description TEXT NOT NULL,
    quantity INTEGER NOT NULL DEFAULT 1,
    unit_price REAL NOT NULL,
    amount REAL NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_invoice_line_items_invoice
    ON invoice_line_items (tenant_id, invoice_id);

CREATE TABLE IF NOT EXISTS invoice_communication_events (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    invoice_id TEXT NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
    status TEXT NOT NULL DEFAULT 'drafted',
    channel TEXT NOT NULL DEFAULT 'email',
    drafted_content TEXT NOT NULL,
    provider_message_id TEXT,
    recipient_email TEXT,
    error_detail TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_invoice_comm_events_tenant_invoice
    ON invoice_communication_events (tenant_id, invoice_id, created_at DESC);

CREATE TABLE IF NOT EXISTS payment_events (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    invoice_id TEXT NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
    amount REAL NOT NULL,
    method TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    completed_at TIMESTAMP
);

CREATE TABLE IF NOT EXISTS bookings (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT,
    service_id TEXT,
    product_id TEXT,
    quote_id TEXT REFERENCES quotes(id) ON DELETE SET NULL,
    payment_intent_id TEXT,
    start_time TIMESTAMP NOT NULL,
    end_time TIMESTAMP NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    notes TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_bookings_tenant_time
    ON bookings (tenant_id, start_time, end_time);

CREATE TABLE IF NOT EXISTS booking_resources (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    resource_type TEXT NOT NULL,
    availability_schedule TEXT NOT NULL DEFAULT '[]',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS booking_resource_reservations (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    booking_id TEXT NOT NULL REFERENCES bookings(id) ON DELETE CASCADE,
    resource_id TEXT NOT NULL REFERENCES booking_resources(id) ON DELETE CASCADE,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS service_resource_requirements (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    service_id TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
    resource_type TEXT NOT NULL,
    quantity INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS availability_blocks (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    service_id TEXT NOT NULL,
    resource_id TEXT,
    start_time TIMESTAMP NOT NULL,
    end_time TIMESTAMP NOT NULL,
    is_available BOOLEAN NOT NULL DEFAULT TRUE,
    is_recurring BOOLEAN NOT NULL DEFAULT FALSE,
    recurrence_rule TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_availability_blocks_tenant_service
    ON availability_blocks (tenant_id, service_id, start_time);

CREATE TABLE IF NOT EXISTS booking_slots (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    service_id TEXT,
    resource_id TEXT,
    start_time TIMESTAMP NOT NULL,
    end_time TIMESTAMP NOT NULL,
    status TEXT NOT NULL DEFAULT 'available'
        CHECK (status IN ('available', 'soft_locked', 'booked')),
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS orders (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT,
    total_amount REAL NOT NULL DEFAULT 0.0,
    currency TEXT NOT NULL DEFAULT 'USD',
    status TEXT NOT NULL DEFAULT 'pending',
    is_consumable BOOLEAN NOT NULL DEFAULT FALSE,
    estimated_duration_days INTEGER,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_orders_tenant ON orders (tenant_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_orders_consumable_restock
    ON orders (is_consumable, estimated_duration_days, created_at);

CREATE TABLE IF NOT EXISTS order_items (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    order_id TEXT NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    product_id TEXT,
    service_id TEXT,
    quantity INTEGER NOT NULL DEFAULT 1,
    price REAL NOT NULL DEFAULT 0.0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS carts (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT,
    channel TEXT NOT NULL DEFAULT 'online',
    status TEXT NOT NULL DEFAULT 'open',
    total_amount_cents BIGINT NOT NULL DEFAULT 0,
    currency TEXT NOT NULL DEFAULT 'USD',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TIMESTAMP
);

CREATE TABLE IF NOT EXISTS cart_items (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    cart_id TEXT NOT NULL REFERENCES carts(id) ON DELETE CASCADE,
    product_id TEXT NOT NULL,
    variant_id TEXT,
    quantity INTEGER NOT NULL DEFAULT 1,
    unit_price_cents BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS subscription_plans (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    product_id TEXT,
    name TEXT NOT NULL,
    description TEXT,
    price_cents BIGINT NOT NULL,
    currency TEXT NOT NULL DEFAULT 'USD',
    frequency TEXT NOT NULL,
    cutoff_day INTEGER,
    status TEXT NOT NULL DEFAULT 'active',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (id, tenant_id)
);

CREATE INDEX IF NOT EXISTS idx_subscription_plans_tenant
    ON subscription_plans (tenant_id, status);

CREATE TABLE IF NOT EXISTS subscribers (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    subscription_plan_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'ACTIVE',
    stripe_subscription_id TEXT,
    health_score INTEGER NOT NULL DEFAULT 100,
    last_engagement_at TIMESTAMP,
    predicted_restock_date BIGINT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (subscription_plan_id, tenant_id)
        REFERENCES subscription_plans(id, tenant_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_subscribers_tenant_plan_status
    ON subscribers (tenant_id, subscription_plan_id, status);

CREATE TABLE IF NOT EXISTS subscriptions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    plan_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active',
    current_period_start TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    current_period_end TIMESTAMP NOT NULL,
    cancel_at_period_end BOOLEAN NOT NULL DEFAULT FALSE,
    canceled_at TIMESTAMP,
    health_score REAL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (plan_id, tenant_id)
        REFERENCES subscription_plans(id, tenant_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_subscriptions_tenant_status_period
    ON subscriptions (tenant_id, status, current_period_end);

CREATE TABLE IF NOT EXISTS fulfillment_batches (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    subscription_plan_id TEXT NOT NULL,
    fulfillment_date DATE NOT NULL,
    subscriber_count INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'PENDING',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (subscription_plan_id, tenant_id)
        REFERENCES subscription_plans(id, tenant_id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_fulfillment_batches_tenant_plan
    ON fulfillment_batches (tenant_id, subscription_plan_id);

CREATE TABLE IF NOT EXISTS delivery_tasks (
    id TEXT PRIMARY KEY,
    tenant_id TEXT,
    organization_id TEXT NOT NULL,
    order_id TEXT NOT NULL REFERENCES orders(id) ON DELETE CASCADE,
    driver_id TEXT,
    route_plan_id TEXT,
    provider TEXT,
    provider_delivery_id TEXT,
    status TEXT NOT NULL DEFAULT 'PENDING',
    estimated_arrival TIMESTAMP,
    delivery_location TEXT,
    delivery_location_lat REAL,
    delivery_location_lng REAL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS delivery_provider_bindings (
    delivery_task_id TEXT PRIMARY KEY REFERENCES delivery_tasks(id) ON DELETE CASCADE,
    tenant_id TEXT,
    organization_id TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('shippo', 'doordash')),
    account_namespace TEXT NOT NULL CHECK (length(account_namespace) BETWEEN 1 AND 128),
    is_test BOOLEAN NOT NULL DEFAULT FALSE,
    provider_object_id TEXT NOT NULL CHECK (length(provider_object_id) BETWEEN 1 AND 256),
    label_url TEXT,
    tracking_number TEXT,
    carrier TEXT,
    last_event_at_ms BIGINT,
    last_event_digest TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (organization_id, provider, account_namespace, is_test, provider_object_id)
);

CREATE INDEX IF NOT EXISTS idx_delivery_binding_tracking
    ON delivery_provider_bindings (organization_id, provider, account_namespace, is_test, carrier, tracking_number);

CREATE TABLE IF NOT EXISTS shipping_purchase_intents (
    id TEXT PRIMARY KEY,
    organization_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    order_id TEXT NOT NULL,
    rate_id TEXT NOT NULL,
    account_namespace TEXT NOT NULL,
    is_test INTEGER NOT NULL DEFAULT 0 CHECK (is_test IN (0, 1)),
    status TEXT NOT NULL CHECK (status IN ('dispatched', 'recorded')),
    transaction_id TEXT,
    receipt_json TEXT,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (organization_id, order_id),
    UNIQUE (organization_id, account_namespace, is_test, transaction_id),
    CHECK ((status = 'recorded' AND transaction_id IS NOT NULL AND receipt_json IS NOT NULL)
        OR (status = 'dispatched' AND receipt_json IS NULL))
);

CREATE TRIGGER IF NOT EXISTS shipping_purchase_identity_immutable
BEFORE UPDATE ON shipping_purchase_intents
WHEN OLD.id IS NOT NEW.id OR OLD.organization_id IS NOT NEW.organization_id
    OR OLD.actor_id IS NOT NEW.actor_id OR OLD.order_id IS NOT NEW.order_id
    OR OLD.rate_id IS NOT NEW.rate_id OR OLD.account_namespace IS NOT NEW.account_namespace
    OR OLD.is_test IS NOT NEW.is_test OR OLD.created_at IS NOT NEW.created_at
    OR (OLD.transaction_id IS NOT NULL AND OLD.transaction_id IS NOT NEW.transaction_id)
    OR (OLD.status = 'recorded' AND (OLD.status IS NOT NEW.status OR OLD.receipt_json IS NOT NEW.receipt_json))
BEGIN
    SELECT RAISE(ABORT, 'shipping purchase identity and recorded receipt are immutable');
END;

CREATE TABLE IF NOT EXISTS sites (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    slug TEXT NOT NULL,
    domain TEXT,
    theme_config TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'draft',
    published_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, slug)
);

CREATE TABLE IF NOT EXISTS builder_sites (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    domain TEXT,
    publication_generation BIGINT NOT NULL DEFAULT 0,
    current_publication_id TEXT,
    publication_tenant_id TEXT,
    published_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (id, publication_tenant_id)
);

CREATE TABLE IF NOT EXISTS builder_pages (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    site_id TEXT NOT NULL REFERENCES builder_sites(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    title TEXT NOT NULL,
    seo_metadata TEXT NOT NULL DEFAULT '{}',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS builder_blocks (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    page_id TEXT NOT NULL REFERENCES builder_pages(id) ON DELETE CASCADE,
    block_type TEXT NOT NULL,
    content TEXT NOT NULL DEFAULT '{}',
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS referrals (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    referral_code TEXT NOT NULL UNIQUE,
    clicks INTEGER NOT NULL DEFAULT 0,
    conversions INTEGER NOT NULL DEFAULT 0,
    created_at_unix BIGINT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS loyalty_ledger (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    points_balance INTEGER NOT NULL DEFAULT 0,
    tier_name TEXT,
    last_updated TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (tenant_id, customer_id)
);

CREATE TABLE IF NOT EXISTS pos_terminal_sessions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    device_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'ACTIVE',
    started_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_synced_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    offline_changes_count INTEGER NOT NULL DEFAULT 0,
    sync_status TEXT NOT NULL DEFAULT 'SYNCED',
    last_conflict_resolved_at TIMESTAMP,
    UNIQUE (tenant_id, device_id)
);

CREATE TABLE IF NOT EXISTS pos_offline_transactions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    client_id TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'PENDING',
    amount_cents BIGINT NOT NULL,
    currency TEXT NOT NULL,
    payload TEXT NOT NULL,
    device_signature TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS terminal_cash_receipts (
    tenant_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    order_id TEXT NOT NULL REFERENCES orders(id),
    amount_cents BIGINT NOT NULL CHECK (amount_cents >= 0),
    customer_id TEXT REFERENCES customers(id),
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, operation_id),
    UNIQUE (order_id)
);

CREATE TABLE IF NOT EXISTS terminal_payment_operations (
    tenant_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    amount_cents BIGINT NOT NULL CHECK (amount_cents > 0 AND amount_cents <= 99999999),
    currency TEXT NOT NULL,
    provider_fingerprint TEXT NOT NULL,
    stripe_payment_intent_id TEXT,
    state TEXT NOT NULL CHECK (state IN ('creating', 'ready', 'capturing', 'succeeded', 'reconciliation_required')),
    provider_receipt TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (tenant_id, operation_id),
    UNIQUE (tenant_id, stripe_payment_intent_id),
    UNIQUE (provider_fingerprint, stripe_payment_intent_id),
    CHECK (state NOT IN ('ready', 'capturing', 'succeeded') OR stripe_payment_intent_id IS NOT NULL),
    CHECK (state <> 'succeeded' OR provider_receipt IS NOT NULL)
);

CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    quote_id TEXT REFERENCES quotes(id) ON DELETE SET NULL,
    customer_id TEXT NOT NULL,
    title TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'Active',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS project_tasks (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'Pending',
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS leads (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    source TEXT,
    contact_info TEXT,
    context TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS opportunities (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    lead_id TEXT REFERENCES leads(id) ON DELETE SET NULL,
    title TEXT NOT NULL,
    stage TEXT NOT NULL DEFAULT 'Qualified',
    estimated_value BIGINT,
    priority TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS ohc_staff_member (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    phone_number TEXT NOT NULL,
    role TEXT NOT NULL,
    pin_hash TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS ohc_timecard_event (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    staff_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    event_time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    sync_status TEXT NOT NULL DEFAULT 'SYNCED',
    request_identity TEXT,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS shifts (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    location_id TEXT,
    staff_id TEXT REFERENCES ohc_staff_member(id) ON DELETE SET NULL,
    start_time TIMESTAMP NOT NULL,
    end_time TIMESTAMP NOT NULL,
    role TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'scheduled'
        CHECK (status IN ('scheduled', 'in_progress', 'completed', 'cancelled', 'called_out')),
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);
