//! agents セクション (ロール・カテゴリ単位のバインディング) の設定型を定義します。

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// agents バインディングで許可されるカテゴリ名 (固定 6 種)。
pub(crate) const CATEGORY_NAMES: &[&str] = &[
    "quick",
    "deep",
    "high-reasoning",
    "visual",
    "writing",
    "research",
];

/// ロール別のエージェントバインディング設定。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AgentsConfig {
    /// オーケストレータロールのバインディング。
    pub orchestrator: RoleBindingConfig,
    /// explorer ロールのバインディング。
    pub explorer: RoleBindingConfig,
    /// worker ロールのバインディング。
    pub worker: RoleBindingConfig,
    /// reviewer ロールのバインディング。
    pub reviewer: RoleBindingConfig,
}

impl AgentsConfig {
    /// ロール名とカテゴリからバインディングを解決する。
    ///
    /// 各フィールドはカテゴリ指定値がロール指定値に優先し、カテゴリが未指定の
    /// フィールドはロール値で補完する。`logical_model` の最終フォールバックは
    /// ロール名そのもの (小文字)。プリセットは名前の参照のみで、本文は
    /// 設定に含まれない。
    ///
    /// # Errors
    /// ロール名が固定 4 ロール外なら [`ConfigError::UnknownAgentRole`]、
    /// カテゴリ名が固定 6 カテゴリ外なら [`ConfigError::UnknownCategory`] を返す。
    pub fn binding_for(
        &self,
        role: &str,
        category: Option<&str>,
    ) -> Result<ResolvedAgentBinding, ConfigError> {
        let binding = match role {
            "orchestrator" => &self.orchestrator,
            "explorer" => &self.explorer,
            "worker" => &self.worker,
            "reviewer" => &self.reviewer,
            other => {
                return Err(ConfigError::UnknownAgentRole {
                    role: other.to_string(),
                });
            }
        };
        if let Some(category) = category
            && !CATEGORY_NAMES.contains(&category)
        {
            return Err(ConfigError::UnknownCategory {
                role: role.to_string(),
                category: category.to_string(),
            });
        }
        let category_binding = category.and_then(|name| binding.categories.get(name));
        let logical_model = category_binding
            .and_then(|found| found.logical_model.clone())
            .or_else(|| binding.logical_model.clone())
            .unwrap_or_else(|| role.to_string());
        let preset = category_binding
            .and_then(|found| found.preset.clone())
            .or_else(|| binding.preset.clone());
        let generation = match category_binding {
            Some(found) => merge_generation(&binding.generation, &found.generation),
            None => binding.generation.clone(),
        };
        Ok(ResolvedAgentBinding {
            logical_model,
            preset,
            generation,
        })
    }
}

/// ロールとカテゴリの生成パラメータ上書きをフィールド単位でマージする。
///
/// カテゴリ側で指定のあるフィールドが優先され、未指定のフィールドはロール側の
/// 値で補完される。
fn merge_generation(
    role: &GenerationOverridesConfig,
    category: &GenerationOverridesConfig,
) -> GenerationOverridesConfig {
    GenerationOverridesConfig {
        temperature: category.temperature.or(role.temperature),
        top_p: category.top_p.or(role.top_p),
        max_tokens: category.max_tokens.or(role.max_tokens),
        reasoning_effort: category.reasoning_effort.or(role.reasoning_effort),
    }
}

/// ロール 1 件分のバインディング設定。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RoleBindingConfig {
    /// 使用する論理モデル名。省略時はロール名 (小文字) を使用する。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_model: Option<String>,
    /// 使用するプリセット名。本文ではなく参照のみを記述する。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// 生成パラメータの上書き。
    pub generation: GenerationOverridesConfig,
    /// カテゴリ別のバインディング (キーは固定 6 カテゴリ名)。
    pub categories: BTreeMap<String, CategoryBindingConfig>,
}

/// カテゴリ 1 件分のバインディング設定。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CategoryBindingConfig {
    /// 使用する論理モデル名。省略時はロール側の指定に従う。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_model: Option<String>,
    /// 使用するプリセット名。省略時はロール側の指定に従う。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// 生成パラメータの上書き。
    pub generation: GenerationOverridesConfig,
}

/// 生成パラメータの上書き指定。指定したフィールドのみ反映する。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GenerationOverridesConfig {
    /// サンプリング温度。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// nucleus サンプリングの確率質量。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// 最大出力トークン数。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// 推論強度。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffortConfig>,
}

/// 推論強度の指定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffortConfig {
    /// 低強度。
    Low,
    /// 中強度。
    #[default]
    Medium,
    /// 高強度。
    High,
}

