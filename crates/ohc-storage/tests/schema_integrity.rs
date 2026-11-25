// SPDX-License-Identifier: Apache-2.0
//! Schema foreign-key integrity and live PostgreSQL migration verification tests
//! for `ohc-storage`.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::time::Duration;

use ohc_storage::{migrate_postgres, migrate_sqlite};
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

#[tokio::test]
async fn sqlite_foreign_keys_target_valid_unique_parent_keys_and_enforce_terminal_cash_cascade() {
    let temp_dir = tempfile::tempdir().expect("create temp dir");
    let db_path = temp_dir.path().join("ohc_storage_fk_audit.sqlite");
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
        .expect("parse sqlite url")
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("open real sqlite database");

    migrate_sqlite(&pool)
        .await
        .expect("apply all sqlite migrations");

    let fk_check_rows = sqlx::query("PRAGMA foreign_key_check;")
        .fetch_all(&pool)
        .await
        .expect("PRAGMA foreign_key_check must pass across all tables");
    assert!(
        fk_check_rows.is_empty(),
        "expected 0 foreign key violations, got {}",
        fk_check_rows.len()
    );

    let tables: BTreeSet<String> = sqlx::query(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations'",
    )
    .fetch_all(&pool)
    .await
    .expect("list sqlite tables")
    .into_iter()
    .map(|row| row.get::<String, _>("name"))
    .collect();

    let mut table_columns: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut table_unique_key_sets: BTreeMap<String, BTreeSet<Vec<String>>> = BTreeMap::new();

    for table in &tables {
        let info_rows = sqlx::query(&format!("PRAGMA table_info(\"{table}\");"))
            .fetch_all(&pool)
            .await
            .expect("read table_info");
        let mut cols = BTreeSet::new();
        let mut pk_cols: Vec<(i64, String)> = Vec::new();
        for row in info_rows {
            let name: String = row.get("name");
            let pk_order: i64 = row.get("pk");
            cols.insert(name.clone());
            if pk_order > 0 {
                pk_cols.push((pk_order, name));
            }
        }
        pk_cols.sort_by_key(|(ord, _)| *ord);
        let mut unique_sets: BTreeSet<Vec<String>> = BTreeSet::new();
        if !pk_cols.is_empty() {
            let mut key: Vec<String> = pk_cols.into_iter().map(|(_, col)| col).collect();
            key.sort();
            unique_sets.insert(key);
        }

        let idx_rows = sqlx::query(&format!("PRAGMA index_list(\"{table}\");"))
            .fetch_all(&pool)
            .await
            .expect("read index_list");
        for idx in idx_rows {
            let is_unique: i64 = idx.get("unique");
            let is_partial: i64 = idx.get("partial");
            if is_unique == 1 && is_partial == 0 {
                let idx_name: String = idx.get("name");
                let idx_cols = sqlx::query(&format!("PRAGMA index_info(\"{idx_name}\");"))
                    .fetch_all(&pool)
                    .await
                    .expect("read index_info");
                let mut key: Vec<String> = idx_cols
                    .into_iter()
                    .filter_map(|r| r.get::<Option<String>, _>("name"))
                    .collect();
                if !key.is_empty() {
                    key.sort();
                    unique_sets.insert(key);
                }
            }
        }

        table_columns.insert(table.clone(), cols);
        table_unique_key_sets.insert(table.clone(), unique_sets);
    }

    struct ForeignKeyEntry {
        seq: i64,
        parent: String,
        to_col: Option<String>,
    }

    let mut fk_errors = Vec::new();
    for table in &tables {
        let fk_rows = sqlx::query(&format!("PRAGMA foreign_key_list(\"{table}\");"))
            .fetch_all(&pool)
            .await
            .expect("read foreign_key_list");
        let mut grouped: BTreeMap<i64, Vec<ForeignKeyEntry>> = BTreeMap::new();
        for row in fk_rows {
            let id: i64 = row.get("id");
            grouped.entry(id).or_default().push(ForeignKeyEntry {
                seq: row.get("seq"),
                parent: row.get("table"),
                to_col: row.get("to"),
            });
        }
        for (_id, mut entries) in grouped {
            entries.sort_by_key(|entry| entry.seq);
            let parent = entries[0].parent.clone();
            let Some(parent_cols) = table_columns.get(&parent) else {
                fk_errors.push(format!(
                    "{table} references missing parent table `{parent}`"
                ));
                continue;
            };
            let to_cols: Vec<Option<String>> =
                entries.iter().map(|entry| entry.to_col.clone()).collect();
            if to_cols.iter().all(Option::is_none) {
                continue;
            }
            let target_cols: Vec<String> = to_cols.into_iter().flatten().collect();
            let missing: Vec<&String> = target_cols
                .iter()
                .filter(|col| !parent_cols.contains(*col))
                .collect();
            if !missing.is_empty() {
                fk_errors.push(format!(
                    "{table} references missing column(s) {missing:?} on `{parent}`"
                ));
                continue;
            }
            let mut normalized_target = target_cols.clone();
            normalized_target.sort();
            let parent_uniques = table_unique_key_sets
                .get(&parent)
                .expect("parent unique sets");
            if !parent_uniques.contains(&normalized_target) {
                fk_errors.push(format!(
                    "{table} references non-unique column set {target_cols:?} on `{parent}` (uniques={parent_uniques:?})"
                ));
            }
        }
    }
    assert!(
        fk_errors.is_empty(),
        "SQLite foreign key schema errors detected:\n{}",
        fk_errors.join("\n")
    );

    // Exercise real composite FK + ON DELETE CASCADE on terminal_cash_receipts / items
    // and CHECK constraints on terminal_payment_operations.
    sqlx::query("INSERT INTO tenants (id, name) VALUES ('ws-pos', 'Corner Cafe')")
        .execute(&pool)
        .await
        .expect("insert tenant");
    sqlx::query("INSERT INTO customers (id, tenant_id, name) VALUES ('cust-1', 'ws-pos', 'Pat')")
        .execute(&pool)
        .await
        .expect("insert customer");
    sqlx::query(
        "INSERT INTO orders (id, tenant_id, customer_id, total_amount, currency, status) \
         VALUES ('ord-cash-1', 'ws-pos', 'cust-1', 12.50, 'USD', 'paid')",
    )
    .execute(&pool)
    .await
    .expect("insert order");
    sqlx::query(
        "INSERT INTO products (id, tenant_id, name, price) VALUES ('prod-1', 'ws-pos', 'Latte', 12.50)",
    )
    .execute(&pool)
    .await
    .expect("insert product");

    // Orphan child row must fail FK constraint.
    let orphan_err = sqlx::query(
        "INSERT INTO terminal_cash_receipt_items (tenant_id, operation_id, product_id, quantity, amount_cents, lock_id) \
         VALUES ('ws-pos', 'op-missing', 'prod-1', 1, 1250, 'lock-1')",
    )
    .execute(&pool)
    .await;
    assert!(
        orphan_err.is_err(),
        "orphan terminal_cash_receipt_items row must fail composite FK"
    );

    sqlx::query(
        "INSERT INTO terminal_cash_receipts (tenant_id, operation_id, order_id, amount_cents, customer_id) \
         VALUES ('ws-pos', 'op-cash-1', 'ord-cash-1', 1250, 'cust-1')",
    )
    .execute(&pool)
    .await
    .expect("insert terminal_cash_receipts parent");

    sqlx::query(
        "INSERT INTO terminal_cash_receipt_items (tenant_id, operation_id, product_id, quantity, amount_cents, lock_id) \
         VALUES ('ws-pos', 'op-cash-1', 'prod-1', 1, 1250, 'lock-1')",
    )
    .execute(&pool)
    .await
    .expect("insert terminal_cash_receipt_items child");

    sqlx::query(
        "DELETE FROM terminal_cash_receipts WHERE tenant_id = 'ws-pos' AND operation_id = 'op-cash-1'",
    )
    .execute(&pool)
    .await
    .expect("delete parent receipt");

    let remaining_items: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM terminal_cash_receipt_items WHERE tenant_id = 'ws-pos' AND operation_id = 'op-cash-1'",
    )
    .fetch_one(&pool)
    .await
    .expect("count cascaded child items");
    assert_eq!(
        remaining_items, 0,
        "ON DELETE CASCADE must remove child items"
    );

    sqlx::query(
        "INSERT INTO terminal_payment_operations (tenant_id, operation_id, amount_cents, currency, provider_fingerprint, state) \
         VALUES ('ws-pos', 'op-card-1', 1250, 'usd', 'fp-1', 'creating')",
    )
    .execute(&pool)
    .await
    .expect("insert terminal_payment_operations row");

    let invalid_state = sqlx::query(
        "UPDATE terminal_payment_operations SET state = 'succeeded' WHERE tenant_id = 'ws-pos' AND operation_id = 'op-card-1'",
    )
    .execute(&pool)
    .await;
    assert!(
        invalid_state.is_err(),
        "CHECK constraint must reject succeeded payment operation without receipt or intent id"
    );

    // Verify first-party agent definitions are seeded and immutable on SQLite.
    let seeded_defs: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, source FROM agent_definitions WHERE source = 'first_party' ORDER BY id ASC",
    )
    .fetch_all(&pool)
    .await
    .expect("read seeded first_party agent_definitions on SQLite");
    assert_eq!(
        seeded_defs,
        vec![
            (
                "00000000-0000-4000-8000-000000000001".to_string(),
                "first_party".to_string()
            ),
            (
                "00000000-0000-4000-8000-000000000002".to_string(),
                "first_party".to_string()
            ),
        ]
    );
    assert!(
        sqlx::query(
            "DELETE FROM agent_definitions WHERE id = '00000000-0000-4000-8000-000000000001'"
        )
        .execute(&pool)
        .await
        .is_err(),
        "first_party agent_definitions must be immutable on SQLite"
    );

    // Verify newly inserted products leave pn_counter_p / pn_counter_n NULL and
    // delivery_provider_bindings.last_event_at_ms defaults to NULL.
    let pn_counters: (Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT pn_counter_p, pn_counter_n FROM products WHERE id = 'prod-1'")
            .fetch_one(&pool)
            .await
            .expect("read sqlite product pn_counters");
    assert_eq!(pn_counters, (None, None));

    sqlx::query(
        "INSERT INTO delivery_tasks (id, tenant_id, organization_id, order_id, status, provider) \
         VALUES ('00000000-0000-4000-8000-000000000099', 'ws-pos', 'ws-pos', 'ord-cash-1', 'LABEL_CREATED', 'shippo')",
    )
    .execute(&pool)
    .await
    .expect("insert sqlite delivery_task");
    sqlx::query(
        "INSERT INTO delivery_provider_bindings (delivery_task_id, tenant_id, organization_id, provider, account_namespace, is_test, provider_object_id) \
         VALUES ('00000000-0000-4000-8000-000000000099', 'ws-pos', 'ws-pos', 'shippo', 'acct-test', 1, 'tx_test_1')",
    )
    .execute(&pool)
    .await
    .expect("insert sqlite delivery_provider_binding");
    let last_event_at_ms: Option<i64> = sqlx::query_scalar(
        "SELECT last_event_at_ms FROM delivery_provider_bindings WHERE delivery_task_id = '00000000-0000-4000-8000-000000000099'",
    )
    .fetch_one(&pool)
    .await
    .expect("read sqlite last_event_at_ms");
    assert_eq!(last_event_at_ms, None);
}

