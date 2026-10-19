//! agents セクション (ロール・カテゴリ単位のバインディング) の設定型を定義します。

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ConfigError;
use crate::agent_categories::{CATEGORIES, category_for_role};

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
    pub reviewer: CategorizedRoleBindingConfig,
    pub roles: AdditionalRoleBindings,
}

/// agents 内の明示的な route 参照 (チェーンの要素) を旧名→新名で置き換える。
///
/// 各 role (orchestrator/explorer/worker/reviewer/roles.web_researcher/roles.planner/
/// roles.advisor/roles.multimodal_looker) と worker.categories / reviewer.categories の全 category が対象。
/// `models` が空 (暗黙の role 名参照) のものは変更しない。
/// 変換は元の値に対する1回の map lookup で行い、chain/swap での連続置換誤接続を防ぐ。
pub fn rename_route_refs(agents: &mut AgentsConfig, renames: &BTreeMap<String, String>) {
    if renames.is_empty() {
        return;
    }

    for models in [
        &mut agents.orchestrator.models,
        &mut agents.explorer.models,
        &mut agents.worker.base.models,
        &mut agents.reviewer.base.models,
        &mut agents.roles.web_researcher.models,
        &mut agents.roles.planner.models,
        &mut agents.roles.advisor.models,
        &mut agents.roles.multimodal_looker.models,
        &mut agents.roles.compactor.models,
    ]
    .into_iter()
    .chain(
        agents
            .worker
            .categories
            .values_mut()
            .map(|binding| &mut binding.models),
    )
    .chain(
        agents
            .reviewer
            .categories
            .values_mut()
            .map(|binding| &mut binding.models),
    ) {
        for entry in models {
            if let Some(new_name) = renames.get(&entry.route) {
                entry.route.clone_from(new_name);
            }
        }
    }
}

/// 指定した route を明示的に使用するロールと worker/reviewer カテゴリを返す。
pub fn roles_using(logical: &str, agents: &AgentsConfig) -> Vec<String> {
    explicit_refs(agents)
        .into_iter()
        .filter_map(|(address, name)| (name == logical).then_some(address))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// 明示的な route 参照 (チェーンの各要素) を、ロールまたはカテゴリのアドレスと組にして返す。
///
/// アドレスは [`roles_using`] と同じ表記。暗黙のロール名参照は含めない。
/// 1 つのアドレスがチェーンの要素数だけ現れる。
pub fn explicit_refs(agents: &AgentsConfig) -> Vec<(String, String)> {
    let mut refs: Vec<_> = role_bindings(agents)
        .into_iter()
        .flat_map(|(address, binding)| {
            binding
                .models
                .iter()
                .map(move |entry| (address.to_string(), entry.route.clone()))
        })
        .collect();
    for (role, categories) in [
        ("worker", &agents.worker.categories),
        ("reviewer", &agents.reviewer.categories),
    ] {
        for (category, binding) in categories {
            for entry in &binding.models {
                refs.push((format!("{role}.categories.{category}"), entry.route.clone()));
            }
        }
    }
    refs
}

/// [`explicit_refs`] のアドレスが指すチェーン。未知のアドレスは `None`。
/// カテゴリが未設定なら空の既定値を作って返す。
pub fn chain_at_mut<'a>(
    agents: &'a mut AgentsConfig,
    address: &str,
) -> Option<&'a mut Vec<ChainEntryConfig>> {
    let binding = match address {
        "orchestrator" => &mut agents.orchestrator,
        "explorer" => &mut agents.explorer,
        "worker" => &mut agents.worker.base,
        "reviewer" => &mut agents.reviewer.base,
        "roles.web_researcher" => &mut agents.roles.web_researcher,
        "roles.planner" => &mut agents.roles.planner,
        "roles.advisor" => &mut agents.roles.advisor,
        "roles.multimodal_looker" => &mut agents.roles.multimodal_looker,
        "roles.compactor" => &mut agents.roles.compactor,
        address => {
            let (categories, category) =
                if let Some(category) = address.strip_prefix("worker.categories.") {
                    (&mut agents.worker.categories, category)
                } else {
                    (
                        &mut agents.reviewer.categories,
                        address.strip_prefix("reviewer.categories.")?,
                    )
                };
            return Some(&mut categories.entry(category.to_owned()).or_default().models);
        }
    };
    Some(&mut binding.models)
}

