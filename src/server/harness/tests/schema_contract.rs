use std::fs;
use std::path::Path;

fn collect_migrations(dir: &Path) -> String {
    let mut paths: Vec<_> = fs::read_dir(dir)
        .expect("migration directory must exist")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("sql"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| fs::read_to_string(path).expect("migration SQL must be readable"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn harness_middleware_oss_migrations_cover_all_runtime_tables() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let migrations_root = manifest_dir.join("../../../crates/ohc-storage/migrations");
    let postgres_schema = collect_migrations(&migrations_root.join("postgres"));
    let sqlite_schema = collect_migrations(&migrations_root.join("sqlite"));

    for table in [
        "harness_sessions",
        "harness_tasks",
        "harness_turns",
        "harness_attempts",
        "harness_bindings",
        "harness_leases",
        "harness_events",
        "harness_event_parents",
        "harness_event_branch_heads",
        "harness_artifacts",
        "harness_workspace_snapshots",
        "harness_native_records",
        "harness_command_inbox",
        "harness_command_outbox",
        "harness_model_runtimes",
        "harness_model_runtime_workers",
        "harness_inference_admissions",
        "harness_handoff_operations",
        "harness_capsules",
        "harness_capsule_loss_entries",
    ] {
        let marker = format!("CREATE TABLE IF NOT EXISTS {table}");
        assert!(
            postgres_schema.contains(&marker),
            "PostgreSQL OSS migrations missing {table}"
        );
        assert!(
            sqlite_schema.contains(&marker),
            "SQLite OSS migrations missing {table}"
        );
    }
}
