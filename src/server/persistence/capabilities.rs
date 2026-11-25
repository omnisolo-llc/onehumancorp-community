#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum DatabaseBackend {
    Postgres,
    Sqlite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatabaseCapabilities {
    pub transactions: bool,
    pub advisory_locks: bool,
    pub row_level_security: bool,
    pub pg_vector: bool,
}

impl DatabaseCapabilities {
    pub fn for_backend(backend: DatabaseBackend) -> Self {
        match backend {
            DatabaseBackend::Postgres => Self {
                transactions: true,
                advisory_locks: true,
                row_level_security: true,
                pg_vector: true,
            },
            DatabaseBackend::Sqlite => Self {
                transactions: true,
                advisory_locks: false,
                row_level_security: false,
                pg_vector: false,
            },
        }
    }
}