#[tokio::test]
async fn postgres_migrations_apply_cleanly_are_idempotent_and_enforce_terminal_cash_constraints() {
    let url = std::env::var("OHC_STORAGE_TEST_DATABASE_URL")
        .ok()
        .filter(|v| v.starts_with("postgres://") || v.starts_with("postgresql://"))
        .or_else(|| {
            std::env::var("OMNISOLO_POSTGRES_ADMIN_URL")
                .ok()
                .filter(|v| v.starts_with("postgres://") || v.starts_with("postgresql://"))
        })
        .or_else(|| {
            std::env::var("OMNISOLO_DATABASE_URL")
                .ok()
                .filter(|v| v.starts_with("postgres://") || v.starts_with("postgresql://"))
        });

    let require_pg = std::env::var("OMNISOLO_REQUIRE_POSTGRES_TESTS").ok();
    let Some(database_url) = url else {
        assert!(
            require_pg.as_deref() != Some("1"),
            "OMNISOLO_REQUIRE_POSTGRES_TESTS=1 requires a PostgreSQL URL in OHC_STORAGE_TEST_DATABASE_URL, OMNISOLO_POSTGRES_ADMIN_URL, or OMNISOLO_DATABASE_URL"
        );
        return;
    };

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&database_url)
        .await
        .expect("connect to live PostgreSQL database");

    migrate_postgres(&pool)
        .await
        .expect("first PostgreSQL migration run must succeed");
    migrate_postgres(&pool)
        .await
        .expect("re-running PostgreSQL migrations must be idempotent");

    let mut tx = pool.begin().await.expect("start transaction");
    sqlx::query("INSERT INTO tenants (id, name) VALUES ('pg-fk-ws', 'PG Test Shop')")
        .execute(&mut *tx)
        .await
        .expect("insert tenant");
    sqlx::query(
        "INSERT INTO customers (id, tenant_id, name) VALUES ('pg-cust-1', 'pg-fk-ws', 'Ada')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert customer");
    sqlx::query(
        "INSERT INTO orders (id, tenant_id, customer_id, total_amount, currency, status) \
         VALUES ('pg-ord-1', 'pg-fk-ws', 'pg-cust-1', 25.00, 'USD', 'paid')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert order");
    sqlx::query(
        "INSERT INTO products (id, tenant_id, name, price) VALUES ('pg-prod-1', 'pg-fk-ws', 'Coffee', 25.00)",
    )
    .execute(&mut *tx)
    .await
    .expect("insert product");
    sqlx::query(
        "INSERT INTO terminal_cash_receipts (tenant_id, operation_id, order_id, amount_cents, customer_id) \
         VALUES ('pg-fk-ws', 'pg-op-1', 'pg-ord-1', 2500, 'pg-cust-1')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert terminal_cash_receipts");
    sqlx::query(
        "INSERT INTO terminal_cash_receipt_items (tenant_id, operation_id, product_id, quantity, amount_cents, lock_id) \
         VALUES ('pg-fk-ws', 'pg-op-1', 'pg-prod-1', 2, 2500, 'pg-lock-1')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert terminal_cash_receipt_items");
    sqlx::query(
        "DELETE FROM terminal_cash_receipts WHERE tenant_id = 'pg-fk-ws' AND operation_id = 'pg-op-1'",
    )
    .execute(&mut *tx)
    .await
    .expect("delete terminal_cash_receipts parent");

    let cascaded_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM terminal_cash_receipt_items WHERE tenant_id = 'pg-fk-ws' AND operation_id = 'pg-op-1'",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("verify ON DELETE CASCADE in PostgreSQL");
    assert_eq!(cascaded_count, 0);

    sqlx::query(
        "INSERT INTO terminal_payment_operations (tenant_id, operation_id, amount_cents, currency, provider_fingerprint, state) \
         VALUES ('pg-fk-ws', 'pg-pay-1', 2500, 'usd', 'fp-pg-1', 'creating')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert terminal_payment_operations");

    tx.rollback().await.expect("rollback verification tx");
}

