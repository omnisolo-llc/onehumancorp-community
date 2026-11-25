// SPDX-License-Identifier: Apache-2.0
//! Single-tenant Community Edition standalone server entrypoint.

use std::sync::Arc;

use ohc_core::defaults::{
    InProcessLocalTransport, LocalOwnerAuthPolicy, NoOpPlatformBilling, SingleTenantContext,
    SingleTenantRuntime,
};
use server_lib::server_builder::OhcServerBuilder;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    server_lib::utils::fs::cleanup_stale_temp_files();
    let use_json = std::env::var("LOG_FORMAT").unwrap_or_default() == "json";
    let subscriber = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::from_default_env()
            .add_directive(tracing::Level::INFO.into()),
    );
    if use_json {
        subscriber.json().init();
    } else {
        subscriber.init();
    }

    let db_path = server_lib::config::get_safe_user_dir().join("omnisolo-standalone.db");
    if let Some(parent) = db_path.parent() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent);
        }
        #[cfg(not(unix))]
        let _ = std::fs::create_dir_all(parent);
    }
    let sqlite_url = std::env::var("OMNISOLO_DATABASE_URL")
        .ok()
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .filter(|value| value.trim().starts_with("sqlite"))
        .unwrap_or_else(|| format!("sqlite://{}?mode=rwc", db_path.display()));

    let workspace_id = std::env::var("OMNISOLO_WORKSPACE_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default_tenant".to_string());

    let deployment = Arc::new(SingleTenantRuntime::new(sqlite_url)?);
    let tenant_ctx = Arc::new(SingleTenantContext::new(workspace_id.clone())?);
    let auth_policy = Arc::new(LocalOwnerAuthPolicy::new(workspace_id)?);
    let billing_platform = Arc::new(NoOpPlatformBilling);
    let sync_transport = Arc::new(InProcessLocalTransport::default());

    let shutdown = server_lib::runtime_shutdown::CancellationToken::new();
    server_lib::runtime_shutdown::install_signal_handler(shutdown.clone());

    OhcServerBuilder::new(
        deployment,
        tenant_ctx,
        auth_policy,
        billing_platform,
        sync_transport,
    )
    .serve_with_graceful_shutdown(shutdown)
    .await
}
