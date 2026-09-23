//! agents セクション (ロール・カテゴリ単位のバインディング) の設定型を定義します。

use std::collections::{BTreeMap, BTreeSet};

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
    pub worker: WorkerBindingConfig,
    /// reviewer ロールのバインディング。
    pub reviewer: RoleBindingConfig,
    pub roles: AdditionalRoleBindings,
}

/// agents 内の明示的な logical_model 参照を旧名→新名で置き換える。
///
/// 各 role (orchestrator/explorer/worker/reviewer/roles.web_researcher/roles.planner/
/// roles.oracle/roles.multimodal_looker) と worker.categories の全 category が対象。
/// logical_model が None (暗黙の role 名参照) のものは変更しない。
/// 変換は元の値に対する1回の map lookup で行い、chain/swap での連続置換誤接続を防ぐ。
pub fn rename_logical_model_refs(agents: &mut AgentsConfig, renames: &BTreeMap<String, String>) {
    if renames.is_empty() {
        return;
    }

    for logical in [
        &mut agents.orchestrator.logical_model,
        &mut agents.explorer.logical_model,
        &mut agents.worker.base.logical_model,
        &mut agents.reviewer.logical_model,
        &mut agents.roles.web_researcher.logical_model,
        &mut agents.roles.planner.logical_model,
        &mut agents.roles.oracle.logical_model,
        &mut agents.roles.multimodal_looker.logical_model,
    ]
    .into_iter()
    .chain(
        agents
            .worker
            .categories
            .values_mut()
            .map(|binding| &mut binding.logical_model),
    )
    .flatten()
    {
        if let Some(new_name) = renames.get(logical) {
            *logical = new_name.clone();
        }
    }
}

