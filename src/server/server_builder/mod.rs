// SPDX-License-Identifier: Apache-2.0
//! Composable server builder wiring OSS extension seams (`ohc_core::extensions`).

pub mod builder;
pub mod capabilities;
pub mod grpc_router;
pub mod grpc_services;
pub mod http_gateway_helpers;
pub mod http_routes;
pub mod http_subrouters;
pub mod hub_grpc_helpers;
pub mod metrics_handlers;
pub mod ui_caches;
pub mod ui_inbox_ledger_readers;
pub mod ui_ops_handlers;
pub mod ui_triage_feed_readers;
pub mod ui_triage_handlers;
pub mod ui_unified_feed_handlers;
pub mod workers;

pub use builder::{
    OhcServerBuilder, ServerContext, auth_policy, billing_platform, distributed_cache,
    install_runtime_extensions, sync_transport, tenant_context,
};