/// `models` 未指定のため、ロール名へのフォールバックで指定 route を使うロール。
///
/// アドレスは [`roles_using`] と同じ表記。カテゴリはロールの設定を継承するため含めない。
pub fn implicit_roles_using(logical: &str, agents: &AgentsConfig) -> Vec<String> {
    role_bindings(agents)
        .into_iter()
        .filter(|(address, binding)| {
            // The compactor has no implicit route: without a chain it uses the run's own.
            binding.models.is_empty()
                && *address != "roles.compactor"
                && address.strip_prefix("roles.").unwrap_or(address) == logical
        })
        .map(|(address, _)| address.to_string())
        .collect()
}

/// 設定上のアドレスと全8ロール、および compactor のバインディングを列挙する。
fn role_bindings(agents: &AgentsConfig) -> [(&str, &RoleBindingConfig); 9] {
    [
        ("orchestrator", &agents.orchestrator),
        ("explorer", &agents.explorer),
        ("worker", &agents.worker.base),
        ("reviewer", &agents.reviewer),
        ("roles.web_researcher", &agents.roles.web_researcher),
        ("roles.planner", &agents.roles.planner),
        ("roles.advisor", &agents.roles.advisor),
        ("roles.multimodal_looker", &agents.roles.multimodal_looker),
        ("roles.compactor", &agents.roles.compactor),
    ]
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AdditionalRoleBindings {
    pub web_researcher: RoleBindingConfig,
    pub planner: RoleBindingConfig,
    #[serde(alias = "oracle")]
    pub advisor: RoleBindingConfig,
    pub multimodal_looker: RoleBindingConfig,
    /// 要約 (compaction) にだけ使うチェーン。agent としては起動しない。
    /// `models` が空なら、実行中ロールのチェーンで要約する (ADR 0030)。
    pub compactor: RoleBindingConfig,
}

impl AgentsConfig {
    /// ロール名とカテゴリからバインディングを解決する。
    ///
    /// 各フィールドはカテゴリ指定値がロール指定値に優先し、カテゴリが未指定の
    /// フィールドはロール値で補完する。`models` の最終フォールバックは
    /// ロール名そのもの (小文字) の route 1 要素。プリセットは名前の参照のみで、本文は
    /// 設定に含まれない。
    ///
    /// # Errors
    /// worker/reviewer 以外、またはカテゴリの所属ロールと異なる指定は
    /// [`ConfigError::CategoryNotAllowedForRole`] を返す。
    /// 未知のロール・カテゴリは型付きエラーになる。
    pub fn binding_for(
        &self,
        role: &str,
        category: Option<&str>,
    ) -> Result<ResolvedAgentBinding, ConfigError> {
        let role = if role == "oracle" { "advisor" } else { role };
        if let Some(category) = category
            && category_for_role(role, category).is_none()
            && (!matches!(role, "worker" | "reviewer")
                || CATEGORIES
                    .iter()
                    .any(|defined| defined.id.as_str() == category))
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
            "reviewer" => &self.reviewer.base,
            "web_researcher" => &self.roles.web_researcher,
            "planner" => &self.roles.planner,
            "advisor" => &self.roles.advisor,
            "multimodal_looker" => &self.roles.multimodal_looker,
            other => {
                return Err(ConfigError::UnknownAgentRole {
                    role: other.to_string(),
                });
            }
        };
        if let Some(category) = category
            && category_for_role(role, category).is_none()
        {
            return Err(ConfigError::UnknownCategory {
                role: role.to_string(),
                category: category.to_string(),
            });
        }
        let category_binding = match role {
            "worker" => category.and_then(|name| self.worker.categories.get(name)),
            "reviewer" => category.and_then(|name| self.reviewer.categories.get(name)),
            _ => None,
        };
        let models = category_binding
            .map(|found| &found.models)
            .filter(|models| !models.is_empty())
            .or_else(|| Some(&binding.models).filter(|models| !models.is_empty()))
            .cloned()
            .unwrap_or_else(|| vec![ChainEntryConfig::route(role)]);
        let preset = category_binding
            .and_then(|found| found.preset.clone())
            .or_else(|| binding.preset.clone());
        let generation = match category_binding {
            Some(found) => merge_generation(&binding.generation, &found.generation),
            None => binding.generation.clone(),
        };
        Ok(ResolvedAgentBinding {
            models,
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
    }
}

/// ロール 1 件分のバインディング設定。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RoleBindingConfig {
    /// 優先順に並べた route のチェーン。省略時はロール名 (小文字) の route 1 要素。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ChainEntryConfig>,
    /// 使用するプリセット名。本文ではなく参照のみを記述する。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// 生成パラメータの上書き。
    pub generation: GenerationOverridesConfig,
}

impl RoleBindingConfig {
    /// チェーンの先頭要素の route 名。`models` 未指定なら `None`。
    pub fn head_route(&self) -> Option<&str> {
        self.models.first().map(|entry| entry.route.as_str())
    }
}

/// カテゴリを持つロールのベース設定とカテゴリ別バインディング。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CategorizedRoleBindingConfig {
    #[serde(flatten)]
    pub base: RoleBindingConfig,
    /// ロールに属するカテゴリ別のバインディング。
    pub categories: BTreeMap<String, CategoryBindingConfig>,
}

