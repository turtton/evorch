//! ロール構成プロファイル (ロール割り当てとルーティングの名前付きセット) を定義します。

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{AgentsConfig, Config, ProviderProfileConfig, RoutingConfig};

/// トップレベルの `[agents]` / `[routing]` を指す予約済みプロファイル名。
pub const DEFAULT_ROLE_PROFILE: &str = "default";

/// 名前付きのロール構成プロファイル。
///
/// トップレベルの `[agents]` / `[routing]` と同じ形で、プロジェクトは
/// `role_profile = "<name>"` によっていずれかを選択する。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RoleProfileConfig {
    /// ロール別エージェントバインディング。
    pub agents: AgentsConfig,
    /// ルーティング設定。
    pub routing: RoutingConfig,
}

/// プロファイル名が `[a-z0-9_-]{1,64}` を満たすか。
pub fn is_valid_role_profile_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

impl Config {
    /// 選択可能なプロファイル名。先頭は常に [`DEFAULT_ROLE_PROFILE`]。
    pub fn role_profile_names(&self) -> Vec<String> {
        std::iter::once(DEFAULT_ROLE_PROFILE.to_owned())
            .chain(self.role_profiles.keys().cloned())
            .collect()
    }

    /// 指定プロファイルのロール構成。`default` と `None` はトップレベルを返す。
    pub fn role_profile_config(&self, name: Option<&str>) -> Option<RoleProfileConfig> {
        match name {
            None | Some(DEFAULT_ROLE_PROFILE) => Some(RoleProfileConfig {
                agents: self.agents.clone(),
                routing: self.routing.clone(),
            }),
            Some(name) => self.role_profiles.get(name).cloned(),
        }
    }

    /// 選択中のプロファイル名。未選択・未知の名前は `default` として扱う。
    pub fn active_role_profile(&self) -> &str {
        match self.role_profile.as_deref() {
            Some(name) if self.role_profiles.contains_key(name) => name,
            _ => DEFAULT_ROLE_PROFILE,
        }
    }

    /// ロールのチェーンとモデルの種別を検証する ([`Self::chain_problems`] の最初の問題を返す)。
    ///
    /// # Errors
    /// 問題があれば [`crate::ConfigError::InvalidField`] を返す。
    pub fn validate_chains(&self) -> Result<(), crate::ConfigError> {
        self.chain_problems().into_iter().next().map_or(Ok(()), Err)
    }

    /// ロールのチェーンとモデルの種別の問題をすべて返す。
    ///
    /// - チェーンが同じスコープ (トップレベルかロールプロファイル) にない route を参照している。
    ///   暗黙のロール名参照は実行時に解決するため対象外。
    /// - profile の `default_model` が `image_generation` のモデル (ADR 0038)。
    /// - [`scope_kind_problems`] が各スコープで見つける種別の問題。
    ///
    /// 厳格な読み込みは最初の問題で失敗し、通常の読み込みはすべて警告して、該当する
    /// チェーンを使う時点の失敗に委ねる。
    pub fn chain_problems(&self) -> Vec<crate::ConfigError> {
        let scopes: Vec<_> = std::iter::once((String::new(), &self.agents, &self.routing))
            .chain(self.role_profiles.iter().map(|(name, profile)| {
                (
                    format!("role_profiles.{name}."),
                    &profile.agents,
                    &profile.routing,
                )
            }))
            .collect();
        let mut problems = Vec::new();
        for (prefix, agents, routing) in &scopes {
            for (address, route) in super::agents::explicit_refs(agents) {
                if !routing.routes.contains_key(&route) {
                    problems.push(crate::ConfigError::InvalidField {
                        path: format!("{prefix}agents.{address}.models"),
                        message: format!(
                            "unknown route `{route}`; define [[{prefix}routing.routes.{route}]]"
                        ),
                    });
                }
            }
        }
        problems.extend(self.model_kind_problems());
        dedup(problems)
    }

    /// Provider defaults, route homogeneity and chain kind/provider constraints across
    /// all role profiles, independently of unresolved route references during editing.
    pub fn model_kind_problems(&self) -> Vec<crate::ConfigError> {
        let mut problems = Vec::new();
        for (name, profile) in &self.providers {
            if profile.model_kind(&profile.default_model) == super::ModelKind::ImageGeneration {
                problems.push(crate::ConfigError::InvalidField {
                    path: format!("providers.{name}.default_model"),
                    message: format!(
                        "default_model `{}` is an image_generation model; route candidates \
                         without a model send conversations to it, so choose a chat model (ADR 0038)",
                        profile.default_model
                    ),
                });
            }
        }
        for (prefix, agents, routing) in
            std::iter::once((String::new(), &self.agents, &self.routing)).chain(
                self.role_profiles.iter().map(|(name, profile)| {
                    (
                        format!("role_profiles.{name}."),
                        &profile.agents,
                        &profile.routing,
                    )
                }),
            )
        {
            problems.extend(scope_kind_problems(
                &prefix,
                agents,
                routing,
                &self.providers,
            ));
        }
        dedup(problems)
    }

