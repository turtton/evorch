//! Pure, fail-closed tool scope evaluation (ADR 0008); no execution or approval I/O.

use agents::{CapabilityDecision, RoleCapabilities};
use sandbox::PolicyDecision;

/// Required dimensions, in canonical diagnostic priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScopeDimension {
    Network,
    FsRead,
    FsWrite,
    ProcessSpawn,
    Credential,
}

impl ScopeDimension {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::FsRead => "fs_read",
            Self::FsWrite => "fs_write",
            Self::ProcessSpawn => "process_spawn",
            Self::Credential => "credential",
        }
    }
}

/// The authority that refused or requires approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeLayer {
    RoleAllowlist,
    RoleCredential,
    PerToolPolicy,
}

impl ScopeLayer {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RoleAllowlist => "role_allowlist",
            Self::RoleCredential => "role_credential",
            Self::PerToolPolicy => "per_tool_policy",
        }
    }

    fn reason(self, dimension: Option<ScopeDimension>) -> String {
        format!(
            "{}.{}",
            self.as_str(),
            dimension.map_or("tool", ScopeDimension::as_str)
        )
    }
}

/// One blocking gate. `dimension: None` means a tool-wide gate on an empty scope.
/// Reasons contain only stable layer/dimension identifiers, never input names.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum ScopeDecision {
    Allow,
    Deny {
        layer: ScopeLayer,
        dimension: Option<ScopeDimension>,
        reason: String,
    },
    NeedsApproval {
        layer: ScopeLayer,
        dimension: Option<ScopeDimension>,
        reason: String,
    },
}

impl ScopeDecision {
    fn deny(layer: ScopeLayer, dimension: Option<ScopeDimension>) -> Self {
        Self::Deny {
            layer,
            dimension,
            reason: layer.reason(dimension),
        }
    }

    fn ask(layer: ScopeLayer, dimension: Option<ScopeDimension>) -> Self {
        Self::NeedsApproval {
            layer,
            dimension,
            reason: layer.reason(dimension),
        }
    }
}

/// Judge a tool call against the role allowlist and its own policy.
/// Required scope is retained for deterministic diagnostics. Credential scope
/// fails closed until an explicit credential authority exists.
pub fn judge_tool_scope(
    role: &RoleCapabilities,
    role_name: &str,
    tool: &str,
    required: &[ScopeDimension],
    per_tool: PolicyDecision,
) -> ScopeDecision {
    let dimension = required.iter().copied().min();
    match role.check_tool(role_name, tool) {
        CapabilityDecision::Allowed => {}
        CapabilityDecision::Denied { .. } => {
            return ScopeDecision::deny(ScopeLayer::RoleAllowlist, dimension);
        }
    }
    if required.contains(&ScopeDimension::Credential) {
        return ScopeDecision::deny(ScopeLayer::RoleCredential, Some(ScopeDimension::Credential));
    }
    match per_tool {
        PolicyDecision::AutoAllow => ScopeDecision::Allow,
        PolicyDecision::Ask => ScopeDecision::ask(ScopeLayer::PerToolPolicy, dimension),
        PolicyDecision::Deny => ScopeDecision::deny(ScopeLayer::PerToolPolicy, dimension),
    }
}
