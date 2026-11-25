pub mod autodream;
pub mod oauth;
pub mod offline_sync;
pub mod omnichannel_webhook;
pub mod pos;
pub mod staff_mesh;
pub mod staff_timecards;
pub mod terminal_api;
pub(crate) mod terminal_offline_authority;
pub mod twilio_voice;
pub mod twilio_webhook;

pub mod agents;
pub mod billing_webhook;
pub mod dynamic_workflows;
pub mod growth;
pub mod growth_previews;
pub mod health;
pub mod merchant_checkout;
pub mod merchant_webhooks;
pub mod onboarding;
pub mod usage_api;
pub mod workspace_views;

pub mod agent_feed;
pub mod audio_command;
pub mod booking;
pub mod cart;
pub mod catalog;
pub mod docs;
pub mod fulfillment;
pub mod incidents;
pub mod invoice;
pub mod legacy_triage;
pub mod local_seo;
pub mod mcp_webhook;
pub mod meta_webhook;
pub mod perplexity;
pub mod recovery;
pub mod search;
pub mod settings;
pub mod shipping;
pub mod subscription;
pub mod ui_orders;

pub mod assistant;
pub mod inbox;
pub mod inquiries;
pub mod integrations_settings;
pub mod payment_ledger;
pub mod quotes;

pub mod agent_metrics;
pub mod agent_orchestrate;
pub mod agent_stream;
pub mod checkout_api;
pub mod crm;
pub mod field_ops;
pub mod field_service_routing;
pub mod omnisolo_job_queue;
pub mod proposals;
pub mod realtime;
pub mod storefront_delivery;
pub mod tool_integrations;
pub mod unified_inbox_webhook;
pub mod unified_ws;
pub(crate) mod walkup;
pub mod work_triage;
pub mod ws_batch;
pub mod ws_compression;

pub(crate) mod sync_transaction;
pub mod telemetry_settings;

pub mod stripe_webhook_security;

pub mod voice_provisioning;
pub mod widget;

pub mod production_readiness;

pub(crate) mod fixture_boundary;

#[cfg(test)]
mod e2e_backend_regression_test;
#[cfg(test)]
mod final29_backend_regression_test;
#[cfg(test)]
mod last7_backend_regression_test;
#[cfg(test)]
mod staff_timecards_test;

pub mod sms_settings;
pub mod task_center;
