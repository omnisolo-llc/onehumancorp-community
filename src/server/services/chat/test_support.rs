//! Real, isolated single-tenant PostgreSQL prerequisites for OSS chat service tests.
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

pub(super) const ADDITIVE_CONTENT_TYPE_VERSION: i64 = 8;

static SCHEMA_DDL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) fn test_database_url(raw: Option<&str>) -> Result<PgConnectOptions, &'static str> {
    const ERROR: &str = "OHC_CHAT_TEST_DATABASE_URL must select an explicit loopback PostgreSQL ohc_*_test database without URL options";
    let raw = raw.ok_or(ERROR)?;
    if raw.chars().any(char::is_control) {
        return Err(ERROR);
    }
    let parsed = url::Url::parse(raw).map_err(|_| ERROR)?;
    let host = parsed
        .host_str()
        .ok_or(ERROR)?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let ip: std::net::IpAddr = host.parse().map_err(|_| ERROR)?;
    let name = parsed.path().strip_prefix('/').ok_or(ERROR)?;
    if !matches!(parsed.scheme(), "postgres" | "postgresql")
        || !ip.is_loopback()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !name.starts_with("ohc_")
        || !name.ends_with("_test")
        || name.len() <= 9
        || name.len() > 63
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(ERROR);
    }
    raw.parse().map_err(|_| ERROR)
}

pub(super) fn fixture_migrator(with_content_type: bool) -> sqlx::migrate::Migrator {
    let mut migrations: Vec<sqlx::migrate::Migration> =
        ::ohc_storage::POSTGRES_MIGRATOR.iter().cloned().collect();
    assert!(
        !migrations.is_empty(),
        "active ohc_storage PostgreSQL migrations must not be empty"
    );
    if with_content_type {
        migrations.push(sqlx::migrate::Migration::new(
            ADDITIVE_CONTENT_TYPE_VERSION,
            std::borrow::Cow::Borrowed("chat_message_content_type"),
            sqlx::migrate::MigrationType::Simple,
            std::borrow::Cow::Borrowed(
                "ALTER TABLE chat_messages ADD COLUMN IF NOT EXISTS content_type VARCHAR(50) NOT NULL DEFAULT 'text';",
            ),
            false,
        ));
    }
    sqlx::migrate::Migrator {
        migrations: std::borrow::Cow::Owned(migrations),
        ..sqlx::migrate::Migrator::DEFAULT
    }
}

pub(super) struct ChatFixture {
    pub(super) pool: PgPool,
    schema: String,
}

impl ChatFixture {
    pub(super) async fn new() -> Self {
        Self::with_content_type(false).await
    }

    pub(super) async fn legacy() -> Self {
        Self::with_content_type(false).await
    }

    async fn with_content_type(with_content_type: bool) -> Self {
        let value = std::env::var("OHC_CHAT_TEST_DATABASE_URL").ok();
        let options = test_database_url(value.as_deref())
            .expect("missing or unsafe disposable chat database prerequisite");
        let migrations = fixture_migrator(with_content_type);
        let schema = format!("chat_case_{}", Uuid::new_v4().simple());
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect_with(options.options([("search_path", schema.as_str())]))
            .await
            .expect("owned chat fixture database must be available");
        let encoding: String = sqlx::query_scalar("SHOW server_encoding")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            encoding, "UTF8",
            "the disposable fixture must preserve Unicode data"
        );
        {
            let _ddl_guard = SCHEMA_DDL_LOCK.lock().await;
            sqlx::query(&format!("CREATE SCHEMA {schema}"))
                .execute(&pool)
                .await
                .unwrap();
            migrations
                .run(&pool)
                .await
                .expect("actual ohc_storage PostgreSQL migrations must execute in SQLx order");
        }
        Self { pool, schema }
    }

    pub(super) async fn count_for_workspace(&self, workspace: Uuid, table: &str, id: Uuid) -> i64 {
        assert!(
            [
                "chat_inboxes",
                "chat_channels",
                "chat_contacts",
                "chat_conversations",
                "chat_messages"
            ]
            .contains(&table)
        );
        sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {table} WHERE id = $1 AND tenant_id = $2"
        ))
        .bind(id.to_string())
        .bind(workspace.to_string())
        .fetch_one(&self.pool)
        .await
        .expect("workspace count query must succeed; SQL errors are not zero rows")
    }

    pub(super) async fn finish(self) {
        {
            let _ddl_guard = SCHEMA_DDL_LOCK.lock().await;
            let tables: Vec<String> =
                sqlx::query_scalar("SELECT tablename FROM pg_tables WHERE schemaname = $1")
                    .bind(&self.schema)
                    .fetch_all(&self.pool)
                    .await
                    .unwrap();
            for chunk in tables.chunks(25) {
                let drop_list = chunk
                    .iter()
                    .map(|table| format!("\"{}\".\"{}\"", self.schema, table))
                    .collect::<Vec<_>>()
                    .join(", ");
                sqlx::query(&format!("DROP TABLE IF EXISTS {drop_list} CASCADE"))
                    .execute(&self.pool)
                    .await
                    .unwrap();
            }
            sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
                .execute(&self.pool)
                .await
                .unwrap();
        }
        self.pool.close().await;
    }
}