impl std::ops::Deref for CategorizedRoleBindingConfig {
    type Target = RoleBindingConfig;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl std::ops::DerefMut for CategorizedRoleBindingConfig {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.base
    }
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

/// 共通フィールドの読み取りは `worker.models` 等でも可能。変更は `base` 経由。
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
    /// 優先順に並べた route のチェーン。省略時はロール側の指定に従う。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<ChainEntryConfig>,
    /// 使用するプリセット名。省略時はロール側の指定に従う。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// 生成パラメータの上書き。
    pub generation: GenerationOverridesConfig,
}

impl CategoryBindingConfig {
    /// チェーンの先頭要素の route 名。`models` 未指定 (ロール側に従う) なら `None`。
    pub fn head_route(&self) -> Option<&str> {
        self.models.first().map(|entry| entry.route.as_str())
    }
}

/// ロールのチェーンの要素 1 件 (ADR 0030)。
///
/// route (同じモデルへの経路プール) を名前で参照し、その route のメンバーへ送る
/// 推論強度を指定する。チェーンがチェーンを参照することはない。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ChainEntryConfig {
    /// 参照する `routing.routes` の名前。
    pub route: String,
    /// この要素で送る推論強度。省略時はプロバイダ既定。モデルごとに有効な値が異なるため自由形式の文字列とする。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

impl ChainEntryConfig {
    /// 推論強度を指定しない要素を作る。
    pub fn route(route: impl Into<String>) -> Self {
        Self {
            route: route.into(),
            reasoning_effort: None,
        }
    }

    /// 推論強度を指定した要素を作る。
    pub fn with_effort(route: impl Into<String>, reasoning_effort: impl Into<String>) -> Self {
        Self {
            route: route.into(),
            reasoning_effort: Some(reasoning_effort.into()),
        }
    }
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
}

/// [`AgentsConfig::binding_for`] の解決結果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedAgentBinding {
    /// 実際に使用するチェーン。常に 1 要素以上。
    pub models: Vec<ChainEntryConfig>,
    /// 実際に使用するプリセット名 (未指定なら None)。
    pub preset: Option<String>,
    /// マージ済みの生成パラメータ上書き。
    pub generation: GenerationOverridesConfig,
}

