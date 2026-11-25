use std::fmt;

use serde::{Deserialize, Serialize};

/// Deployment topology governing fail-closed sandbox tier requirements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentMode {
    LocalSingleTenant,
    CloudMultiTenant,
}

impl DeploymentMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            DeploymentMode::LocalSingleTenant => "local_single_tenant",
            DeploymentMode::CloudMultiTenant => "cloud_multi_tenant",
        }
    }

    /// Parse a deployment mode string, failing closed on unrecognized values.
    pub fn parse(raw: &str) -> Result<Self, TierPolicyError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "local" | "local_single_tenant" | "standalone" => Ok(DeploymentMode::LocalSingleTenant),
            "cloud" | "cloud_multi_tenant" | "multitenant" | "multi_tenant" => {
                Ok(DeploymentMode::CloudMultiTenant)
            }
            other => Err(TierPolicyError::InvalidDeploymentMode(other.to_string())),
        }
    }

    /// Resolve deployment mode from environment variables with fail-closed behavior.
    pub fn from_env() -> Result<Self, TierPolicyError> {
        if let Ok(raw) = std::env::var("OHC_DEPLOYMENT_MODE")
            && !raw.trim().is_empty()
        {
            return Self::parse(&raw);
        }
        if std::env::var_os("KUBERNETES_SERVICE_HOST").is_some() {
            return Ok(DeploymentMode::CloudMultiTenant);
        }
        Ok(DeploymentMode::LocalSingleTenant)
    }
}

/// Ordered defense-in-depth isolation tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxTier {
    Tier1ToolWasm = 1,
    Tier2LocalOs = 2,
    Tier3CloudMicroVm = 3,
}

impl SandboxTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxTier::Tier1ToolWasm => "tier1_tool_wasm",
            SandboxTier::Tier2LocalOs => "tier2_local_os",
            SandboxTier::Tier3CloudMicroVm => "tier3_cloud_microvm",
        }
    }

    pub fn rank(&self) -> u8 {
        *self as u8
    }

    pub fn satisfies(&self, minimum_required: SandboxTier) -> bool {
        self.rank() >= minimum_required.rank()
    }
}

/// Workload security classification used to compute the minimum required sandbox tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadClass {
    PureToolWasm,
    ReadOnlyInspection,
    UntrustedShell,
    UntrustedCodeExecution,
    BrowserAutomation,
}