/// [`AgentsConfig::binding_for`] の解決結果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedAgentBinding {
    /// 実際に使用する論理モデル名。
    pub logical_model: String,
    /// 実際に使用するプリセット名 (未指定なら None)。
    pub preset: Option<String>,
    /// マージ済みの生成パラメータ上書き。
    pub generation: GenerationOverridesConfig,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, ConfigError};

    // Given: ロール直下と categories.quick の両方に logical_model / preset を含む設定 TOML
    // When: Config にパースする
    // Then: 各値が意図どおり読み取れる
    #[test]
    fn agents_binding_parses_role_category_logical_model_and_preset() {
        let doc = r#"
[agents.worker]
logical_model = "worker"
preset = "worker-appendix"

[agents.worker.generation]
temperature = 0.7
top_p = 1.0
max_tokens = 4096
reasoning_effort = "medium"

[agents.worker.categories.quick]
logical_model = "worker-quick"
preset = "quick-appendix"
"#;

        let config: Config = toml::from_str(doc).expect("agents 設定をパースできる");
        let agents = &config.agents;

        assert_eq!(agents.worker.logical_model.as_deref(), Some("worker"));
        assert_eq!(agents.worker.preset.as_deref(), Some("worker-appendix"));
        assert_eq!(agents.worker.generation.temperature, Some(0.7));
        assert_eq!(agents.worker.generation.top_p, Some(1.0));
        assert_eq!(agents.worker.generation.max_tokens, Some(4096));
        assert_eq!(
            agents.worker.generation.reasoning_effort,
            Some(ReasoningEffortConfig::Medium)
        );
        let quick = agents
            .worker
            .categories
            .get("quick")
            .expect("quick カテゴリが存在する");
        assert_eq!(quick.logical_model.as_deref(), Some("worker-quick"));
        assert_eq!(quick.preset.as_deref(), Some("quick-appendix"));
    }

    // Given: [agents.worker] 直下に prompt 本文や system 本文を含む設定 TOML
    // When: Config にパースする
    // Then: いずれも unknown field として拒否される (preset は参照のみ許可)
    #[test]
    fn agents_binding_rejects_prompt_body_field() {
        let prompt_doc = "[agents.worker]\nprompt = \"あなたは worker です\"\n";
        let system_doc = "[agents.worker]\nsystem = \"あなたは worker です\"\n";

        let prompt_error =
            toml::from_str::<Config>(prompt_doc).expect_err("prompt 本文は拒否される");
        let system_error =
            toml::from_str::<Config>(system_doc).expect_err("system 本文は拒否される");

        assert!(
            prompt_error.to_string().contains("`prompt`"),
            "prompt 本文フィールドの拒否エラー: {prompt_error}"
        );
        assert!(
            system_error.to_string().contains("`system`"),
            "system 本文フィールドの拒否エラー: {system_error}"
        );
    }

    // Given: generation の 4 フィールドをすべて含む設定 TOML
    // When: GenerationOverridesConfig にパースする
    // Then: 型付きの値として読み取れる
    #[test]
    fn generation_overrides_parse_typed_fields() {
        let doc = r#"
temperature = 0.2
top_p = 0.9
max_tokens = 8192
reasoning_effort = "high"
"#;

        let generation: GenerationOverridesConfig =
            toml::from_str(doc).expect("generation 上書きをパースできる");

        assert_eq!(generation.temperature, Some(0.2));
        assert_eq!(generation.top_p, Some(0.9));
        assert_eq!(generation.max_tokens, Some(8192));
        assert_eq!(
            generation.reasoning_effort,
            Some(ReasoningEffortConfig::High)
        );
    }

    // Given: generation に未知のキーを含む設定 TOML / When: パースする
    // Then: エラーとして拒否される
    #[test]
    fn generation_overrides_reject_unknown_field() {
        let doc = "temperature = 0.2\nseed = 42\n";

        let result = toml::from_str::<GenerationOverridesConfig>(doc);

        assert!(
            result.is_err(),
            "generation の未知キーは拒否される: {result:?}"
        );
    }

    // Given: ロールとカテゴリの両方にフィールド単位で値がある設定
    // When: binding_for("worker", Some("quick")) を呼ぶ
    // Then: カテゴリ値がロール値にフィールド単位で優先する (未指定はロール値で補完)
    #[test]
    fn binding_for_prefers_category_over_role_per_field() {
        let doc = r#"
[agents.worker]
logical_model = "worker"
preset = "worker-appendix"

[agents.worker.generation]
temperature = 0.2
max_tokens = 4096

[agents.worker.categories.quick]
logical_model = "worker-quick"

[agents.worker.categories.quick.generation]
temperature = 0.9
"#;
        let config: Config = toml::from_str(doc).expect("agents 設定をパースできる");

        let resolved = config
            .agents
            .binding_for("worker", Some("quick"))
            .expect("カテゴリバインディングを解決できる");

        assert_eq!(resolved.logical_model, "worker-quick");
        assert_eq!(resolved.preset.as_deref(), Some("worker-appendix"));
        assert_eq!(resolved.generation.temperature, Some(0.9));
        assert_eq!(resolved.generation.max_tokens, Some(4096));
        assert_eq!(resolved.generation.top_p, None);
    }

    // Given: 固定 6 カテゴリ以外のカテゴリ名 / When: binding_for を呼ぶ
    // Then: UnknownCategory の型付きエラーになる
    #[test]
    fn binding_for_unknown_category_is_typed_error() {
        let agents = AgentsConfig::default();

        let result = agents.binding_for("worker", Some("typo"));

        match result {
            Err(ConfigError::UnknownCategory { role, category }) => {
                assert_eq!(role, "worker");
                assert_eq!(category, "typo");
            }
            other => panic!("UnknownCategory を期待した: {other:?}"),
        }
    }

    // Given: 何も設定していない既定の AgentsConfig / When: binding_for("worker", None) を呼ぶ
    // Then: logical_model はロール名 "worker" になり、preset は None
    #[test]
    fn binding_for_defaults_logical_model_to_role_name() {
        let agents = AgentsConfig::default();

        let resolved = agents
            .binding_for("worker", None)
            .expect("既定バインディングを解決できる");

        assert_eq!(resolved.logical_model, "worker");
        assert_eq!(resolved.preset, None);
        assert_eq!(resolved.generation, GenerationOverridesConfig::default());
    }

    // Given: 固定 4 ロール以外のロール名 / When: binding_for を呼ぶ
    // Then: UnknownAgentRole の型付きエラーになる
    #[test]
    fn binding_for_unknown_role_is_typed_error() {
        let agents = AgentsConfig::default();

        let result = agents.binding_for("typo", None);

        match result {
            Err(ConfigError::UnknownAgentRole { role }) => assert_eq!(role, "typo"),
            other => panic!("UnknownAgentRole を期待した: {other:?}"),
        }
    }
}
