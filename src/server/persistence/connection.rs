use ohc_core::extensions::StorageHandle;
use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection};

use super::capabilities::{DatabaseBackend, DatabaseCapabilities};

#[derive(Clone)]
pub struct AppDatabase {
    connection: DatabaseConnection,
    backend: DatabaseBackend,
    storage_handle: Option<StorageHandle>,
}

#[derive(Clone)]
pub struct DatabaseUrl {
    raw: String,
}

impl DatabaseUrl {
    pub fn new(raw: impl Into<String>) -> Self {
        Self { raw: raw.into() }
    }

    pub fn expose(&self) -> &str {
        &self.raw
    }

    pub(crate) fn expose_for_connection(&self) -> &str {
        &self.raw
    }
}

impl std::fmt::Debug for DatabaseUrl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("DatabaseUrl(REDACTED)")
    }
}

impl AppDatabase {
    pub async fn connect(url: &str) -> Result<Self, sea_orm::DbErr> {
        Self::connect_with_optional_sqlcipher_key(url, None).await
    }

    pub async fn connect_with_sqlcipher_key(
        url: &str,
        sqlite_encryption_key: &str,
    ) -> Result<Self, sea_orm::DbErr> {
        Self::connect_with_optional_sqlcipher_key(url, Some(sqlite_encryption_key)).await
    }

    async fn connect_with_optional_sqlcipher_key(
        url: &str,
        sqlite_encryption_key: Option<&str>,
    ) -> Result<Self, sea_orm::DbErr> {
        if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            let pg_pool = crate::db::secure_pg_pool_options()
                .connect(url)
                .await
                .map_err(|error| sea_orm::DbErr::Custom(error.to_string()))?;
            return Ok(Self::from_storage_handle(&StorageHandle::Postgres(pg_pool)));
        }
        if url.starts_with("sqlite:") && sqlite_encryption_key.is_some() {
            let sqlite_pool = crate::db::connect_sqlite_pool(url, sqlite_encryption_key)
                .await
                .map_err(|error| sea_orm::DbErr::Custom(error.to_string()))?;
            let database = Self::from_storage_handle(&StorageHandle::Sqlite(sqlite_pool));
            if sqlite_encryption_key.is_some()
                && let Err(error) = require_sqlite_encryption(&database.connection).await
            {
                let _ = database.connection.close().await;
                return Err(error);
            }
            return Ok(database);
        }
        let mut options = ConnectOptions::new(url.to_owned());
        options.sqlx_logging(false);
        let connection = Database::connect(options).await?;
        let storage_handle = match connection.get_database_backend() {
            sea_orm::DatabaseBackend::Sqlite => Some(StorageHandle::Sqlite(
                connection.get_sqlite_connection_pool().clone(),
            )),
            sea_orm::DatabaseBackend::Postgres => Some(StorageHandle::Postgres(
                connection.get_postgres_connection_pool().clone(),
            )),
            sea_orm::DatabaseBackend::MySql => {
                let _ = connection.close().await;
                return Err(sea_orm::DbErr::Custom(
                    "MySQL backend is not supported in Community Edition; use SQLite or PostgreSQL"
                        .to_string(),
                ));
            }
        };
        let backend = Self::detect_backend(&connection)?;
        Ok(Self {
            connection,
            backend,
            storage_handle,
        })
    }

    /// Adopt a unified [`StorageHandle`] so SeaORM shares the runtime's hardened
    /// connection pool, encryption settings, and lifecycle hooks.
    pub fn from_storage_handle(handle: &StorageHandle) -> Self {
        match handle {
            StorageHandle::Postgres(pool) => Self {
                connection: sea_orm::SqlxPostgresConnector::from_sqlx_postgres_pool(pool.clone()),
                backend: DatabaseBackend::Postgres,
                storage_handle: Some(handle.clone()),
            },
            StorageHandle::Sqlite(pool) => Self {
                connection: sea_orm::SqlxSqliteConnector::from_sqlx_sqlite_pool(pool.clone()),
                backend: DatabaseBackend::Sqlite,
                storage_handle: Some(handle.clone()),
            },
        }
    }

    pub fn from_connection(connection: DatabaseConnection) -> Self {
        let storage_handle = match connection.get_database_backend() {
            sea_orm::DatabaseBackend::Sqlite => Some(StorageHandle::Sqlite(
                connection.get_sqlite_connection_pool().clone(),
            )),
            sea_orm::DatabaseBackend::Postgres => Some(StorageHandle::Postgres(
                connection.get_postgres_connection_pool().clone(),
            )),
            sea_orm::DatabaseBackend::MySql => None,
        };
        let backend = Self::detect_backend(&connection).unwrap_or(DatabaseBackend::Sqlite);
        Self {
            connection,
            backend,
            storage_handle,
        }
    }

    fn detect_backend(connection: &DatabaseConnection) -> Result<DatabaseBackend, sea_orm::DbErr> {
        match connection.get_database_backend() {
            sea_orm::DatabaseBackend::Sqlite => Ok(DatabaseBackend::Sqlite),
            sea_orm::DatabaseBackend::Postgres => Ok(DatabaseBackend::Postgres),
            sea_orm::DatabaseBackend::MySql => Err(sea_orm::DbErr::Custom(
                "MySQL backend is not supported in Community Edition".to_string(),
            )),
        }
    }

    pub fn backend(&self) -> DatabaseBackend {
        self.backend
    }

    pub fn capabilities(&self) -> DatabaseCapabilities {
        DatabaseCapabilities::for_backend(self.backend)
    }

    pub fn connection(&self) -> &DatabaseConnection {
        &self.connection
    }

    pub fn storage_handle(&self) -> Option<&StorageHandle> {
        self.storage_handle.as_ref()
    }
}

pub(crate) async fn require_sqlite_encryption(
    connection: &DatabaseConnection,
) -> Result<(), sea_orm::DbErr> {
    if connection.get_database_backend() != sea_orm::DatabaseBackend::Sqlite {
        return Ok(());
    }
    let encrypted = connection
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "PRAGMA cipher_version".to_owned(),
        ))
        .await?
        .and_then(|row| row.try_get_by_index::<String>(0).ok())
        .is_some_and(|version| !version.trim().is_empty());
    if !encrypted {
        return Err(sea_orm::DbErr::Custom(
            "SQLite storage must use SQLCipher encryption".to_owned(),
        ));
    }
    Ok(())
}
