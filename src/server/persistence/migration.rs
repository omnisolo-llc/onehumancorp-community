use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ConnectionTrait, DatabaseTransaction, EntityTrait, QueryOrder, Set,
    Statement, TransactionTrait,
};

use super::{capabilities::DatabaseBackend as AppBackend, connection::AppDatabase};
use server_auth::seaorm_store::entities as auth_entities;

pub const CORE_SCHEMA_VERSION: &str = "20260730_000001_portable_core";
pub const AUTH_SCHEMA_VERSION: &str = "20260730_000002_auth_registration";
pub const PORTABLE_ROLE_SCHEMA_VERSION: &str = "20260801_000003_portable_user_roles";
const POSTGRES_MIGRATION_LOCK_KEY: i64 = 0x4f48_435f_4d49_4752;

pub async fn migrate(database: &AppDatabase) -> Result<(), sea_orm::DbErr> {
    let connection = database.connection();
    if database.backend() == AppBackend::Postgres {
        let pool = match database.storage_handle() {
            Some(ohc_core::extensions::StorageHandle::Postgres(pool)) => pool.clone(),
            _ => connection.get_postgres_connection_pool().clone(),
        };
        ohc_storage::migrate_postgres(&pool)
            .await
            .map_err(|error| sea_orm::DbErr::Custom(error.to_string()))?;
        let migration_guard = acquire_postgres_migration_guard(connection).await?;
        run_post_storage_migrations(&migration_guard).await?;
        migration_guard.commit().await?;
        return Ok(());
    }

    prepare_legacy_sqlite_users_table(connection).await?;
    let pool = match database.storage_handle() {
        Some(ohc_core::extensions::StorageHandle::Sqlite(pool)) => pool.clone(),
        _ => connection.get_sqlite_connection_pool().clone(),
    };
    ohc_storage::migrate_sqlite(&pool)
        .await
        .map_err(|error| sea_orm::DbErr::Custom(error.to_string()))?;
    repair_legacy_sqlite_sms_schema(connection).await?;
    run_post_storage_migrations(connection).await
}

async fn run_post_storage_migrations<C>(connection: &C) -> Result<(), sea_orm::DbErr>
where
    C: ConnectionTrait + TransactionTrait,
{
    backfill_portable_user_roles(connection).await?;
    backfill_identity_email_claims(connection).await?;
    configure_agent_definition_authority(connection).await?;

    insert_default_or_ignore(
        connection,
        "onehumancorp_schema_versions",
        &["id", "applied_at"],
        vec![CORE_SCHEMA_VERSION.to_owned().into(), Utc::now().into()],
    )
    .await?;

    insert_default_or_ignore(
        connection,
        "application_settings",
        &["key", "value", "updated_at", "updated_by"],
        vec![
            "registration_mode".to_owned().into(),
            "closed".to_owned().into(),
            Utc::now().into(),
            sea_orm::Value::String(None),
        ],
    )
    .await?;

    insert_default_or_ignore(
        connection,
        "onehumancorp_schema_versions",
        &["id", "applied_at"],
        vec![AUTH_SCHEMA_VERSION.to_owned().into(), Utc::now().into()],
    )
    .await?;

    insert_default_or_ignore(
        connection,
        "onehumancorp_schema_versions",
        &["id", "applied_at"],
        vec![
            PORTABLE_ROLE_SCHEMA_VERSION.to_owned().into(),
            Utc::now().into(),
        ],
    )
    .await?;
    Ok(())
}

