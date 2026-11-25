#[test]
fn harness_middleware_schema_contains_canonical_records_and_fences() {
    let migration = include_str!(
        "../../../crates/ohc-storage/migrations/postgres/0007_oss_complete_business_and_harness_schema.sql"
    );
    for required in [
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
        "tenant_id",
        "source_attempt_id",
        "ingest_attempt_id",
        "durable_sequence",
        "delivery_sequence",
        "fencing_token",
    ] {
        assert!(
            migration.contains(required),
            "migration is missing required contract marker: {required}"
        );
    }
    assert!(
        !migration.contains("ROW LEVEL SECURITY"),
        "OSS storage migrations must remain free of multi-tenant RLS"
    );
}
