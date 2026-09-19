use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum EscalationApproval {
    #[default]
    #[serde(alias = "quick")]
    Auto,
    User,
    Off,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SandboxConfig {
    pub allow_network: bool,
    #[serde(default)]
    pub escalation_approval: EscalationApproval,
    #[serde(default)]
    pub escalate_to_user_on_deny: bool,
}