/// 指定した論理モデルを明示的に使用するロールと worker カテゴリを返す。
pub fn roles_using(logical: &str, agents: &AgentsConfig) -> Vec<String> {
    explicit_refs(agents)
        .into_iter()
        .filter_map(|(address, name)| (name == logical).then_some(address))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// 明示的な logical_model 参照を、ロールまたはカテゴリのアドレスと組にして返す。
///
/// アドレスは [`roles_using`] と同じ表記。暗黙のロール名参照は含めない。
pub fn explicit_refs(agents: &AgentsConfig) -> Vec<(String, String)> {
    let mut refs: Vec<_> = role_bindings(agents)
        .into_iter()
        .filter_map(|(address, binding)| {
            binding
                .logical_model
                .as_ref()
                .map(|logical| (address.to_string(), logical.clone()))
        })
        .collect();
    for (category, binding) in &agents.worker.categories {
        if let Some(logical) = &binding.logical_model {
            refs.push((format!("worker.categories.{category}"), logical.clone()));
        }
    }
    refs
}

/// logical_model 未指定のため、ロール名へのフォールバックで指定論理名を使うロール。
///
/// アドレスは [`roles_using`] と同じ表記。カテゴリはロールの設定を継承するため含めない。
pub fn implicit_roles_using(logical: &str, agents: &AgentsConfig) -> Vec<String> {
    role_bindings(agents)
        .into_iter()
        .filter(|(address, binding)| {
            binding.logical_model.is_none()
                && address.strip_prefix("roles.").unwrap_or(address) == logical
        })
        .map(|(address, _)| address.to_string())
        .collect()
}

/// 設定上のアドレスと全8ロールのバインディングを列挙する。
fn role_bindings(agents: &AgentsConfig) -> [(&str, &RoleBindingConfig); 8] {
    [
        ("orchestrator", &agents.orchestrator),
        ("explorer", &agents.explorer),
        ("worker", &agents.worker.base),
        ("reviewer", &agents.reviewer),
        ("roles.web_researcher", &agents.roles.web_researcher),
        ("roles.planner", &agents.roles.planner),
        ("roles.oracle", &agents.roles.oracle),
        ("roles.multimodal_looker", &agents.roles.multimodal_looker),
    ]
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AdditionalRoleBindings {
    pub web_researcher: RoleBindingConfig,
    pub planner: RoleBindingConfig,
    pub oracle: RoleBindingConfig,
    pub multimodal_looker: RoleBindingConfig,
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
    /// worker 以外へのカテゴリ指定は [`ConfigError::CategoryNotAllowedForRole`]、
    /// ロール名が固定 8 ロール外なら [`ConfigError::UnknownAgentRole`]、
    /// カテゴリ名が固定 6 カテゴリ外なら [`ConfigError::UnknownCategory`] を返す。
    pub fn binding_for(
        &self,
        role: &str,
        category: Option<&str>,
    ) -> Result<ResolvedAgentBinding, ConfigError> {
        if role != "worker"
            && let Some(category) = category
        {
            return Err(ConfigError::CategoryNotAllowedForRole {
                role: role.to_string(),
                category: category.to_string(),
            });
        }
        let binding = match role {
            "orchestrator" => &self.orchestrator,
            "explorer" => &self.explorer,
            "worker" => &self.worker.base,
            "reviewer" => &self.reviewer,
            "web_researcher" => &self.roles.web_researcher,
            "planner" => &self.roles.planner,
            "oracle" => &self.roles.oracle,
            "multimodal_looker" => &self.roles.multimodal_looker,
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
        let category_binding = category.and_then(|name| self.worker.categories.get(name));
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
        reasoning_effort: category
            .reasoning_effort
            .clone()
            .or_else(|| role.reasoning_effort.clone()),
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
}

/// worker 専用のロール設定とカテゴリ別バインディング。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerBindingConfig {
    #[serde(flatten)]
    pub base: RoleBindingConfig,
    /// カテゴリ別のバインディング (キーは固定 6 カテゴリ名)。
    pub categories: BTreeMap<String, CategoryBindingConfig>,
}

/// 共通フィールドの読み取りは `worker.logical_model` 等でも可能。変更は `base` 経由。
impl std::ops::Deref for WorkerBindingConfig {
    type Target = RoleBindingConfig;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
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
    /// 推論強度。モデルごとに有効な値が異なるため自由形式の文字列とする。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
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

    #[test]
    fn explicit_refs_lists_all_roles_and_categories_with_matching_addresses() {
        // Given: every role and category explicitly selects the same logical model.
        let addresses = [
            "orchestrator",
            "explorer",
            "worker",
            "reviewer",
            "roles.web_researcher",
            "roles.planner",
            "roles.oracle",
            "roles.multimodal_looker",
        ]
        .into_iter()
        .map(String::from)
        .chain(
            CATEGORY_NAMES
                .iter()
                .map(|name| format!("worker.categories.{name}")),
        )
        .collect::<Vec<_>>();
        let document = addresses
            .iter()
            .map(|address| format!("[agents.{address}]\nlogical_model = 'shared'\n"))
            .collect::<String>();
        let agents = toml::from_str::<Config>(&document).expect("agents").agents;
        // When: enumerating explicit references.
        let refs = explicit_refs(&agents);
        let expected: BTreeMap<_, _> = addresses
            .iter()
            .map(|address| (address.clone(), "shared".into()))
            .collect();
        // Then: every address occurs once and matches roles_using's naming.
        assert_eq!(refs.len(), addresses.len());
        assert_eq!(refs.into_iter().collect::<BTreeMap<_, _>>(), expected);
        assert_eq!(
            roles_using("shared", &agents),
            expected.into_keys().collect::<Vec<_>>()
        );
        for logical in ["worker", "web_researcher", "shared"] {
            assert!(implicit_roles_using(logical, &agents).is_empty());
        }
    }

    #[test]
    fn explicit_refs_excludes_implicit_roles_and_categories() {
        // Given: a mixture of explicit and inherited references.
        let mut agents = AgentsConfig::default();
        agents
            .worker
            .categories
            .insert("quick".into(), Default::default());
        agents.explorer.logical_model = Some("explore".into());
        agents.worker.categories.insert(
            "deep".into(),
            CategoryBindingConfig {
                logical_model: Some("reason".into()),
                ..Default::default()
            },
        );
        // When: enumerating explicit references.
        let refs: BTreeMap<_, _> = explicit_refs(&agents).into_iter().collect();
        // Then: no implicit role or category is materialized.
        assert_eq!(
            refs,
            BTreeMap::from([
                ("explorer".into(), "explore".into()),
                ("worker.categories.deep".into(), "reason".into()),
            ])
        );
        assert!(explicit_refs(&AgentsConfig::default()).is_empty());
    }

    #[test]
    fn implicit_roles_using_matches_role_fallback_not_config_address_or_category() {
        // Given: all role names are implicit, and a category inherits worker's fallback.
        let mut agents = AgentsConfig::default();
        agents
            .worker
            .categories
            .insert("quick".into(), Default::default());
        // When: looking up every role's fallback name.
        for (logical, address) in [
            ("orchestrator", "orchestrator"),
            ("explorer", "explorer"),
            ("worker", "worker"),
            ("reviewer", "reviewer"),
            ("web_researcher", "roles.web_researcher"),
            ("planner", "roles.planner"),
            ("oracle", "roles.oracle"),
            ("multimodal_looker", "roles.multimodal_looker"),
        ] {
            // Then: only the role address is reported (not its inheriting categories).
            assert_eq!(implicit_roles_using(logical, &agents), [address]);
        }
        for logical in ["quick", "roles.web_researcher", "unknown", "Worker"] {
            assert!(implicit_roles_using(logical, &agents).is_empty());
        }
        // Given: an explicit reference equal to the fallback is still not implicit.
        agents.worker.base.logical_model = Some("worker".into());
        agents.roles.web_researcher.logical_model = Some("web_researcher".into());
        // When/Then: explicit references suppress fallback warnings.
        assert!(implicit_roles_using("worker", &agents).is_empty());
        assert!(implicit_roles_using("web_researcher", &agents).is_empty());
    }

    #[test]
    fn rename_logical_model_refs_updates_all_explicit_roles() {
        // Given: 全 8 ロールが同じ論理モデルを明示的に指定する。
        let doc = r#"
[agents.orchestrator]
logical_model = "old"
[agents.explorer]
logical_model = "old"
preset = "old"
[agents.worker]
logical_model = "old"
[agents.worker.generation]
reasoning_effort = "old"
[agents.reviewer]
logical_model = "old"
[agents.roles.web_researcher]
logical_model = "old"
[agents.roles.planner]
logical_model = "old"
[agents.roles.oracle]
logical_model = "old"
[agents.roles.multimodal_looker]
logical_model = "old"
"#;
        let mut agents = toml::from_str::<Config>(doc)
            .expect("agents fixture")
            .agents;
        let expected = toml::from_str::<Config>(
            &doc.replace("logical_model = \"old\"", "logical_model = \"new\""),
        )
        .expect("expected agents")
        .agents;

        rename_logical_model_refs(&mut agents, &[("old".into(), "new".into())].into());

        // Then: logical_model のみ変更し、preset や generation は保持する。
        assert_eq!(agents, expected);
    }

    #[test]
    fn rename_logical_model_refs_updates_all_categories() {
        let mut agents = AgentsConfig::default();
        for category in CATEGORY_NAMES {
            agents.worker.categories.insert(
                (*category).into(),
                CategoryBindingConfig {
                    logical_model: Some("old".into()),
                    preset: Some("old".into()),
                    ..Default::default()
                },
            );
        }
        let mut expected = agents.clone();
        for binding in expected.worker.categories.values_mut() {
            binding.logical_model = Some("new".into());
        }

        rename_logical_model_refs(&mut agents, &[("old".into(), "new".into())].into());

        assert_eq!(agents, expected);
    }

    #[test]
    fn rename_logical_model_refs_leaves_implicit_refs_untouched() {
        // Given: 全ロールとカテゴリで logical_model が未指定。
        let mut agents = AgentsConfig::default();
        agents.worker.categories.insert(
            "quick".into(),
            CategoryBindingConfig {
                preset: Some("quick-preset".into()),
                ..Default::default()
            },
        );
        let original = agents.clone();
        let renames = [
            "orchestrator",
            "explorer",
            "worker",
            "reviewer",
            "web_researcher",
            "planner",
            "oracle",
            "multimodal_looker",
            "quick",
        ]
        .into_iter()
        .map(|name| (name.into(), "renamed".into()))
        .collect();

        rename_logical_model_refs(&mut agents, &renames);

        assert_eq!(agents, original);

        // 明示的な worker 参照が変わっても、それを継承するカテゴリは未指定のまま。
        agents.worker.base.logical_model = Some("worker".into());
        let mut expected = agents.clone();
        expected.worker.base.logical_model = Some("renamed".into());
        rename_logical_model_refs(&mut agents, &renames);
        assert_eq!(agents, expected);
    }

    fn agents_with_a_and_b_refs() -> AgentsConfig {
        toml::from_str::<Config>(
            r#"
[agents.explorer]
logical_model = "A"
[agents.reviewer]
logical_model = "B"
[agents.worker.categories.quick]
logical_model = "A"
[agents.worker.categories.deep]
logical_model = "B"
"#,
        )
        .expect("agents fixture")
        .agents
    }

    #[test]
    fn rename_logical_model_refs_maps_chain_once() {
        let mut agents = agents_with_a_and_b_refs();
        let mut expected = agents.clone();
        expected.explorer.logical_model = Some("B".into());
        expected.reviewer.logical_model = Some("C".into());
        expected
            .worker
            .categories
            .get_mut("quick")
            .unwrap()
            .logical_model = Some("B".into());
        expected
            .worker
            .categories
            .get_mut("deep")
            .unwrap()
            .logical_model = Some("C".into());

        rename_logical_model_refs(
            &mut agents,
            &[("A".into(), "B".into()), ("B".into(), "C".into())].into(),
        );

        assert_eq!(agents, expected);
    }

    #[test]
    fn rename_logical_model_refs_swaps_original_refs() {
        let mut agents = agents_with_a_and_b_refs();
        let mut expected = agents.clone();
        expected.explorer.logical_model = Some("B".into());
        expected.reviewer.logical_model = Some("A".into());
        expected
            .worker
            .categories
            .get_mut("quick")
            .unwrap()
            .logical_model = Some("B".into());
        expected
            .worker
            .categories
            .get_mut("deep")
            .unwrap()
            .logical_model = Some("A".into());

        rename_logical_model_refs(
            &mut agents,
            &[("A".into(), "B".into()), ("B".into(), "A".into())].into(),
        );

        assert_eq!(agents, expected);
    }

    #[test]
    fn rename_logical_model_refs_empty_map_is_noop() {
        let mut agents = agents_with_a_and_b_refs();
        let original = agents.clone();

        rename_logical_model_refs(&mut agents, &BTreeMap::new());

        assert_eq!(agents, original);
    }

    #[test]
    fn rename_logical_model_refs_leaves_unmapped_refs_untouched() {
        let mut agents = agents_with_a_and_b_refs();
        let original = agents.clone();

        rename_logical_model_refs(&mut agents, &[("other".into(), "new".into())].into());

        assert_eq!(agents, original);
    }

    #[test]
    fn binding_for_rejects_category_when_role_is_not_worker() {
        // Given: worker 以外のロールと既知のカテゴリ。
        let agents = AgentsConfig::default();
        // When: explorer のカテゴリを解決する。
        let result = agents.binding_for("explorer", Some("quick"));
        // Then: ロールの既定値へ黙ってフォールバックしない。
        match result {
            Err(ConfigError::CategoryNotAllowedForRole { role, category }) => {
                assert_eq!(role, "explorer");
                assert_eq!(category, "quick");
            }
            other => panic!("CategoryNotAllowedForRole を期待した: {other:?}"),
        }
    }

    #[test]
    fn agents_binding_rejects_categories_when_role_is_not_worker() {
        // Given: explorer にカテゴリを設定した TOML。
        let doc = "[agents.explorer.categories.quick]\nlogical_model = \"fast\"\n";
        // When: 設定をパースする。
        let result = toml::from_str::<Config>(doc);
        // Then: categories は未知フィールドとして拒否される。
        let error = result.expect_err("worker 以外に categories は設定できない");
        assert!(error.to_string().contains("unknown field `categories`"));
    }

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
            Some("medium".to_owned())
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
        assert_eq!(generation.reasoning_effort, Some("high".to_owned()));
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

    #[test]
    fn roles_using_finds_base_roles_and_worker_categories() {
        // Given: 複数の固定ロールと worker カテゴリが同じ論理モデルを明示的に指定する。
        let doc = r#"
[agents.orchestrator]
logical_model = "shared"
[agents.explorer]
logical_model = "shared"
[agents.worker]
logical_model = "shared"
[agents.worker.categories.quick]
logical_model = "shared"
[agents.reviewer]
logical_model = "shared"
[agents.roles.web_researcher]
logical_model = "shared"
[agents.roles.planner]
logical_model = "shared"
[agents.roles.oracle]
logical_model = "shared"
[agents.roles.multimodal_looker]
logical_model = "shared"
"#;
        let config: Config = toml::from_str(doc).expect("agents 設定をパースできる");

        // When: 論理モデルを使用する明示的なロールとカテゴリを検索する。
        let result = roles_using("shared", &config.agents);

        // Then: 表示名がソート済みで重複なく返る。
        assert_eq!(
            result,
            vec![
                "explorer",
                "orchestrator",
                "reviewer",
                "roles.multimodal_looker",
                "roles.oracle",
                "roles.planner",
                "roles.web_researcher",
                "worker",
                "worker.categories.quick",
            ]
        );
    }

    #[test]
    fn roles_using_returns_empty_for_unused_logical() {
        // Given: 論理モデルを明示的に指定していない既定の agents 設定。
        let agents = AgentsConfig::default();

        // When: 未使用の論理モデルを検索する。
        let result = roles_using("unused", &agents);

        // Then: binding_for のロール名フォールバックを数えず空を返す。
        assert!(result.is_empty());
    }
}