#[tokio::test]
async fn postgres_schema_types_arrays_jsonb_and_portable_role_backfill_succeed() {
    let url = std::env::var("OHC_STORAGE_TEST_DATABASE_URL")
        .ok()
        .filter(|v| v.starts_with("postgres://") || v.starts_with("postgresql://"))
        .or_else(|| {
            std::env::var("OMNISOLO_POSTGRES_ADMIN_URL")
                .ok()
                .filter(|v| v.starts_with("postgres://") || v.starts_with("postgresql://"))
        })
        .or_else(|| {
            std::env::var("OMNISOLO_DATABASE_URL")
                .ok()
                .filter(|v| v.starts_with("postgres://") || v.starts_with("postgresql://"))
        });

    let require_pg = std::env::var("OMNISOLO_REQUIRE_POSTGRES_TESTS").ok();
    let Some(database_url) = url else {
        assert!(
            require_pg.as_deref() != Some("1"),
            "OMNISOLO_REQUIRE_POSTGRES_TESTS=1 requires a PostgreSQL URL in OHC_STORAGE_TEST_DATABASE_URL, OMNISOLO_POSTGRES_ADMIN_URL, or OMNISOLO_DATABASE_URL"
        );
        return;
    };

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&database_url)
        .await
        .expect("connect to live PostgreSQL database");

    migrate_postgres(&pool)
        .await
        .expect("PostgreSQL migrations must succeed");

    let expected_udt_types = [
        ("tenants", "enabled_currencies", "jsonb"),
        ("users", "roles", "_text"),
        ("roles", "permissions", "_text"),
        ("api_keys", "id", "uuid"),
        ("onboarding_state", "state_json", "jsonb"),
        ("onboarding_state", "preparation_receipt", "jsonb"),
        ("customers", "preferences", "jsonb"),
        ("customers", "profile_summary", "jsonb"),
        ("customer_timeline", "metadata", "jsonb"),
        ("customer_memory_context", "id", "uuid"),
        ("customer_memory_context", "context_graph", "jsonb"),
        ("products", "metadata", "jsonb"),
        ("products", "seo_schema_json", "jsonb"),
        ("product_variants", "price_modifier", "int8"),
        ("proposals", "milestones", "jsonb"),
        ("quotes", "acceptance_receipt", "jsonb"),
        ("quote_line_items", "service_item_id", "uuid"),
        ("invoices", "due_date", "timestamptz"),
        ("invoices", "total_amount_cents", "int4"),
        ("invoices", "amount_paid_cents", "int4"),
        ("invoices", "amount_refunded_cents", "int4"),
        ("booking_resources", "availability_schedule", "jsonb"),
        ("delivery_tasks", "id", "uuid"),
        ("delivery_provider_bindings", "delivery_task_id", "uuid"),
        ("builder_sites", "id", "uuid"),
        ("builder_sites", "tenant_id", "uuid"),
        ("builder_pages", "seo_metadata", "jsonb"),
        ("builder_blocks", "content", "jsonb"),
        ("builder_publications", "publication_id", "uuid"),
        ("pos_offline_transactions", "payload", "jsonb"),
        ("agent_feed", "payload", "jsonb"),
        ("agent_feed_items", "context_payload", "jsonb"),
        ("agent_feed_items", "proposed_action", "jsonb"),
        ("agent_feed_decisions", "dispatch_payload", "jsonb"),
        ("agent_action_requests", "payload", "jsonb"),
        ("agent_approvals", "payload", "jsonb"),
        ("ohc_job_queue", "payload", "jsonb"),
        ("sub_agent_queue", "payload", "jsonb"),
        ("business_milestones", "metadata", "jsonb"),
        ("tenant_workflow_receipts", "generation", "int8"),
        ("assistant_execution_tasks", "archived", "int4"),
        ("assistant_tasks", "model_config", "jsonb"),
        ("assistant_messages", "tool_metadata", "jsonb"),
        ("assistant_connectors", "config", "jsonb"),
        ("assistant_skills", "config", "jsonb"),
        ("harness_sessions", "labels", "jsonb"),
        ("harness_tasks", "dependency_task_ids", "jsonb"),
        ("harness_turns", "output_message_ids", "jsonb"),
        ("affiliate_ledgers", "commission_amount", "int8"),
        ("agent_session_data", "_sync_status", "text"),
    ];

    for (table, column, expected_udt) in expected_udt_types {
        let actual_udt: String = sqlx::query_scalar(
            "SELECT udt_name FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2",
        )
        .bind(table)
        .bind(column)
        .fetch_one(&pool)
        .await
        .unwrap_or_else(|err| panic!("missing column {table}.{column}: {err}"));
        assert_eq!(
            actual_udt, expected_udt,
            "PostgreSQL column {table}.{column} must have udt_name `{expected_udt}`, got `{actual_udt}`"
        );
    }

    let mut tx = pool.begin().await.expect("start regression transaction");

    sqlx::query(
        "INSERT INTO tenants (id, name, enabled_currencies) \
         VALUES ('pg-type-ws', 'Type Check Bakery', '[\"USD\", \"EUR\"]'::jsonb)",
    )
    .execute(&mut *tx)
    .await
    .expect("insert tenant with JSONB enabled_currencies");

    sqlx::query(
        "INSERT INTO roles (id, tenant_id, name, permissions) \
         VALUES ('pg-type-role', 'pg-type-ws', 'ADMIN', ARRAY['orders:read', 'orders:write']::TEXT[])",
    )
    .execute(&mut *tx)
    .await
    .expect("insert role with TEXT[] permissions");

    sqlx::query(
        "INSERT INTO users (id, username, email, password_hash, roles, active, tenant_id) \
         VALUES ('pg-type-user', 'typecheck@example.test', 'typecheck@example.test', 'hash', ARRAY['ADMIN', 'OPERATOR']::TEXT[], TRUE, 'pg-type-ws')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert user with TEXT[] roles");

    // Exercise the exact portable role backfill statement used during server startup.
    sqlx::query(
        "INSERT INTO identity_user_roles (user_id, role_name, tenant_id, position) \
         SELECT users.id, role_name, users.tenant_id, position::INTEGER - 1 \
         FROM users \
         CROSS JOIN LATERAL unnest(COALESCE(users.roles, ARRAY[]::TEXT[])) WITH ORDINALITY AS legacy_roles(role_name, position) \
         WHERE users.id = 'pg-type-user' \
         ON CONFLICT (user_id, role_name) DO NOTHING",
    )
    .execute(&mut *tx)
    .await
    .expect("portable role backfill unnest(COALESCE(users.roles, ARRAY[]::TEXT[])) must succeed");

    let backfilled_roles: Vec<(String, i32)> = sqlx::query_as(
        "SELECT role_name, position FROM identity_user_roles \
         WHERE user_id = 'pg-type-user' ORDER BY position ASC",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("query backfilled identity_user_roles");
    assert_eq!(
        backfilled_roles,
        vec![("ADMIN".to_string(), 0), ("OPERATOR".to_string(), 1)]
    );

    sqlx::query(
        "INSERT INTO business_milestones (id, tenant_id, milestone_type, metadata) \
         VALUES ('pg-type-milestone', 'DEFAULT', 'first_sale', '{\"title\":\"First Sale\"}'::jsonb)",
    )
    .execute(&mut *tx)
    .await
    .expect("insert business_milestones with DEFAULT tenant_id and JSONB metadata");

    sqlx::query(
        "INSERT INTO agent_feed_decisions (tenant_id, action_id, origin, decision_state, actor_id, token_id, dispatch_payload, dispatch_status) \
         VALUES ('pg-type-ws', 'pg-type-action', 'agent_feed', 'APPROVED', 'pg-type-user', 'tok-1', '{\"action\":\"approve\"}'::jsonb, 'NOT_REQUESTED')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert canonical agent_feed_decisions row with JSONB dispatch_payload");

    // Verify affiliate_ledgers.commission_amount query used by GET /api/v1/growth/affiliate/stats.
    sqlx::query(
        "INSERT INTO affiliate_ledgers (id, tenant_id, customer_id, commission_amount) \
         VALUES ('aff-led-1', 'pg-type-ws', 'cust-1', 1250)",
    )
    .execute(&mut *tx)
    .await
    .expect("insert affiliate_ledgers row with commission_amount");
    let total_commission: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(commission_amount), 0)::bigint FROM affiliate_ledgers WHERE tenant_id = $1",
    )
    .bind("pg-type-ws")
    .fetch_one(&mut *tx)
    .await
    .expect("sum affiliate_ledgers.commission_amount");
    assert_eq!(total_commission, 1250);

    // Verify agent_session_data._sync_status query used by Agent Memory Pipeline.
    sqlx::query(
        "INSERT INTO agents (id, tenant_id, name, role, status) \
         VALUES ('pg-type-agent', 'pg-type-ws', 'Memory Agent', 'Operations', 'ACTIVE')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert agent for agent_session_data check");
    sqlx::query(
        "INSERT INTO agent_session_data (session_id, tenant_id, agent_id, context_data) \
         VALUES ('pg-type-sess', 'pg-type-ws', 'pg-type-agent', '{\"note\":\"hello\"}')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert agent_session_data with default _sync_status");
    let pending_sessions: Vec<(String, String)> = sqlx::query_as(
        "SELECT s.session_id, a.tenant_id \
         FROM agent_session_data s \
         JOIN agents a ON s.agent_id = a.id \
         WHERE s._sync_status IS NULL OR s._sync_status != 'processing' OR s.updated_at < NOW() - INTERVAL '10 minutes' \
         ORDER BY s.last_accessed ASC \
         FOR UPDATE OF s SKIP LOCKED \
         LIMIT 100",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("query agent_session_data._sync_status for Agent Memory Pipeline");
    assert!(
        pending_sessions
            .iter()
            .any(|(sid, tid)| sid == "pg-type-sess" && tid == "pg-type-ws"),
        "expected pending session to be returned by Agent Memory Pipeline query"
    );

    // Verify ohc_quote_acceptance_guard trigger monotonically bumps quotes.updated_at on edit.
    sqlx::query(
        "INSERT INTO quotes (id, tenant_id, customer_id, status, total_amount_cents, required_deposit_cents) \
         VALUES ('11111111-1111-4111-8111-111111111111', 'pg-type-ws', 'cust-1', 'DRAFT', 5000, 1000)",
    )
    .execute(&mut *tx)
    .await
    .expect("insert quote for trigger verification");
    let before_updated_at: String = sqlx::query_scalar(
        "SELECT to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') FROM quotes WHERE id = '11111111-1111-4111-8111-111111111111'",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("read initial quotes.updated_at");
    sqlx::query(
        "UPDATE quotes SET total_amount_cents = 9000 WHERE id = '11111111-1111-4111-8111-111111111111'",
    )
    .execute(&mut *tx)
    .await
    .expect("update quote terms");
    let after_updated_at: String = sqlx::query_scalar(
        "SELECT to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') FROM quotes WHERE id = '11111111-1111-4111-8111-111111111111'",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("read updated quotes.updated_at");
    assert_ne!(
        before_updated_at, after_updated_at,
        "ohc_quote_acceptance_guard trigger must advance quotes.updated_at on economic term mutation"
    );

    // Verify first-party agent_definitions are seeded idempotently on PostgreSQL.
    let seeded_pg_defs: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, source FROM agent_definitions WHERE source = 'first_party' ORDER BY id ASC",
    )
    .fetch_all(&mut *tx)
    .await
    .expect("query first_party agent_definitions on PostgreSQL");
    assert_eq!(
        seeded_pg_defs,
        vec![
            (
                "00000000-0000-4000-8000-000000000001".to_string(),
                "first_party".to_string()
            ),
            (
                "00000000-0000-4000-8000-000000000002".to_string(),
                "first_party".to_string()
            ),
        ]
    );

    // Verify products.pn_counter_p / pn_counter_n default to NULL on PostgreSQL.
    sqlx::query(
        "INSERT INTO products (id, tenant_id, title, price, price_cents, inventory_count, available_quantity, locked_quantity) \
         VALUES ('pg-null-pn-prod', 'pg-type-ws', 'Plain Stock Cake', 10.0, 1000, 12, 12, 0)",
    )
    .execute(&mut *tx)
    .await
    .expect("insert product without explicit pn_counters");
    let pg_pn_counters: (Option<i64>, Option<i64>) = sqlx::query_as(
        "SELECT pn_counter_p, pn_counter_n FROM products WHERE id = 'pg-null-pn-prod'",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("read pg pn_counters");
    assert_eq!(pg_pn_counters, (None, None));

    // Verify delivery_provider_bindings.last_event_at_ms is nullable with NULL default on PG.
    sqlx::query(
        "INSERT INTO orders (id, tenant_id, total_amount, currency, status) \
         VALUES ('pg-ship-ord-1', 'pg-type-ws', 10.00, 'USD', 'pending')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert shipping order");
    sqlx::query(
        "INSERT INTO delivery_tasks (id, tenant_id, organization_id, order_id, status, provider) \
         VALUES ('00000000-0000-4000-8000-000000000199'::uuid, 'pg-type-ws', 'pg-type-ws', 'pg-ship-ord-1', 'LABEL_CREATED', 'shippo')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert delivery_task");
    sqlx::query(
        "INSERT INTO delivery_provider_bindings (delivery_task_id, tenant_id, organization_id, provider, account_namespace, is_test, provider_object_id) \
         VALUES ('00000000-0000-4000-8000-000000000199'::uuid, 'pg-type-ws', 'pg-type-ws', 'shippo', 'acct-pg-test', TRUE, 'tx_pg_test_1')",
    )
    .execute(&mut *tx)
    .await
    .expect("insert delivery_provider_binding without last_event_at_ms");
    let pg_last_event_at_ms: Option<i64> = sqlx::query_scalar(
        "SELECT last_event_at_ms FROM delivery_provider_bindings WHERE delivery_task_id = '00000000-0000-4000-8000-000000000199'::uuid",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("read delivery_provider_bindings.last_event_at_ms");
    assert_eq!(pg_last_event_at_ms, None);

    tx.rollback()
        .await
        .expect("rollback regression transaction");
}
