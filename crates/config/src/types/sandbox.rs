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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    /// Enables Web tools exposed by a role; each call still follows its tool policy.
    pub web_tools_enabled: bool,
    #[serde(default)]
    pub escalation_approval: EscalationApproval,
    #[serde(default)]
    pub escalate_to_user_on_deny: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            web_tools_enabled: true,
            escalation_approval: EscalationApproval::default(),
            escalate_to_user_on_deny: false,
        }
    }
}
