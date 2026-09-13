//! Pure, fail-closed tool scope evaluation (ADR 0008); no execution or approval I/O.

use agents::{CapabilityDecision, NetworkAccess, RoleCapabilities};
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
    RoleNetwork,
    RoleCredential,
    PerToolPolicy,
    Session,
}

impl ScopeLayer {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RoleAllowlist => "role_allowlist",
            Self::RoleNetwork => "role_network",
            Self::RoleCredential => "role_credential",
            Self::PerToolPolicy => "per_tool_policy",
            Self::Session => "session",
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

/// Judge role -> per-tool -> session, returning the first deny before any ask.
///
/// Required scope is a typed slice; duplicates and ordering have no effect.
/// Tool-wide gates report its first canonical dimension (or None for empty scope).
/// Local filesystem/process grants come from the role allowlist and per-tool
/// policy; session and role network settings apply only to required network scope.
/// Role OptIn passes, as in `judge_web_network_access`; session OptIn asks.
/// With no denies, only the first approval gate is returned, not combined prose.
///
/// Credentials always deny: roles cannot grant them, and PolicyDecision cannot
/// distinguish an explicit override from default/wildcard AutoAllow. A future
/// credential grant needs a separate explicit authority, not a policy bypass.
///
/// The six inputs intentionally mirror the existing network evaluator plus scope:
/// keeping each independent authority visible avoids hiding the AND contract.
pub fn judge_tool_scope(
    role: &RoleCapabilities,
    role_name: &str,
    tool: &str,
    required: &[ScopeDimension],
    per_tool: PolicyDecision,
    session: NetworkAccess,
) -> ScopeDecision {
    let dimension = required.iter().copied().min();
    match role.check_tool(role_name, tool) {
        CapabilityDecision::Allowed => {}
        CapabilityDecision::Denied { .. } => {
            return ScopeDecision::deny(ScopeLayer::RoleAllowlist, dimension);
        }
    }
    let network_required = required.contains(&ScopeDimension::Network);
    if network_required {
        match role.network {
            NetworkAccess::Denied => {
                return ScopeDecision::deny(ScopeLayer::RoleNetwork, Some(ScopeDimension::Network));
            }
            NetworkAccess::OptIn | NetworkAccess::Allowed => {}
        }
    }
    if required.contains(&ScopeDimension::Credential) {
        return ScopeDecision::deny(ScopeLayer::RoleCredential, Some(ScopeDimension::Credential));
    }

    let approval = match per_tool {
        PolicyDecision::AutoAllow => None,
        PolicyDecision::Ask => Some(ScopeDecision::ask(ScopeLayer::PerToolPolicy, dimension)),
        PolicyDecision::Deny => {
            return ScopeDecision::deny(ScopeLayer::PerToolPolicy, dimension);
        }
    };
    if network_required {
        match session {
            NetworkAccess::Allowed => {}
            NetworkAccess::OptIn => {
                return approval.unwrap_or_else(|| {
                    ScopeDecision::ask(ScopeLayer::Session, Some(ScopeDimension::Network))
                });
            }
            NetworkAccess::Denied => {
                return ScopeDecision::deny(ScopeLayer::Session, Some(ScopeDimension::Network));
            }
        }
    }
    approval.unwrap_or(ScopeDecision::Allow)
}
