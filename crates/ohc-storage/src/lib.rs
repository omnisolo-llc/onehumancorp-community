// SPDX-License-Identifier: Apache-2.0
//! Unified single-tenant storage migrations for OneHumanCorp Community Edition.
//!
//! Provides versioned `sqlx` migrators for SQLCipher-backed SQLite and portable
//! single-tenant PostgreSQL, together with a schema parser used to enforce
//! strict table-set and column-set parity across both engines.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use sqlx::migrate::Migrator;
use sqlx::{PgPool, SqlitePool};

/// Advisory lock identifier (`"OHCMIGR1"` in ASCII hex) preventing concurrent
/// PostgreSQL migration execution across processes.
pub const POSTGRES_MIGRATION_LOCK_KEY: i64 = 0x4f48_434d_4947_5231;

/// Embedded versioned single-tenant SQLite migrations.
pub static SQLITE_MIGRATOR: Migrator = sqlx::migrate!("./migrations/sqlite");

/// Embedded versioned single-tenant PostgreSQL migrations.
pub static POSTGRES_MIGRATOR: Migrator = sqlx::migrate!("./migrations/postgres");

/// Errors returned while running storage migrations.
#[derive(Debug, thiserror::Error)]
pub enum StorageMigrationError {
    /// Failed to execute a SQL statement or acquire a pooled connection.
    #[error("storage sql error: {0}")]
    Sqlx(#[from] sqlx::Error),

    /// Failed while applying embedded `sqlx` migrations.
    #[error("storage migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

/// Applies the unified single-tenant SQLite schema migrations idempotently.
///
/// Enables foreign-key enforcement before executing all embedded SQLite
/// migrations. Re-running on an already-migrated database is a clean no-op.
pub async fn migrate_sqlite(pool: &SqlitePool) -> Result<(), StorageMigrationError> {
    sqlx::query("PRAGMA foreign_keys = ON;")
        .execute(pool)
        .await?;
    SQLITE_MIGRATOR.run(pool).await?;
    Ok(())
}

/// Applies the unified single-tenant PostgreSQL schema migrations idempotently.
///
/// Serializes concurrent callers with a session-level PostgreSQL advisory lock
/// that is always released before returning.
pub async fn migrate_postgres(pool: &PgPool) -> Result<(), StorageMigrationError> {
    let mut conn = pool.acquire().await?;
    sqlx::query("SELECT pg_advisory_lock($1);")
        .bind(POSTGRES_MIGRATION_LOCK_KEY)
        .execute(&mut *conn)
        .await?;

    let migration_result = POSTGRES_MIGRATOR.run(&mut *conn).await;

    let unlock_result = sqlx::query("SELECT pg_advisory_unlock($1);")
        .bind(POSTGRES_MIGRATION_LOCK_KEY)
        .execute(&mut *conn)
        .await;

    migration_result?;
    unlock_result?;
    Ok(())
}

/// Extracts a normalized map of `table_name -> set(column_name)` from SQL DDL.
///
/// Supports `CREATE TABLE [IF NOT EXISTS] [schema.]table (...)` and
/// `ALTER TABLE [IF EXISTS] [schema.]table ADD [COLUMN] [IF NOT EXISTS] col ...`,
/// while cleanly stripping line comments, block comments, string literals, and
/// dollar-quoted PL/pgSQL bodies.
pub fn extract_schema_tables_and_columns(sql: &str) -> BTreeMap<String, BTreeSet<String>> {
    let cleaned = strip_sql_comments_and_dollar_blocks(sql);
    let mut tables: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for raw_stmt in split_top_level_statements(&cleaned) {
        let stmt = raw_stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        let upper = stmt.to_ascii_uppercase();
        if upper.starts_with("CREATE TABLE") {
            if let Some((table_name, cols)) = parse_create_table(stmt) {
                tables.entry(table_name).or_default().extend(cols);
            }
        } else if upper.starts_with("ALTER TABLE")
            && let Some((table_name, col_name)) = parse_alter_add_column(stmt)
        {
            tables.entry(table_name).or_default().insert(col_name);
        }
    }

    tables
}

/// Reads every `*.sql` file in `dir` in sorted filename order and merges all
/// extracted tables and columns into a single canonical schema map.
pub fn extract_directory_schema(dir: &Path) -> std::io::Result<BTreeMap<String, BTreeSet<String>>> {
    let mut sql_files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("sql") {
            sql_files.push(path);
        }
    }
    sql_files.sort();

    let mut combined: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in sql_files {
        let content = std::fs::read_to_string(&path)?;
        for (table, cols) in extract_schema_tables_and_columns(&content) {
            combined.entry(table).or_default().extend(cols);
        }
    }
    Ok(combined)
}

fn strip_sql_comments_and_dollar_blocks(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;

    while i < len {
        // Line comment: -- ... \n
        if i + 1 < len && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            i += 2;
            while i < len && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(' ');
            continue;
        }
        // Block comment: /* ... */
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(len);
            out.push(' ');
            continue;
        }
        // Single-quoted string literal: keep a neutral placeholder so semicolons
        // or parentheses inside string defaults never confuse statement splitting.
        if bytes[i] == b'\'' {
            out.push('\'');
            i += 1;
            while i < len {
                if bytes[i] == b'\'' {
                    if i + 1 < len && bytes[i + 1] == b'\'' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push('\'');
            continue;
        }
        // Dollar-quoted body ($tag$ ... $tag$ or $$ ... $$): replace with empty literal.
        if bytes[i] == b'$'
            && let Some((tag_end, tag)) = parse_dollar_tag(bytes, i)
        {
            i = tag_end;
            while i < len {
                if bytes[i] == b'$'
                    && let Some((close_end, close_tag)) = parse_dollar_tag(bytes, i)
                    && close_tag == tag
                {
                    i = close_end;
                    break;
                }
                i += 1;
            }
            out.push_str(" '' ");
            continue;
        }

        let ch = sql[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }

    out
}

fn parse_dollar_tag(bytes: &[u8], start: usize) -> Option<(usize, &[u8])> {
    if bytes.get(start) != Some(&b'$') {
        return None;
    }
    let mut j = start + 1;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    if bytes.get(j) == Some(&b'$') {
        Some((j + 1, &bytes[start..=j]))
    } else {
        None
    }
}

fn split_top_level_statements(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut paren_depth: usize = 0;
    let mut in_single_quote = false;
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_single_quote {
            current.push(ch);
            if ch == '\'' {
                if chars.peek() == Some(&'\'') {
                    if let Some(next_ch) = chars.next() {
                        current.push(next_ch);
                    }
                } else {
                    in_single_quote = false;
                }
            }
            continue;
        }

        match ch {
            '\'' => {
                in_single_quote = true;
                current.push(ch);
            }
            '(' => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' => {
                paren_depth = paren_depth.saturating_sub(1);
                current.push(ch);
            }
            ';' if paren_depth == 0 => {
                if !current.trim().is_empty() {
                    statements.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.trim().is_empty() {
        statements.push(current);
    }
    statements
}

fn normalize_identifier(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('"').trim_matches('`');
    let without_schema = trimmed.rsplit('.').next().unwrap_or(trimmed);
    without_schema
        .trim_matches('"')
        .trim_matches('`')
        .to_ascii_lowercase()
}

fn parse_create_table(stmt: &str) -> Option<(String, BTreeSet<String>)> {
    let open_idx = stmt.find('(')?;
    let close_idx = stmt.rfind(')')?;
    if close_idx <= open_idx {
        return None;
    }

    let header = &stmt[..open_idx];
    let header_tokens: Vec<&str> = header.split_whitespace().collect();
    if header_tokens.len() < 3 {
        return None;
    }

    // Skip `CREATE TABLE` and optional `IF NOT EXISTS`.
    let mut idx = 2;
    if header_tokens.len() >= idx + 3
        && header_tokens[idx].eq_ignore_ascii_case("IF")
        && header_tokens[idx + 1].eq_ignore_ascii_case("NOT")
        && header_tokens[idx + 2].eq_ignore_ascii_case("EXISTS")
    {
        idx += 3;
    }
    let table_name = normalize_identifier(header_tokens.get(idx)?);
    if table_name.is_empty() {
        return None;
    }

    let body = &stmt[open_idx + 1..close_idx];
    let mut columns = BTreeSet::new();
    for item in split_top_level_commas(body) {
        let trimmed = item.trim();
        if trimmed.is_empty() {
            continue;
        }
        let first_token = trimmed
            .split(|c: char| c.is_whitespace() || c == '(')
            .next()
            .unwrap_or("");
        if is_table_constraint_keyword(first_token) {
            continue;
        }
        let col_name = normalize_identifier(first_token);
        if !col_name.is_empty() {
            columns.insert(col_name);
        }
    }

    Some((table_name, columns))
}

fn parse_alter_add_column(stmt: &str) -> Option<(String, String)> {
    let tokens: Vec<&str> = stmt.split_whitespace().collect();
    if tokens.len() < 5 {
        return None;
    }
    let mut idx = 2;
    if tokens.len() >= idx + 2
        && tokens[idx].eq_ignore_ascii_case("IF")
        && tokens[idx + 1].eq_ignore_ascii_case("EXISTS")
    {
        idx += 2;
    }
    let table_name = normalize_identifier(tokens.get(idx)?);
    idx += 1;

    if !tokens.get(idx)?.eq_ignore_ascii_case("ADD") {
        return None;
    }
    idx += 1;
    if tokens
        .get(idx)
        .is_some_and(|t| t.eq_ignore_ascii_case("COLUMN"))
    {
        idx += 1;
    }
    if tokens.len() >= idx + 3
        && tokens[idx].eq_ignore_ascii_case("IF")
        && tokens[idx + 1].eq_ignore_ascii_case("NOT")
        && tokens[idx + 2].eq_ignore_ascii_case("EXISTS")
    {
        idx += 3;
    }

    let col_token = *tokens.get(idx)?;
    if is_table_constraint_keyword(col_token) {
        return None;
    }
    let col_name = normalize_identifier(col_token);
    if col_name.is_empty() {
        return None;
    }
    Some((table_name, col_name))
}

fn split_top_level_commas(body: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth: usize = 0;
    let mut in_single_quote = false;
    let mut start = 0;
    let bytes = body.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if in_single_quote {
            if bytes[i] == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single_quote = false;
            }
            i += 1;
            continue;
        }
        match bytes[i] {
            b'\'' => in_single_quote = true,
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(&body[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }

    if start <= body.len() {
        parts.push(&body[start..]);
    }
    parts
}

fn is_table_constraint_keyword(token: &str) -> bool {
    matches!(
        token.to_ascii_uppercase().as_str(),
        "PRIMARY" | "FOREIGN" | "UNIQUE" | "CHECK" | "CONSTRAINT" | "INDEX" | "KEY" | "EXCLUDE"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::Row;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::path::PathBuf;
    use std::str::FromStr;

    fn crate_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn collect_files_recursive(root: &Path, out: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(root).expect("read crate directory");
        for entry in entries {
            let path = entry.expect("read dir entry").path();
            if path.is_dir() {
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == "target")
                {
                    continue;
                }
                collect_files_recursive(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    #[tokio::test]
    async fn sqlite_migrations_apply_cleanly_are_idempotent_and_enforce_real_constraints() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let db_path = temp_dir.path().join("ohc_storage_test.sqlite");
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
            .expect("parse sqlite url")
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("open real sqlite database");

        // First migration run.
        migrate_sqlite(&pool)
            .await
            .expect("first sqlite migration run should succeed");
        // Second migration run verifies strict idempotency.
        migrate_sqlite(&pool)
            .await
            .expect("re-running sqlite migrations must be idempotent");

        let existing_tables: BTreeSet<String> = sqlx::query(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations'",
        )
        .fetch_all(&pool)
        .await
        .expect("query sqlite_master tables")
        .into_iter()
        .map(|row| row.get::<String, _>("name"))
        .collect();

        let required_tables = [
            "tenants",
            "users",
            "sessions",
            "api_keys",
            "onboarding_state",
            "processed_webhook_events",
            "standing_authority",
            "commit_authority",
            "customers",
            "proposals",
            "proposal_line_items",
            "quotes",
            "invoices",
            "invoice_line_items",
            "invoice_communication_events",
            "bookings",
            "booking_resources",
            "availability_blocks",
            "services",
            "products",
            "product_variants",
            "orders",
            "subscription_plans",
            "subscribers",
            "fulfillment_batches",
            "omni_inbox_messages",
            "agent_feed",
            "agent_feed_items",
            "agent_action_requests",
            "action_required_queue",
            "shared_tasks_v4",
            "agent_session_data",
            "knowledge_embeddings",
            "connection_vault",
            "ohc_provider_connections",
            "usage_ledger",
            "ohc_usage_accounts",
            "ohc_usage_records",
            "ohc_usage_receipts",
            "sites",
            "referrals",
            "pos_terminal_sessions",
            "pos_offline_transactions",
            "task_center_spaces",
            "task_center_tasks",
            "task_center_steps",
            "task_center_artifacts",
            "task_center_approvals",
            "task_center_standing_policies",
            "tenant_workflow_receipts",
            "sms_notification_preferences",
            "sms_verification_challenges",
            "sms_notification_events",
            "sms_notification_dispatches",
            "agent_event_subscriptions",
            "sub_agent_queue",
        ];

        for table in required_tables {
            assert!(
                existing_tables.contains(table),
                "expected core business table `{table}` to exist after SQLite migration"
            );
        }

        // Verify critical Blocker #1 / #5 / #6 columns via PRAGMA table_info.
        for (table, column) in [
            ("onboarding_state", "preparation_receipt"),
            ("proposals", "stripe_checkout_session_id"),
            ("invoices", "stripe_checkout_session_id"),
            ("processed_webhook_events", "event_id"),
            ("usage_ledger", "charged_micros"),
            ("task_center_tasks", "idempotency_key"),
            ("affiliate_ledgers", "commission_amount"),
            ("agent_session_data", "_sync_status"),
        ] {
            let pragma = format!("PRAGMA table_info({table})");
            let cols: BTreeSet<String> = sqlx::query(&pragma)
                .fetch_all(&pool)
                .await
                .expect("query table_info")
                .into_iter()
                .map(|row| row.get::<String, _>("name"))
                .collect();
            assert!(
                cols.contains(column),
                "expected column `{table}.{column}` to exist in migrated SQLite DB"
            );
        }

        // Exercise real transactional writes across core tables, FK constraints,
        // and the `orders_admit_sms` trigger.
        sqlx::query("INSERT INTO tenants (id, name) VALUES ('ws-solo-1', 'Solo Bakery')")
            .execute(&pool)
            .await
            .expect("insert workspace tenant");

        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, tenant_id) \
             VALUES ('user-1', 'owner', 'owner@solo.example', 'hash', 'ws-solo-1')",
        )
        .execute(&pool)
        .await
        .expect("insert local owner user");

        sqlx::query(
            "INSERT INTO onboarding_state (tenant_id, user_id, current_step, state_json, preparation_receipt) \
             VALUES ('ws-solo-1', 'user-1', 3, '{\"ready\":true}', '{\"receipt_id\":\"rcpt-1\"}')",
        )
        .execute(&pool)
        .await
        .expect("insert unified onboarding_state with preparation_receipt");

        sqlx::query(
            "INSERT INTO proposals (id, tenant_id, status, total_amount_cents, required_deposit_cents, stripe_checkout_session_id) \
             VALUES ('prop-1', 'ws-solo-1', 'SENT', 15000, 5000, 'cs_live_test_123')",
        )
        .execute(&pool)
        .await
        .expect("insert proposal with stripe_checkout_session_id");

        sqlx::query(
            "INSERT INTO processed_webhook_events (event_id, provider, event_type, tenant_id) \
             VALUES ('evt_123', 'stripe', 'checkout.session.completed', 'ws-solo-1')",
        )
        .execute(&pool)
        .await
        .expect("insert processed_webhook_events dedup record");

        sqlx::query(
            "INSERT INTO orders (id, tenant_id, total_amount, currency, status) \
             VALUES ('ord-001', 'ws-solo-1', 42.50, 'USD', 'paid')",
        )
        .execute(&pool)
        .await
        .expect("insert order triggering orders_admit_sms");

        let sms_status: String = sqlx::query_scalar(
            "SELECT status FROM sms_notification_events \
             WHERE tenant_id = 'ws-solo-1' AND event_id = 'ord-001' AND event_type = 'new_order'",
        )
        .fetch_one(&pool)
        .await
        .expect("orders_admit_sms trigger should create sms_notification_events record");
        assert_eq!(sms_status, "no_recipients");

        sqlx::query(
            "INSERT INTO ohc_usage_accounts (tenant_id, limit_micros) VALUES ('ws-solo-1', 5000000)",
        )
        .execute(&pool)
        .await
        .expect("insert local usage account");

        sqlx::query(
            "INSERT INTO usage_ledger (id, tenant_id, event_id, provider, model, payer_mode, charged_micros) \
             VALUES ('ul-1', 'ws-solo-1', 'ev-1', 'openai', 'gpt-4o-mini', 'byok_api', 1250)",
        )
        .execute(&pool)
        .await
        .expect("insert usage_ledger record");

        sqlx::query(
            "INSERT INTO task_center_spaces (id, tenant_id, name, created_at_unix, updated_at_unix) \
             VALUES ('sp-1', 'ws-solo-1', 'Operations', 1700000000, 1700000000)",
        )
        .execute(&pool)
        .await
        .expect("insert task_center_spaces record");

        sqlx::query(
            "INSERT INTO task_center_tasks (id, tenant_id, space_id, title, prompt, status, provenance, idempotency_key, created_at_unix, updated_at_unix) \
             VALUES ('tk-1', 'ws-solo-1', 'sp-1', 'Draft invoice', 'Prepare invoice', 'queued', 'draft', 'idem-1', 1700000000, 1700000000)",
        )
        .execute(&pool)
        .await
        .expect("insert task_center_tasks record");

        let fk_violations = sqlx::query("PRAGMA foreign_key_check;")
            .fetch_all(&pool)
            .await
            .expect("PRAGMA foreign_key_check must succeed without schema mismatch");
        assert!(
            fk_violations.is_empty(),
            "expected zero SQLite foreign key violations, got {}",
            fk_violations.len()
        );
    }

    #[test]
    fn sqlite_and_postgres_migrations_have_100_percent_table_and_column_parity() {
        let root = crate_root();
        let sqlite_schema = extract_directory_schema(&root.join("migrations/sqlite"))
            .expect("extract sqlite migration schema");
        let postgres_schema = extract_directory_schema(&root.join("migrations/postgres"))
            .expect("extract postgres migration schema");

        assert!(
            sqlite_schema.len() >= 100,
            "expected comprehensive unified business schema, got {} tables",
            sqlite_schema.len()
        );

        let sqlite_tables: BTreeSet<&String> = sqlite_schema.keys().collect();
        let postgres_tables: BTreeSet<&String> = postgres_schema.keys().collect();

        let only_sqlite: Vec<&&String> = sqlite_tables.difference(&postgres_tables).collect();
        let only_postgres: Vec<&&String> = postgres_tables.difference(&sqlite_tables).collect();
        assert!(
            only_sqlite.is_empty() && only_postgres.is_empty(),
            "table-set drift detected! only_in_sqlite={only_sqlite:?}, only_in_postgres={only_postgres:?}"
        );

        let mut column_drifts = Vec::new();
        for (table, sqlite_cols) in &sqlite_schema {
            let postgres_cols = postgres_schema
                .get(table)
                .expect("table verified present in postgres");
            let missing_in_pg: Vec<&String> = sqlite_cols.difference(postgres_cols).collect();
            let missing_in_sqlite: Vec<&String> = postgres_cols.difference(sqlite_cols).collect();
            if !missing_in_pg.is_empty() || !missing_in_sqlite.is_empty() {
                column_drifts.push(format!(
                    "{table}: missing_in_postgres={missing_in_pg:?}, missing_in_sqlite={missing_in_sqlite:?}"
                ));
            }
        }
        assert!(
            column_drifts.is_empty(),
            "column-set drift detected across SQLite and PostgreSQL migrations:\n{}",
            column_drifts.join("\n")
        );
    }

    #[test]
    fn oss_storage_crate_contains_zero_cloud_or_multitenant_leaks() {
        // Construct forbidden markers programmatically so this test file itself
        // never embeds restricted cloud/multi-tenant tokens verbatim.
        let forbidden_markers = [
            ["row", "level", "security"].join(" "),
            ["create", "policy"].join(" "),
            ["ohc", "bypassrls"].join("_"),
            ["ohc", "collective"].join("_"),
            ["oidc", "providers"].join("_"),
            ["omnisolo", "multitenant"].join("_"),
            ["power", "sync"].join(""),
            ["trial", "extension"].join("_"),
            ["dynamic", "pricing", "rules"].join("_"),
            ["plan", "tier"].join("_"),
            ["tenant", "ai", "budgets"].join("_"),
            ["create", "publication"].join(" "),
        ];

        let mut files = Vec::new();
        collect_files_recursive(&crate_root(), &mut files);
        files.sort();
        assert!(!files.is_empty(), "expected crate files to scan");

        let mut violations = Vec::new();
        for path in files {
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "Cargo.lock")
            {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let lower = content.to_ascii_lowercase();
            for marker in &forbidden_markers {
                if lower.contains(marker) {
                    violations.push(format!("{}: matched `{marker}`", path.display()));
                }
            }
        }

        assert!(
            violations.is_empty(),
            "OSS storage crate leaked restricted cloud/multi-tenant markers:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn schema_extractor_handles_alter_table_comments_and_dollar_blocks() {
        let sample = r#"
            -- comment with CREATE TABLE fake_comment (id TEXT);
            /* block comment CREATE TABLE block_ignored (id TEXT); */
            CREATE TABLE IF NOT EXISTS public.sample_items (
                id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                price NUMERIC(10, 2) NOT NULL DEFAULT 0,
                status TEXT CHECK (status IN ('open', 'closed')),
                CONSTRAINT chk_id CHECK (length(id) > 0)
            );
            CREATE OR REPLACE FUNCTION sample_fn() RETURNS trigger AS $$
            BEGIN
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;
            ALTER TABLE IF EXISTS public.sample_items ADD COLUMN IF NOT EXISTS external_ref TEXT;
        "#;

        let extracted = extract_schema_tables_and_columns(sample);
        assert_eq!(extracted.len(), 1);
        let cols = extracted
            .get("sample_items")
            .expect("sample_items extracted");
        let expected: BTreeSet<String> = ["id", "tenant_id", "price", "status", "external_ref"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(*cols, expected);
    }
}
