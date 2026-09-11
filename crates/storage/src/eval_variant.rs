use crate::eval::Attribution;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptVersion {
    pub version: String,
    pub system: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleRoute {
    pub role: Attribution,
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Topology {
    #[default]
    Single,
    Sequence {
        roles: Vec<Attribution>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArenaVariant {
    pub prompt: Option<PromptVersion>,
    pub routing: Vec<RoleRoute>,
    pub topology: Topology,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalStep {
    pub role: Attribution,
    pub model: String,
    pub output: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalExecution {
    pub variant: ArenaVariant,
    pub steps: Vec<EvalStep>,
}