impl ResolvedAgentBinding {
    /// チェーンの先頭要素の route 名。
    pub fn head_route(&self) -> &str {
        self.models.first().map_or("", |entry| entry.route.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_categories::categories_for_role;
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
            "roles.advisor",
            "roles.multimodal_looker",
        ]
        .into_iter()
        .map(String::from)
        .chain(
            categories_for_role("worker")
                .map(|category| format!("worker.categories.{}", category.id.as_str())),
        )
        .collect::<Vec<_>>();
        let document = addresses
            .iter()
            .map(|address| format!("[agents.{address}]\nmodels = [{{ route = 'shared' }}]\n"))
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
        agents.explorer.models = vec![ChainEntryConfig::route("explore")];
        agents.worker.categories.insert(
            "deep".into(),
            CategoryBindingConfig {
                models: vec![ChainEntryConfig::route("reason")],
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
            ("advisor", "roles.advisor"),
            ("multimodal_looker", "roles.multimodal_looker"),
        ] {
            // Then: only the role address is reported (not its inheriting categories).
            assert_eq!(implicit_roles_using(logical, &agents), [address]);
        }
        for logical in [
            "quick",
            "roles.web_researcher",
            "unknown",
            "Worker",
            "compactor",
        ] {
            assert!(implicit_roles_using(logical, &agents).is_empty());
        }
        // Given: an explicit reference equal to the fallback is still not implicit.
        agents.worker.base.models = vec![ChainEntryConfig::route("worker")];
        agents.roles.web_researcher.models = vec![ChainEntryConfig::route("web_researcher")];
        // When/Then: explicit references suppress fallback warnings.
        assert!(implicit_roles_using("worker", &agents).is_empty());
        assert!(implicit_roles_using("web_researcher", &agents).is_empty());
    }

    #[test]
    fn rename_route_refs_updates_all_explicit_roles() {
        // Given: 全 8 ロールと compactor が同じ論理モデルを明示的に指定する。
        let doc = r#"
[agents.orchestrator]
models = [{ route = "old" }]
[agents.explorer]
models = [{ route = "old" }]
preset = "old"
[agents.worker]
models = [{ route = "old" }]
[agents.worker.generation]
max_tokens = 1
[agents.reviewer]
models = [{ route = "old" }]
[agents.roles.web_researcher]
models = [{ route = "old" }]
[agents.roles.planner]
models = [{ route = "old" }]
[agents.roles.advisor]
models = [{ route = "old" }]
[agents.roles.multimodal_looker]
models = [{ route = "old" }]
[agents.roles.compactor]
models = [{ route = "old" }]
"#;
        let mut agents = toml::from_str::<Config>(doc)
            .expect("agents fixture")
            .agents;
        let expected = toml::from_str::<Config>(&doc.replace(
            "models = [{ route = \"old\" }]",
            "models = [{ route = \"new\" }]",
        ))
        .expect("expected agents")
        .agents;

        rename_route_refs(&mut agents, &[("old".into(), "new".into())].into());

        // Then: route 参照のみ変更し、preset や generation は保持する。
        assert_eq!(agents, expected);
    }

    #[test]
    fn rename_route_refs_updates_all_categories() {
        let mut agents = AgentsConfig::default();
        for category in categories_for_role("worker") {
            agents.worker.categories.insert(
                category.id.as_str().into(),
                CategoryBindingConfig {
                    models: vec![ChainEntryConfig::route("old")],
                    preset: Some("old".into()),
                    ..Default::default()
                },
            );
        }
        let mut expected = agents.clone();
        for binding in expected.worker.categories.values_mut() {
            binding.models = vec![ChainEntryConfig::route("new")];
        }

        rename_route_refs(&mut agents, &[("old".into(), "new".into())].into());

        assert_eq!(agents, expected);
    }

    #[test]
    fn rename_route_refs_leaves_implicit_refs_untouched() {
        // Given: 全ロールとカテゴリで models が未指定。
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
            "advisor",
            "multimodal_looker",
            "quick",
        ]
        .into_iter()
        .map(|name| (name.into(), "renamed".into()))
        .collect();

        rename_route_refs(&mut agents, &renames);

        assert_eq!(agents, original);

        // 明示的な worker 参照が変わっても、それを継承するカテゴリは未指定のまま。
        agents.worker.base.models = vec![ChainEntryConfig::route("worker")];
        let mut expected = agents.clone();
        expected.worker.base.models = vec![ChainEntryConfig::route("renamed")];
        rename_route_refs(&mut agents, &renames);
        assert_eq!(agents, expected);
    }