/// Ensures pre-storage SQLite `users` snapshots carry the columns and composite index
/// required before `ohc_storage::SQLITE_MIGRATOR` runs `0001_core_workspace_and_identity.sql`.
pub async fn prepare_legacy_sqlite_users_table<C: ConnectionTrait>(
    connection: &C,
) -> Result<(), sea_orm::DbErr> {
    if connection.get_database_backend() != sea_orm::DatabaseBackend::Sqlite {
        return Ok(());
    }
    let columns = connection
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "PRAGMA table_info(users)".to_string(),
        ))
        .await?;
    if columns.is_empty() {
        return Ok(());
    }
    for (name, ddl) in [
        (
            "marketplace_authority_key",
            "ALTER TABLE users ADD COLUMN marketplace_authority_key TEXT",
        ),
        (
            "marketplace_eligible",
            "ALTER TABLE users ADD COLUMN marketplace_eligible BOOLEAN NOT NULL DEFAULT 0",
        ),
    ] {
        if !columns.iter().any(|row| {
            row.try_get::<String>("", "name")
                .is_ok_and(|value| value == name)
        }) {
            connection.execute_unprepared(ddl).await?;
        }
    }
    connection
        .execute_unprepared(
            "CREATE UNIQUE INDEX IF NOT EXISTS ux_users_id_tenant ON users(id, tenant_id)",
        )
        .await?;
    Ok(())
}

/// Repairs legacy SQLite `sms_notification_events` tables whose durable backoff
/// column or pending-outbox index was missing before `ohc_storage` versioning.
async fn repair_legacy_sqlite_sms_schema<C: ConnectionTrait>(
    connection: &C,
) -> Result<(), sea_orm::DbErr> {
    if connection.get_database_backend() != sea_orm::DatabaseBackend::Sqlite {
        return Ok(());
    }
    let columns = connection
        .query_all(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "PRAGMA table_info(sms_notification_events)".to_string(),
        ))
        .await?;
    if columns.is_empty() {
        return Ok(());
    }
    if !columns.iter().any(|row| {
        row.try_get::<String>("", "name")
            .is_ok_and(|value| value == "next_attempt_at")
    }) {
        connection
            .execute_unprepared(
                "ALTER TABLE sms_notification_events ADD COLUMN next_attempt_at BIGINT NOT NULL DEFAULT 0",
            )
            .await?;
    }
    connection
        .execute_unprepared(
            "CREATE INDEX IF NOT EXISTS sms_order_outbox_pending ON sms_notification_events(event_type,status,next_attempt_at,created_at,tenant_id,event_id)",
        )
        .await?;
    Ok(())
}

/// Private identity-derived public eligibility. This runs inside the existing
/// single-tenant migration transaction before any application routes are built.
async fn configure_agent_definition_authority<C>(connection: &C) -> Result<(), sea_orm::DbErr>
where
    C: ConnectionTrait + TransactionTrait,
{
    let backend = connection.get_database_backend();
    let transaction = connection.begin().await?;
    if backend == sea_orm::DatabaseBackend::Postgres {
        let sql = include_str!("agent_definition_authority_pg.sql");
        let (schema, rest) = sql
            .split_once("-- AUTHORITY_SCHEMA_END")
            .ok_or_else(|| sea_orm::DbErr::Custom("authority schema boundary missing".into()))?;
        let (backfill, triggers) = rest
            .split_once("-- AUTHORITY_BACKFILL_END")
            .ok_or_else(|| sea_orm::DbErr::Custom("authority backfill boundary missing".into()))?;
        transaction.execute_unprepared(schema).await?;
        transaction.execute_unprepared(backfill).await?;
        transaction.execute_unprepared(triggers).await?;
    } else {
        let enabled = transaction
            .query_one(Statement::from_string(
                backend,
                "PRAGMA foreign_keys".to_string(),
            ))
            .await?
            .ok_or_else(|| sea_orm::DbErr::Custom("SQLite foreign key state unavailable".into()))?
            .try_get::<i64>("", "foreign_keys")?;
        if enabled != 1 {
            return Err(sea_orm::DbErr::Custom(
                "SQLite foreign keys must be enabled".into(),
            ));
        }
        let columns = transaction
            .query_all(Statement::from_string(
                backend,
                "PRAGMA table_info(users)".to_string(),
            ))
            .await?;
        for (name, ddl) in [
            (
                "marketplace_authority_key",
                "ALTER TABLE users ADD COLUMN marketplace_authority_key TEXT",
            ),
            (
                "marketplace_eligible",
                "ALTER TABLE users ADD COLUMN marketplace_eligible BOOLEAN NOT NULL DEFAULT 0",
            ),
        ] {
            if !columns.iter().any(|row| {
                row.try_get::<String>("", "name")
                    .is_ok_and(|value| value == name)
            }) {
                transaction.execute_unprepared(ddl).await?;
            }
        }
        transaction
            .execute_unprepared(include_str!("agent_definition_authority_sqlite.sql"))
            .await?;
    }
    transaction.commit().await
}

