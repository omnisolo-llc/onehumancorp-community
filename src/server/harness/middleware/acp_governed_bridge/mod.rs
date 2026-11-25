//! Thin Agent Client Protocol (ACP v1) governed execution layer.
//!
//! Wraps external ACP-compliant agent harnesses (`codex` via ACP adapter,
//! `opencode`, `goose`, or custom ACP binaries) inside OneHumanCorp's
//! environment sanitization, MCP tool bridge injection, standing-authority
//! permission interception, and atomic micro-USD `UsageLedger` budget checks.

mod classifier;
mod policy;
mod registry;
mod session;
mod transport;

#[cfg(test)]
mod bridge_tests;

pub use classifier::canonical_payload_sha256;
pub use policy::{
    AcpActionRisk, AcpApprovalTicket, AcpBudgetGuard, AcpPermissionDecision, AcpPermissionOutcome,
    AcpStandingPolicy, evaluate_permission_request,
};
pub use registry::{
    AcpExternalHarnessSpec, AcpHarnessRegistry, AcpMcpBridgeCommand, AcpSandboxTier,
    AcpSanitizedEnvInput, SURROGATE_TOKEN_PREFIX, build_sanitized_env,
    validate_opaque_surrogate_token,
};
pub use session::{AcpCancelHandle, AcpGovernedSession, AcpSessionLaunchConfig, AcpTurnResult};
pub use transport::MAX_ACP_FRAME_BYTES;