    fn agents_with_a_and_b_refs() -> AgentsConfig {
        toml::from_str::<Config>(
            r#"
[agents.explorer]
models = [{ route = "A" }]
[agents.reviewer]
models = [{ route = "B" }]
[agents.worker.categories.quick]
models = [{ route = "A" }]
[agents.worker.categories.deep]
models = [{ route = "B" }]
"#,
        )
        .expect("agents fixture")
        .agents
    }

    #[test]
    fn rename_route_refs_maps_chain_once() {
        let mut agents = agents_with_a_and_b_refs();
        let mut expected = agents.clone();
        expected.explorer.models = vec![ChainEntryConfig::route("B")];
        expected.reviewer.models = vec![ChainEntryConfig::route("C")];
        expected.worker.categories.get_mut("quick").unwrap().models =
            vec![ChainEntryConfig::route("B")];
        expected.worker.categories.get_mut("deep").unwrap().models =
            vec![ChainEntryConfig::route("C")];

        rename_route_refs(
            &mut agents,
            &[("A".into(), "B".into()), ("B".into(), "C".into())].into(),
        );

        assert_eq!(agents, expected);
    }

    #[test]
    fn rename_route_refs_swaps_original_refs() {
        let mut agents = agents_with_a_and_b_refs();
        let mut expected = agents.clone();
        expected.explorer.models = vec![ChainEntryConfig::route("B")];
        expected.reviewer.models = vec![ChainEntryConfig::route("A")];
        expected.worker.categories.get_mut("quick").unwrap().models =
            vec![ChainEntryConfig::route("B")];
        expected.worker.categories.get_mut("deep").unwrap().models =
            vec![ChainEntryConfig::route("A")];

        rename_route_refs(
            &mut agents,
            &[("A".into(), "B".into()), ("B".into(), "A".into())].into(),
        );

        assert_eq!(agents, expected);
    }

    #[test]
    fn rename_route_refs_empty_map_is_noop() {
        let mut agents = agents_with_a_and_b_refs();
        let original = agents.clone();

        rename_route_refs(&mut agents, &BTreeMap::new());

        assert_eq!(agents, original);
    }

    #[test]
    fn rename_route_refs_leaves_unmapped_refs_untouched() {
        let mut agents = agents_with_a_and_b_refs();
        let original = agents.clone();

        rename_route_refs(&mut agents, &[("other".into(), "new".into())].into());

        assert_eq!(agents, original);
    }