    /// `role_profile` の選択を実効の `agents` / `routing` に反映する。
    ///
    /// 未知の名前は警告してトップレベル (`default`) のまま扱う。
    pub(crate) fn apply_role_profile(&mut self) {
        let Some(name) = self.role_profile.as_deref() else {
            return;
        };
        if name == DEFAULT_ROLE_PROFILE {
            return;
        }
        match self.role_profiles.get(name) {
            Some(profile) => {
                self.agents = profile.agents.clone();
                self.routing = profile.routing.clone();
            }
            None => {
                tracing::warn!(role_profile = %name, "unknown role profile; using default");
            }
        }
    }
}

/// 1 つのスコープ (`prefix` が空ならトップレベル、`role_profiles.<name>.` ならそのプロファイル)
/// の、モデルの種別の問題を返す (ADR 0038)。[`route_mixing_problems`] と
/// [`chain_kind_problems`] を合わせたもの。
///
/// 未定義の route と、profile も `model` もない候補は対象外 (別の検査と実行時の解決が扱う)。
pub fn scope_kind_problems(
    prefix: &str,
    agents: &AgentsConfig,
    routing: &RoutingConfig,
    providers: &BTreeMap<String, ProviderProfileConfig>,
) -> Vec<crate::ConfigError> {
    let mut problems = route_mixing_problems(prefix, routing, providers);
    problems.extend(chain_kind_problems(prefix, agents, routing, providers));
    problems
}

/// 1 つの route に種別の違う候補が混ざっている問題を返す (ADR 0038)。
pub fn route_mixing_problems(
    prefix: &str,
    routing: &RoutingConfig,
    providers: &BTreeMap<String, ProviderProfileConfig>,
) -> Vec<crate::ConfigError> {
    let mut problems = Vec::new();
    for (route, candidates) in &routing.routes {
        if super::routing::route_model_kinds(candidates, providers).len() > 1 {
            let members = candidates
                .iter()
                .filter_map(|candidate| {
                    let kind = candidate.kind(providers)?;
                    let model = candidate.model_id(providers)?;
                    Some(format!("{}/{model}: {kind}", candidate.profile))
                })
                .collect::<Vec<_>>()
                .join(", ");
            problems.push(crate::ConfigError::InvalidField {
                path: format!("{prefix}routing.routes.{route}"),
                message: format!(
                    "route `{route}` mixes chat and image_generation candidates ({members}); \
                     keep one model kind per route (ADR 0038)"
                ),
            });
        }
    }
    problems
}

/// チェーンが用途に合わない候補を使う問題を返す (ADR 0038)。
///
/// - 通常のロール・カテゴリ・compactor のチェーン (チェーンを持たずロール名の route を使う
///   ロールを含む) に `image_generation` の候補がある。
/// - image_generator のチェーンに `chat` の候補、または画像生成に対応しない provider の候補がある。
///
/// GUI のロール設定とルーティング設定の保存前の検査もこの関数を使う。
pub fn chain_kind_problems(
    prefix: &str,
    agents: &AgentsConfig,
    routing: &RoutingConfig,
    providers: &BTreeMap<String, ProviderProfileConfig>,
) -> Vec<crate::ConfigError> {
    let mut problems = Vec::new();
    let explicit = super::agents::explicit_refs(agents)
        .into_iter()
        .map(|(address, route)| (address, route, false));
    let implicit = routing.routes.keys().flat_map(|route| {
        super::agents::implicit_roles_using(route, agents)
            .into_iter()
            .map(move |address| (address, route.clone(), true))
    });
    for (address, route, implicit) in explicit.chain(implicit) {
        let Some(candidates) = routing.routes.get(&route) else {
            continue;
        };
        let purpose = super::ChainPurpose::of_address(&address);
        for candidate in candidates {
            let Some(problem) = purpose.candidate_problem(candidate, providers) else {
                continue;
            };
            let source = if implicit {
                format!("the role has no chain and uses route `{route}` named after it")
            } else {
                format!("route `{route}`")
            };
            problems.push(crate::ConfigError::InvalidField {
                path: format!("{prefix}agents.{address}.models"),
                message: format!("{source}: {problem}; {}", purpose.requirement()),
            });
        }
    }
    dedup(problems)
}

/// 同じ問題 (同じ route をチェーンに 2 度置いた場合など) を初出順に 1 つにまとめる。
fn dedup(problems: Vec<crate::ConfigError>) -> Vec<crate::ConfigError> {
    let mut seen = std::collections::BTreeSet::new();
    problems
        .into_iter()
        .filter(|problem| seen.insert(problem.to_string()))
        .collect()
}
