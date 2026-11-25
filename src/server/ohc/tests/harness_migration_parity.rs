//! Logical schema parity across the two supported OSS durable-store dialects.

#[test]
fn sqlite_and_postgres_migrations_preserve_every_canonical_harness_table_and_column() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .expect("workspace root")
        .join("crates/ohc-storage/migrations");
    ohc_storage::assert_sqlite_postgres_schema_parity(
        &manifest_dir.join("sqlite"),
        &manifest_dir.join("postgres"),
    )
    .expect("SQLite and PostgreSQL OSS schemas must remain in parity");
}