async fn insert_default_or_ignore<C>(
    connection: &C,
    table: &str,
    columns: &[&str],
    values: Vec<sea_orm::Value>,
) -> Result<(), sea_orm::DbErr>
where
    C: ConnectionTrait,
{
    let backend = connection.get_database_backend();
    let placeholders: Vec<String> = match backend {
        sea_orm::DatabaseBackend::Postgres => (1..=values.len())
            .map(|index| format!("${index}"))
            .collect(),
        _ => vec!["?".to_string(); values.len()],
    };
    let keyword = match backend {
        sea_orm::DatabaseBackend::Sqlite => "INSERT OR IGNORE",
        _ => "INSERT",
    };
    let mut sql = format!(
        "{keyword} INTO \"{table}\" ({}) VALUES ({})",
        columns
            .iter()
            .map(|column| format!("\"{column}\""))
            .collect::<Vec<_>>()
            .join(", "),
        placeholders.join(", ")
    );
    if backend == sea_orm::DatabaseBackend::Postgres {
        sql.push_str(" ON CONFLICT DO NOTHING");
    }
    connection
        .execute(Statement::from_sql_and_values(backend, sql, values))
        .await?;
    Ok(())
}

async fn acquire_postgres_migration_guard(
    connection: &sea_orm::DatabaseConnection,
) -> Result<DatabaseTransaction, sea_orm::DbErr> {
    let transaction = connection.begin().await?;
    transaction
        .execute(Statement::from_sql_and_values(
            sea_orm::DatabaseBackend::Postgres,
            "SELECT pg_advisory_xact_lock($1)",
            [POSTGRES_MIGRATION_LOCK_KEY.into()],
        ))
        .await?;
    Ok(transaction)
}

async fn backfill_portable_user_roles<C>(connection: &C) -> Result<(), sea_orm::DbErr>
where
    C: ConnectionTrait + TransactionTrait,
{
    let backend = connection.get_database_backend();
    let transaction = connection.begin().await?;
    let marker_sql = match backend {
        sea_orm::DatabaseBackend::Postgres => {
            "INSERT INTO onehumancorp_schema_versions(id,applied_at) VALUES($1,$2) ON CONFLICT(id) DO NOTHING"
        }
        _ => "INSERT OR IGNORE INTO onehumancorp_schema_versions(id,applied_at) VALUES(?,?)",
    };
    let claimed = transaction
        .execute(Statement::from_sql_and_values(
            backend,
            marker_sql,
            [
                PORTABLE_ROLE_SCHEMA_VERSION.to_owned().into(),
                Utc::now().into(),
            ],
        ))
        .await?;
    if claimed.rows_affected() == 0 {
        return transaction.commit().await;
    }
    let sql = match backend {
        sea_orm::DatabaseBackend::Postgres => {
            "INSERT INTO identity_user_roles (user_id, role_name, tenant_id, position) SELECT users.id, role_name, users.tenant_id, position::INTEGER - 1 FROM users CROSS JOIN LATERAL unnest(COALESCE(users.roles, ARRAY[]::TEXT[])) WITH ORDINALITY AS legacy_roles(role_name, position) ON CONFLICT (user_id, role_name) DO NOTHING"
        }
        _ => {
            "INSERT OR IGNORE INTO identity_user_roles (user_id, role_name, tenant_id, position) SELECT users.id, CAST(legacy_roles.value AS TEXT), users.tenant_id, CAST(legacy_roles.key AS INTEGER) FROM users CROSS JOIN json_each(COALESCE(users.roles, '[]')) AS legacy_roles"
        }
    };
    transaction
        .execute(Statement::from_string(backend, sql.to_string()))
        .await?;
    transaction.commit().await
}