impl WorkloadClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkloadClass::PureToolWasm => "pure_tool_wasm",
            WorkloadClass::ReadOnlyInspection => "read_only_inspection",
            WorkloadClass::UntrustedShell => "untrusted_shell",
            WorkloadClass::UntrustedCodeExecution => "untrusted_code_execution",
            WorkloadClass::BrowserAutomation => "browser_automation",
        }
    }

    pub fn is_untrusted_os_workload(&self) -> bool {
        matches!(
            self,
            WorkloadClass::UntrustedShell
                | WorkloadClass::UntrustedCodeExecution
                | WorkloadClass::BrowserAutomation
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxExecutionRequest {
    pub tenant_id: String,
    pub deployment_mode: DeploymentMode,
    pub workload: WorkloadClass,
    pub requested_tier: SandboxTier,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxTierDecision {
    pub tenant_id: String,
    pub deployment_mode: DeploymentMode,
    pub workload: WorkloadClass,
    pub effective_tier: SandboxTier,
    pub minimum_required_tier: SandboxTier,
    pub requires_hardware_isolation: bool,
    pub requires_sentinel_proxy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TierPolicyError {
    MissingTenantId,
    InvalidDeploymentMode(String),
    InsufficientIsolation {
        deployment_mode: DeploymentMode,
        workload: WorkloadClass,
        requested: SandboxTier,
        minimum_required: SandboxTier,
        reason: String,
    },
}

impl fmt::Display for TierPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TierPolicyError::MissingTenantId => {
                write!(f, "tenant_id is required for sandbox tier enforcement")
            }
            TierPolicyError::InvalidDeploymentMode(raw) => {
                write!(f, "invalid deployment mode (fail-closed): {raw}")
            }
            TierPolicyError::InsufficientIsolation {
                deployment_mode,
                workload,
                requested,
                minimum_required,
                reason,
            } => write!(
                f,
                "sandbox tier policy denied {} in {} mode: requested {} < minimum {} ({})",
                workload.as_str(),
                deployment_mode.as_str(),
                requested.as_str(),
                minimum_required.as_str(),
                reason
            ),
        }
    }
}

impl std::error::Error for TierPolicyError {}

/// Compute the minimum required isolation tier for a deployment mode and workload class.
///
/// In `CloudMultiTenant`, untrusted shell/code/browser/OS execution MUST use `Tier3CloudMicroVm`.
pub fn minimum_required_tier(mode: DeploymentMode, workload: WorkloadClass) -> SandboxTier {
    match (mode, workload) {
        (DeploymentMode::CloudMultiTenant, WorkloadClass::PureToolWasm) => {
            SandboxTier::Tier1ToolWasm
        }
        (DeploymentMode::CloudMultiTenant, _) => SandboxTier::Tier3CloudMicroVm,
        (DeploymentMode::LocalSingleTenant, WorkloadClass::PureToolWasm) => {
            SandboxTier::Tier1ToolWasm
        }
        (DeploymentMode::LocalSingleTenant, _) => SandboxTier::Tier2LocalOs,
    }
}

/// Enforce fail-closed sandbox tier invariants for an execution request.
pub fn enforce_tier_policy(
    req: &SandboxExecutionRequest,
) -> Result<SandboxTierDecision, TierPolicyError> {
    let tenant_id = req.tenant_id.trim();
    if tenant_id.is_empty() {
        return Err(TierPolicyError::MissingTenantId);
    }

    let min_tier = minimum_required_tier(req.deployment_mode, req.workload);
    if !req.requested_tier.satisfies(min_tier) {
        let reason = match (req.deployment_mode, req.requested_tier) {
            (DeploymentMode::CloudMultiTenant, SandboxTier::Tier2LocalOs) => {
                "CloudMultiTenant forbids running untrusted tenant workloads in Tier2LocalOs on a shared host; Tier3CloudMicroVm is mandatory"
            }
            (DeploymentMode::CloudMultiTenant, _) => {
                "CloudMultiTenant requires Tier3CloudMicroVm isolation for untrusted tenant execution"
            }
            (DeploymentMode::LocalSingleTenant, _) => {
                "LocalSingleTenant OS workloads require at least Tier2LocalOs OS sandboxing"
            }
        };

        return Err(TierPolicyError::InsufficientIsolation {
            deployment_mode: req.deployment_mode,
            workload: req.workload,
            requested: req.requested_tier,
            minimum_required: min_tier,
            reason: reason.to_string(),
        });
    }

    Ok(SandboxTierDecision {
        tenant_id: tenant_id.to_string(),
        deployment_mode: req.deployment_mode,
        workload: req.workload,
        effective_tier: req.requested_tier,
        minimum_required_tier: min_tier,
        requires_hardware_isolation: req.requested_tier == SandboxTier::Tier3CloudMicroVm,
        requires_sentinel_proxy: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_req(
        tenant: &str,
        mode: DeploymentMode,
        workload: WorkloadClass,
        tier: SandboxTier,
    ) -> SandboxExecutionRequest {
        SandboxExecutionRequest {
            tenant_id: tenant.to_string(),
            deployment_mode: mode,
            workload,
            requested_tier: tier,
        }
    }

    #[test]
    fn cloud_multi_tenant_rejects_tier2_shell_code_browser_and_inspection() {
        for workload in [
            WorkloadClass::UntrustedShell,
            WorkloadClass::UntrustedCodeExecution,
            WorkloadClass::BrowserAutomation,
            WorkloadClass::ReadOnlyInspection,
        ] {
            let err = enforce_tier_policy(&make_req(
                "tenant-acme",
                DeploymentMode::CloudMultiTenant,
                workload,
                SandboxTier::Tier2LocalOs,
            ))
            .expect_err("CloudMultiTenant must reject Tier2LocalOs");

            match err {
                TierPolicyError::InsufficientIsolation {
                    deployment_mode,
                    requested,
                    minimum_required,
                    reason,
                    ..
                } => {
                    assert_eq!(deployment_mode, DeploymentMode::CloudMultiTenant);
                    assert_eq!(requested, SandboxTier::Tier2LocalOs);
                    assert_eq!(minimum_required, SandboxTier::Tier3CloudMicroVm);
                    assert!(reason.contains("Tier3CloudMicroVm"));
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }
    }

    #[test]
    fn cloud_multi_tenant_allows_tier3_microvm_and_tier1_pure_wasm() {
        let t3 = enforce_tier_policy(&make_req(
            "tenant-acme",
            DeploymentMode::CloudMultiTenant,
            WorkloadClass::UntrustedShell,
            SandboxTier::Tier3CloudMicroVm,
        ))
        .unwrap();
        assert_eq!(t3.effective_tier, SandboxTier::Tier3CloudMicroVm);
        assert!(t3.requires_hardware_isolation);
        assert!(t3.requires_sentinel_proxy);

        let t1 = enforce_tier_policy(&make_req(
            "tenant-acme",
            DeploymentMode::CloudMultiTenant,
            WorkloadClass::PureToolWasm,
            SandboxTier::Tier1ToolWasm,
        ))
        .unwrap();
        assert_eq!(t1.effective_tier, SandboxTier::Tier1ToolWasm);
        assert!(!t1.requires_hardware_isolation);
    }

    #[test]
    fn local_single_tenant_requires_at_least_tier2_for_os_workloads() {
        let ok_t2 = enforce_tier_policy(&make_req(
            "local-owner",
            DeploymentMode::LocalSingleTenant,
            WorkloadClass::UntrustedShell,
            SandboxTier::Tier2LocalOs,
        ))
        .unwrap();
        assert_eq!(ok_t2.effective_tier, SandboxTier::Tier2LocalOs);

        let ok_t3 = enforce_tier_policy(&make_req(
            "local-owner",
            DeploymentMode::LocalSingleTenant,
            WorkloadClass::UntrustedShell,
            SandboxTier::Tier3CloudMicroVm,
        ))
        .unwrap();
        assert!(ok_t3.requires_hardware_isolation);

        assert!(matches!(
            enforce_tier_policy(&make_req(
                "local-owner",
                DeploymentMode::LocalSingleTenant,
                WorkloadClass::UntrustedShell,
                SandboxTier::Tier1ToolWasm,
            )),
            Err(TierPolicyError::InsufficientIsolation {
                minimum_required: SandboxTier::Tier2LocalOs,
                ..
            })
        ));
    }

    #[test]
    fn missing_tenant_and_unknown_mode_fail_closed() {
        assert_eq!(
            enforce_tier_policy(&make_req(
                "   ",
                DeploymentMode::CloudMultiTenant,
                WorkloadClass::UntrustedShell,
                SandboxTier::Tier3CloudMicroVm,
            )),
            Err(TierPolicyError::MissingTenantId)
        );
        assert_eq!(
            DeploymentMode::parse("cloud_multi_tenant").unwrap(),
            DeploymentMode::CloudMultiTenant
        );
        assert_eq!(
            DeploymentMode::parse("local_single_tenant").unwrap(),
            DeploymentMode::LocalSingleTenant
        );
        assert!(matches!(
            DeploymentMode::parse("unsafe_mode"),
            Err(TierPolicyError::InvalidDeploymentMode(_))
        ));
    }
}
