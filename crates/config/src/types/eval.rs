//! Settings for replaying threads to evaluate agent configurations (ADR 0036).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::agents::RoleBindingConfig;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EvalConfig {
    /// The model playing the original user. Unset uses the worker binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_simulator: Option<RoleBindingConfig>,
    /// The model grading the replays.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judge: Option<RoleBindingConfig>,
    /// Limits for one replay, counted over its whole delegation tree.
    pub budget: EvalBudgetConfig,
    /// Replays per configuration of the thread under consultation.
    pub repeat_target: u32,
    /// Replays per configuration of each regression thread.
    pub repeat_regression: u32,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            user_simulator: None,
            judge: None,
            budget: EvalBudgetConfig::default(),
            repeat_target: 2,
            repeat_regression: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EvalBudgetConfig {
    /// Messages the simulated user may send, the original first request included.
    pub max_user_turns: u32,
    /// Input plus output tokens of agent requests.
    pub max_tokens: u64,
    pub max_tool_calls: u32,
    pub max_elapsed_secs: u64,
}

impl Default for EvalBudgetConfig {
    fn default() -> Self {
        Self {
            max_user_turns: 20,
            max_tokens: 5_000_000,
            max_tool_calls: 500,
            max_elapsed_secs: 3_600,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn eval_defaults_apply_and_bindings_parse_partially() {
        // Given: no eval section, then one naming only a simulator chain and a budget field.
        let default = EvalConfig::default();
        let parsed: Config = toml::from_str(
            "[eval]\nrepeat_target = 3\n[eval.user_simulator]\nmodels = [{ route = \"cheap\" }]\n[eval.budget]\nmax_user_turns = 5",
        )
        .unwrap();

        // When / Then: omitted values keep their defaults.
        assert_eq!(toml::from_str::<Config>("").unwrap().eval, default);
        assert_eq!(default.budget.max_tokens, 5_000_000);
        assert_eq!(parsed.eval.repeat_target, 3);
        assert_eq!(parsed.eval.repeat_regression, 1);
        assert_eq!(
            parsed.eval.user_simulator.unwrap().head_route(),
            Some("cheap")
        );
        assert_eq!(parsed.eval.judge, None);
        assert_eq!(
            parsed.eval.budget,
            EvalBudgetConfig {
                max_user_turns: 5,
                ..EvalBudgetConfig::default()
            }
        );
        assert!(toml::from_str::<Config>("[eval.budget]\nmax_turns = 1").is_err());
    }
}