async fn backfill_identity_email_claims<C>(connection: &C) -> Result<(), sea_orm::DbErr>
where
    C: ConnectionTrait + TransactionTrait,
{
    let transaction = connection.begin().await?;
    let users = auth_entities::user::Entity::find()
        .order_by_asc(auth_entities::user::Column::Id)
        .all(&transaction)
        .await?;
    let mut expected = Vec::with_capacity(users.len());
    for user in users {
        let normalized_email =
            server_auth::validation::normalize_email(&user.email).map_err(|_| {
                sea_orm::DbErr::Custom(format!(
                    "existing identity has an invalid email: user {}",
                    user.id
                ))
            })?;
        expected.push((normalized_email, user.id, user.created_at));
    }
    expected.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
    for pair in expected.windows(2) {
        if pair[0].0 == pair[1].0 && pair[0].1 != pair[1].1 {
            return Err(identity_email_collision(&pair[0].0));
        }
    }

    let existing = auth_entities::identity_email_claim::Entity::find()
        .all(&transaction)
        .await?;
    let existing = existing
        .into_iter()
        .map(|claim| (claim.normalized_email, claim.user_id))
        .collect::<std::collections::HashMap<_, _>>();
    for (normalized_email, user_id, claimed_at) in expected {
        if let Some(owner) = existing.get(&normalized_email) {
            if owner != &user_id {
                return Err(identity_email_collision(&normalized_email));
            }
            continue;
        }
        auth_entities::identity_email_claim::ActiveModel {
            normalized_email: Set(normalized_email),
            user_id: Set(user_id),
            claimed_at: Set(claimed_at),
        }
        .insert(&transaction)
        .await?;
    }
    transaction.commit().await
}

fn identity_email_collision(normalized_email: &str) -> sea_orm::DbErr {
    sea_orm::DbErr::Custom(format!(
        "identity email collision for normalized email {normalized_email}"
    ))
}

#[cfg(test)]
mod registration_source_tests {
    use super::AppDatabase;
    use chrono::Utc;
    use sea_orm::{ConnectionTrait, DatabaseBackend, EntityTrait, Statement};
    use server_auth::seaorm_store::SeaOrmAuthRepository;
    use server_auth::seaorm_store::entities as auth_entities;

    async fn execute(db: &AppDatabase, sql: &str) {
        db.connection().execute_unprepared(sql).await.unwrap();
    }

    fn user() -> server_auth::User {
        let now = Utc::now();
        server_auth::User {
            id: "new-source-owner".into(),
            username: "new-source-owner".into(),
            email: "new-source-owner@example.test".into(),
            password_hash: "test-owned-unused".into(),
            active: true,
            roles: vec![server_auth::ROLE_ADMIN.into()],
            organization_id: Some("new-source-tenant".into()),
            created_at: now,
            updated_at: now,
        }
    }

    async fn fresh() -> AppDatabase {
        let db = AppDatabase::connect("sqlite::memory:").await.unwrap();
        super::migrate(&db).await.unwrap();
        execute(
            &db,
            "UPDATE application_settings SET value='open' WHERE key='registration_mode'",
        )
        .await;
        execute(&db, "INSERT INTO registration_tickets(id,email,token_hash,issued_at,expires_at,consumed_at,invitation_id) VALUES('new-ticket','new-source-owner@example.test','ticket-hash',CURRENT_TIMESTAMP,datetime('now','+1 day'),NULL,NULL)").await;
        db
    }