    #[test]
    fn binding_for_rejects_category_when_role_is_not_categorized() {
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
    fn private_lesson_categories_route_only_to_their_roles() {
        let agents = AgentsConfig::default();
        assert_eq!(
            agents
                .binding_for("worker", Some("lesson"))
                .unwrap()
                .head_route(),
            "worker"
        );
        assert_eq!(
            agents
                .binding_for("reviewer", Some("lesson_review"))
                .unwrap()
                .head_route(),
            "reviewer"
        );
        assert!(matches!(
            agents.binding_for("worker", Some("lesson_review")),
            Err(ConfigError::CategoryNotAllowedForRole { .. })
        ));
        assert!(matches!(
            agents.binding_for("reviewer", Some("lesson")),
            Err(ConfigError::CategoryNotAllowedForRole { .. })
        ));
    }

    #[test]
    fn agents_binding_rejects_categories_when_role_is_not_categorized() {
        // Given: explorer にカテゴリを設定した TOML。
        let doc = "[agents.explorer.categories.quick]\nmodels = [{ route = \"fast\" }]\n";
        // When: 設定をパースする。
        let result = toml::from_str::<Config>(doc);
        // Then: categories は未知フィールドとして拒否される。
        let error = result.expect_err("worker/reviewer 以外に categories は設定できない");
        assert!(error.to_string().contains("unknown field `categories`"));
    }

    // Given: ロール直下と categories.quick の両方に models / preset を含む設定 TOML
    // When: Config にパースする
    // Then: 各値が意図どおり読み取れる
    #[test]
    fn agents_binding_parses_role_category_models_and_preset() {
        let doc = r#"
[agents.worker]
models = [{ route = "worker" }]
preset = "worker-appendix"

[agents.worker.generation]
temperature = 0.7
top_p = 1.0
max_tokens = 4096

[agents.worker.categories.quick]
models = [{ route = "worker-quick" }]
preset = "quick-appendix"
"#;

        let config: Config = toml::from_str(doc).expect("agents 設定をパースできる");
        let agents = &config.agents;

        assert_eq!(agents.worker.head_route(), Some("worker"));
        assert_eq!(agents.worker.preset.as_deref(), Some("worker-appendix"));
        assert_eq!(agents.worker.generation.temperature, Some(0.7));
        assert_eq!(agents.worker.generation.top_p, Some(1.0));
        assert_eq!(agents.worker.generation.max_tokens, Some(4096));
        let quick = agents
            .worker
            .categories
            .get("quick")
            .expect("quick カテゴリが存在する");
        assert_eq!(quick.head_route(), Some("worker-quick"));
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

    // Given: generation の 3 フィールドをすべて含む設定 TOML
    // When: GenerationOverridesConfig にパースする
    // Then: 型付きの値として読み取れる
    #[test]
    fn generation_overrides_parse_typed_fields() {
        let doc = r#"
temperature = 0.2
top_p = 0.9
max_tokens = 8192
"#;

        let generation: GenerationOverridesConfig =
            toml::from_str(doc).expect("generation 上書きをパースできる");

        assert_eq!(generation.temperature, Some(0.2));
        assert_eq!(generation.top_p, Some(0.9));
        assert_eq!(generation.max_tokens, Some(8192));
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
models = [{ route = "worker" }]
preset = "worker-appendix"

[agents.worker.generation]
temperature = 0.2
max_tokens = 4096

[agents.worker.categories.quick]
models = [{ route = "worker-quick" }]

[agents.worker.categories.quick.generation]
temperature = 0.9
"#;
        let config: Config = toml::from_str(doc).expect("agents 設定をパースできる");

        let resolved = config
            .agents
            .binding_for("worker", Some("quick"))
            .expect("カテゴリバインディングを解決できる");

        assert_eq!(resolved.head_route(), "worker-quick");
        assert_eq!(resolved.preset.as_deref(), Some("worker-appendix"));
        assert_eq!(resolved.generation.temperature, Some(0.9));
        assert_eq!(resolved.generation.max_tokens, Some(4096));
        assert_eq!(resolved.generation.top_p, None);
    }

    #[test]
    fn reviewer_category_bindings_merge_per_field_and_fall_back_to_base() {
        let config: Config = toml::from_str(
            r#"
[agents.reviewer]
models = [{ route = "review-base" }]
preset = "base-appendix"
[agents.reviewer.generation]
temperature = 0.2
top_p = 0.8
max_tokens = 4096
[agents.reviewer.categories.plan-review]
models = [{ route = "plan-model" }]
[agents.reviewer.categories.plan-review.generation]
temperature = 0.6
[agents.reviewer.categories.tool-execution]
preset = "tool-appendix"
[agents.reviewer.categories.tool-execution.generation]
top_p = 0.9
max_tokens = 2048
"#,
        )
        .unwrap();
        let agents = config.agents;
        let plan = agents.binding_for("reviewer", Some("plan-review")).unwrap();
        assert_eq!(plan.head_route(), "plan-model");
        assert_eq!(plan.preset.as_deref(), Some("base-appendix"));
        assert_eq!(plan.generation.temperature, Some(0.6));
        assert_eq!(plan.generation.top_p, Some(0.8));
        assert_eq!(plan.generation.max_tokens, Some(4096));
        let tool = agents
            .binding_for("reviewer", Some("tool-execution"))
            .unwrap();
        assert_eq!(tool.head_route(), "review-base");
        assert_eq!(tool.preset.as_deref(), Some("tool-appendix"));
        assert_eq!(tool.generation.temperature, Some(0.2));
        assert_eq!(tool.generation.top_p, Some(0.9));
        assert_eq!(tool.generation.max_tokens, Some(2048));
        assert_eq!(
            agents
                .binding_for("reviewer", Some("lesson_review"))
                .unwrap(),
            agents.binding_for("reviewer", None).unwrap()
        );
        let defaults = AgentsConfig::default();
        for category in ["plan-review", "tool-execution"] {
            assert_eq!(
                defaults.binding_for("reviewer", Some(category)).unwrap(),
                defaults.binding_for("reviewer", None).unwrap()
            );
        }
    }

    #[test]
    fn category_bindings_reject_wrong_roles_and_unknown_names() {
        let agents = AgentsConfig::default();
        for (role, category) in [
            ("reviewer", "quick"),
            ("worker", "plan-review"),
            ("worker", "tool-execution"),
            ("planner", "plan-review"),
        ] {
            assert!(matches!(
                agents.binding_for(role, Some(category)),
                Err(ConfigError::CategoryNotAllowedForRole { .. })
            ));
        }
        for role in ["worker", "reviewer"] {
            assert!(matches!(
                agents.binding_for(role, Some("unknown")),
                Err(ConfigError::UnknownCategory { .. })
            ));
        }
    }

    #[test]
    fn reviewer_category_model_refs_are_explicit_and_renamed() {
        let mut agents = AgentsConfig::default();
        for category in ["plan-review", "tool-execution", "lesson_review"] {
            agents.reviewer.categories.insert(
                category.into(),
                CategoryBindingConfig {
                    models: vec![ChainEntryConfig::route("old")],
                    preset: Some("old".into()),
                    ..Default::default()
                },
            );
        }
        let expected_addresses = vec![
            "reviewer.categories.lesson_review",
            "reviewer.categories.plan-review",
            "reviewer.categories.tool-execution",
        ];
        assert_eq!(roles_using("old", &agents), expected_addresses);
        assert_eq!(explicit_refs(&agents).len(), 3);
        rename_route_refs(&mut agents, &[("old".into(), "new".into())].into());
        assert!(roles_using("old", &agents).is_empty());
        assert_eq!(roles_using("new", &agents), expected_addresses);
        assert!(agents.reviewer.models.is_empty());
        for binding in agents.reviewer.categories.values() {
            assert_eq!(binding.preset.as_deref(), Some("old"));
        }
    }

    #[test]
    fn reviewer_bindings_reject_unknown_fields() {
        for doc in [
            "[agents.reviewer]\nprompt = 'not allowed'",
            "[agents.reviewer.categories.plan-review]\nweight = 0.5",
            "[agents.reviewer.categories.plan-review.generation]\nseed = 42",
        ] {
            assert!(toml::from_str::<Config>(doc).is_err(), "{doc}");
        }
    }

    // Given: 既知カテゴリ以外のカテゴリ名 / When: binding_for を呼ぶ
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
    // Then: チェーンはロール名 "worker" の 1 要素になり、preset は None
    #[test]
    fn binding_for_defaults_chain_to_role_name() {
        let agents = AgentsConfig::default();

        let resolved = agents
            .binding_for("worker", None)
            .expect("既定バインディングを解決できる");

        assert_eq!(resolved.head_route(), "worker");
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
models = [{ route = "shared" }]
[agents.explorer]
models = [{ route = "shared" }]
[agents.worker]
models = [{ route = "shared" }]
[agents.worker.categories.quick]
models = [{ route = "shared" }]
[agents.reviewer]
models = [{ route = "shared" }]
[agents.roles.web_researcher]
models = [{ route = "shared" }]
[agents.roles.planner]
models = [{ route = "shared" }]
[agents.roles.advisor]
models = [{ route = "shared" }]
[agents.roles.multimodal_looker]
models = [{ route = "shared" }]
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
                "roles.advisor",
                "roles.multimodal_looker",
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
