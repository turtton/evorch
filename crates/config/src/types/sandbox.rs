use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum EscalationApproval {
    /// `quick` is a load-time migration alias only; the generated schema intentionally
    /// lists only `auto`, `user`, and `off`. Runtime compatibility is not schema compatibility.
    #[default]
    #[serde(alias = "quick")]
    Auto,
    User,
    Off,
}

/// Session policy for web_search and web_fetch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WebToolAccess {
    #[default]
    Denied,
    OptIn,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub allow_network: bool,
    #[serde(default)]
    pub web_tool_access: WebToolAccess,
    #[serde(default)]
    pub escalation_approval: EscalationApproval,
    #[serde(default)]
    pub escalate_to_user_on_deny: bool,
}