    async fn receipt(db: &AppDatabase, id: &str) -> (Option<String>, Option<String>) {
        let row = db.connection().query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT consumed_by_user_id, consumed_by_tenant_id FROM registration_tickets WHERE id=?", [id.into()]))
            .await.expect("actual migration must install immutable source identity columns").unwrap();
        (
            row.try_get("", "consumed_by_user_id").unwrap(),
            row.try_get("", "consumed_by_tenant_id").unwrap(),
        )
    }

    #[tokio::test]
    async fn unified_storage_migration_installs_single_tenant_oss_schema_idempotently() {
        let db = AppDatabase::connect("sqlite::memory:").await.unwrap();
        super::migrate(&db).await.unwrap();
        super::migrate(&db).await.unwrap();
        execute(
            &db,
            "INSERT INTO tenants(id,name) VALUES('schema-defaults','Schema defaults')",
        )
        .await;
        let row = db
            .connection()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT owner_id, default_currency, version FROM tenants WHERE id='schema-defaults'".to_owned(),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<Option<String>>("", "owner_id").unwrap(), None);
        assert_eq!(
            row.try_get::<String>("", "default_currency").unwrap(),
            "USD"
        );
        assert_eq!(row.try_get::<i64>("", "version").unwrap(), 1);
    }

    #[tokio::test]
    async fn upgrading_old_tickets_preserves_history_without_inventing_source_owners() {
        let db = AppDatabase::connect("sqlite::memory:").await.unwrap();
        execute(&db, "CREATE TABLE registration_tickets(id TEXT PRIMARY KEY,email TEXT NOT NULL,token_hash TEXT NOT NULL,issued_at TEXT NOT NULL,expires_at TEXT NOT NULL,consumed_at TEXT,invitation_id TEXT,consumed_by_user_id TEXT,consumed_by_tenant_id TEXT)").await;
        execute(&db, "INSERT INTO registration_tickets VALUES('old-consumed','old@example.test','old-hash','2026-01-01','2026-01-02','2026-01-01',NULL,NULL,NULL)").await;
        super::migrate(&db).await.unwrap();
        assert_eq!(receipt(&db, "old-consumed").await, (None, None));
        super::migrate(&db).await.unwrap();
        assert_eq!(receipt(&db, "old-consumed").await, (None, None));
        let row = db
            .connection()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT email, consumed_at FROM registration_tickets WHERE id='old-consumed'"
                    .to_string(),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.try_get::<String>("", "email").unwrap(),
            "old@example.test"
        );
        assert_eq!(
            row.try_get::<String>("", "consumed_at").unwrap(),
            "2026-01-01"
        );
    }

    #[tokio::test]
    async fn migrated_registration_binds_the_source_to_the_actual_created_user_and_tenant() {
        let db = fresh().await;
        let repository = SeaOrmAuthRepository::new(db.connection().clone());
        let created = repository
            .consume_ticket_and_create_user("ticket-hash", Utc::now(), user())
            .await
            .unwrap();
        let tenant =
            auth_entities::tenant::Entity::find_by_id(created.organization_id.clone().unwrap())
                .one(db.connection())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(tenant.name, created.username);
        assert_eq!(
            receipt(&db, "new-ticket").await,
            (Some(created.id), created.organization_id)
        );
    }

    #[tokio::test]
    async fn source_receipt_failure_rolls_back_user_roles_email_claim_and_ticket_consumption() {
        let db = fresh().await;
        execute(&db, "CREATE TRIGGER deny_source_receipt BEFORE UPDATE OF consumed_by_user_id ON registration_tickets BEGIN SELECT RAISE(ABORT,'owned fixture denies source receipt'); END").await;
        let repository = SeaOrmAuthRepository::new(db.connection().clone());
        assert!(
            repository
                .consume_ticket_and_create_user("ticket-hash", Utc::now(), user())
                .await
                .is_err(),
            "source identity must be part of the actual registration transaction"
        );
        for table in [
            "tenants",
            "users",
            "identity_user_roles",
            "identity_email_claims",
        ] {
            let row = db
                .connection()
                .query_one(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    format!("SELECT COUNT(*) AS count FROM {table}"),
                ))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                row.try_get::<i64>("", "count").unwrap(),
                0,
                "{table} must roll back"
            );
        }
        let row = db
            .connection()
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT consumed_at FROM registration_tickets WHERE id='new-ticket'".to_string(),
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.try_get::<Option<String>>("", "consumed_at").unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn postgres_migration_runs_portable_role_backfill_and_authority_triggers_cleanly() {
        let url = std::env::var("OHC_STORAGE_TEST_DATABASE_URL")
            .ok()
            .filter(|v| v.starts_with("postgres://") || v.starts_with("postgresql://"))
            .or_else(|| {
                std::env::var("OMNISOLO_POSTGRES_ADMIN_URL")
                    .ok()
                    .filter(|v| v.starts_with("postgres://") || v.starts_with("postgresql://"))
            });
        let require_pg = std::env::var("OMNISOLO_REQUIRE_POSTGRES_TESTS").ok();
        let Some(database_url) = url else {
            assert!(
                require_pg.as_deref() != Some("1"),
                "OMNISOLO_REQUIRE_POSTGRES_TESTS=1 requires a PostgreSQL URL"
            );
            return;
        };

        let db = AppDatabase::connect(&database_url)
            .await
            .expect("connect AppDatabase to PostgreSQL");
        super::migrate(&db)
            .await
            .expect("first AppDatabase::migrate on PostgreSQL must succeed");
        super::migrate(&db)
            .await
            .expect("second AppDatabase::migrate on PostgreSQL must be idempotent");

        // Verify backfill_portable_user_roles executes cleanly against a real seeded user
        // with TEXT[] roles inside a rolled-back transaction.
        let tx = sea_orm::TransactionTrait::begin(db.connection())
            .await
            .expect("begin postgres verification tx");
        tx.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "INSERT INTO tenants (id, name, enabled_currencies) VALUES ('mig-pg-ws', 'PG Migration Test', '[\"USD\"]'::jsonb) ON CONFLICT (id) DO NOTHING".to_string(),
        ))
        .await
        .expect("insert tenant");
        tx.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "INSERT INTO users (id, username, email, password_hash, roles, active, tenant_id) VALUES ('mig-pg-user', 'migpg@example.test', 'migpg@example.test', 'hash', ARRAY['ADMIN', 'OPERATOR']::TEXT[], TRUE, 'mig-pg-ws') ON CONFLICT (id) DO NOTHING".to_string(),
        ))
        .await
        .expect("insert user with TEXT[] roles");
        tx.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            format!(
                "DELETE FROM onehumancorp_schema_versions WHERE id = '{}'",
                super::PORTABLE_ROLE_SCHEMA_VERSION
            ),
        ))
        .await
        .expect("reset portable role marker inside rollback tx");
        super::backfill_portable_user_roles(&tx)
            .await
            .expect("backfill_portable_user_roles must succeed on PostgreSQL without 42804");

        let rows = tx
            .query_all(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT role_name, position FROM identity_user_roles WHERE user_id = 'mig-pg-user' ORDER BY position ASC".to_string(),
            ))
            .await
            .expect("read backfilled roles");
        let roles: Vec<(String, i32)> = rows
            .into_iter()
            .map(|r| {
                (
                    r.try_get::<String>("", "role_name").unwrap(),
                    r.try_get::<i32>("", "position").unwrap(),
                )
            })
            .collect();
        assert_eq!(
            roles,
            vec![("ADMIN".to_string(), 0), ("OPERATOR".to_string(), 1)]
        );
        tx.rollback().await.expect("rollback postgres test tx");
    }
}
