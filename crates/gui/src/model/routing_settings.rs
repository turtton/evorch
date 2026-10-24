//! フォールバック順を保持するルーティング編集状態。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::Receiver;

use config::{Config, ConfigError, ModelKind, RouteCandidateConfig, RoutingConfig};

/// The model overrides one route candidate can pick (ADR 0038).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelChoices {
    /// Enabled models of the candidate's profile with the route's kind.
    pub models: Vec<String>,
    /// Whether the profile's default model has the route's kind.
    pub profile_default: bool,
}

#[derive(Debug, Default)]
pub struct RoutingSettingsModel {
    pub open: bool,
    pub routes: BTreeMap<String, Vec<RouteCandidateConfig>>,
    /// 保存時に `[routing]` を置き換えても失わないよう、読み込んだ値を保持する。
    pub cooldown: config::CooldownConfig,
    /// `cooldown` と同じく、保存で失わないよう読み込んだ値を保持する。
    pub fallback: config::FallbackConfig,
    pub route_users: BTreeMap<String, Vec<String>>,
    pub implicit_route_users: BTreeMap<String, Vec<String>>,
    pub routes_empty: bool,
    pub pending_new_route: Option<String>,
    pub origin_role_settings: bool,
    pub expanded: BTreeSet<String>,
    pub profile_names: Vec<String>,
    pub profile_models: BTreeMap<String, Vec<String>>,
    /// 候補の種別 (ADR 0038) を決めるプロバイダ設定。
    pub providers: BTreeMap<String, config::ProviderProfileConfig>,
    /// 保存前に、route を使うチェーンの種別を検査するための agents (編集前の route 名で参照する)。
    pub agents: config::AgentsConfig,
    pub new_route_name: String,
    pub route_name_edits: BTreeMap<String, String>,
    pub validation_error: Option<String>,
    pub profiles: super::role_profiles::RoleProfilePicker,
    pub(crate) job: super::role_profiles::RoleProfileJob,
    pub(crate) save_rx: Option<Receiver<Result<Config, String>>>,
}

/// route の候補の実モデル (上書き、なければ profile 既定) で選べる推論強度の和集合。
///
/// 推論強度はロールのチェーン要素で指定する (ADR 0030)。汎用の推論強度を受け付けない
/// provider の候補は寄与しない。
pub fn route_effort_choices(config: &Config, route: &str) -> Vec<String> {
    let mut choices = Vec::new();
    for candidate in config.routing.routes.get(route).into_iter().flatten() {
        let Some(profile) = config.providers.get(&candidate.profile) else {
            continue;
        };
        if matches!(
            profile.provider_type,
            config::ProviderTypeConfig::Anthropic | config::ProviderTypeConfig::Cursor
        ) {
            continue;
        }
        let model = candidate.model.as_ref().unwrap_or(&profile.default_model);
        let levels = profile.effort_levels_by_model();
        let configured_levels = levels.get(model).map(Vec::as_slice);
        let candidate_choices =
            if profile.provider_type == config::ProviderTypeConfig::AnthropicSubscription {
                super::effort::claude_effort_choices(model, configured_levels)
            } else {
                super::effort::effort_choices(configured_levels)
            };
        for effort in candidate_choices {
            if !choices.contains(&effort) {
                choices.push(effort);
            }
        }
    }
    choices
}

impl RoutingSettingsModel {
    pub fn seed_from_config(config: &Config) -> Self {
        let mut profiles: Vec<_> = config.providers.iter().collect();
        profiles.sort_by_key(|(name, profile)| {
            use config::ProviderTypeConfig as Provider;
            let group = match profile.provider_type {
                Provider::AnthropicSubscription
                | Provider::OpenAiCodex
                | Provider::KimiSubscription
                | Provider::Cursor => 0,
                Provider::Anthropic
                | Provider::OpenAi
                | Provider::GithubCopilot
                | Provider::Openrouter
                | Provider::OpenAiCompatible => 1,
            };
            (group, *name)
        });
        Self {
            routes: config.routing.routes.clone(),
            cooldown: config.routing.cooldown.clone(),
            fallback: config.routing.fallback.clone(),
            route_users: config
                .routing
                .routes
                .keys()
                .map(|name| {
                    (
                        name.clone(),
                        config::types::agents::roles_using(name, &config.agents),
                    )
                })
                .collect(),
            implicit_route_users: config
                .routing
                .routes
                .keys()
                .map(|name| {
                    (
                        name.clone(),
                        config::types::agents::implicit_roles_using(name, &config.agents),
                    )
                })
                .collect(),
            routes_empty: config.routing.routes.is_empty(),
            profile_names: profiles.into_iter().map(|(name, _)| name.clone()).collect(),
            profile_models: config
                .providers
                .iter()
                .map(|(name, profile)| (name.clone(), profile.enabled_model_ids()))
                .collect(),
            providers: config.providers.clone(),
            agents: config.agents.clone(),
            ..Self::default()
        }
    }

    pub const fn is_saving(&self) -> bool {
        self.save_rx.is_some()
    }

    pub fn seed_from_config_prefill(config: &Config, logical: &str) -> Self {
        let mut model = Self::seed_from_config(config);
        model.origin_role_settings = true;
        match model.add_route(logical) {
            Ok(()) => {
                // A route created for the image generator starts on an image model.
                if config
                    .agents
                    .image_generator_chain()
                    .iter()
                    .any(|entry| entry.route == logical)
                    && let Some(candidate) = model.image_candidate()
                {
                    model.routes.insert(logical.into(), vec![candidate]);
                }
                model.pending_new_route = Some(logical.into());
                model.route_users.insert(
                    logical.into(),
                    config::types::agents::roles_using(logical, &config.agents),
                );
                model.implicit_route_users.insert(
                    logical.into(),
                    config::types::agents::implicit_roles_using(logical, &config.agents),
                );
            }
            Err(error) => model.validation_error = Some(error.to_string()),
        }
        model
    }

    pub fn add_route(&mut self, name: &str) -> Result<(), ConfigError> {
        self.validate_name(name)?;
        self.routes.insert(
            name.into(),
            vec![RouteCandidateConfig {
                profile: self.profile_names.first().cloned().unwrap_or_default(),
                model: None,
                auto: false,
            }],
        );
        self.expanded.insert(name.into());
        Ok(())
    }

    /// The kind of `model` served by `profile`: its registration, else its ID (ADR 0038).
    fn model_kind(&self, profile: &str, model: &str) -> Option<ModelKind> {
        RouteCandidateConfig {
            profile: profile.into(),
            model: Some(model.into()),
            auto: false,
        }
        .kind(&self.providers)
    }

    /// The kind every candidate of `route` except `skip` shares, when they agree.
    pub fn route_kind(&self, route: &str, skip: Option<usize>) -> Option<ModelKind> {
        let others: Vec<_> = self
            .routes
            .get(route)
            .into_iter()
            .flatten()
            .enumerate()
            .filter(|(index, _)| Some(*index) != skip)
            .map(|(_, candidate)| candidate.clone())
            .collect();
        let kinds = config::types::routing::route_model_kinds(&others, &self.providers);
        match kinds.len() {
            1 => kinds.first().copied(),
            _ => None,
        }
    }

    /// The model overrides candidate `index` of `route` can pick: enabled models of its
    /// profile with the kind the other candidates share, so a route never mixes kinds.
    pub fn model_choices(&self, route: &str, index: usize) -> ModelChoices {
        let Some(candidate) = self.routes.get(route).and_then(|list| list.get(index)) else {
            return ModelChoices::default();
        };
        let kind = self.route_kind(route, Some(index));
        let fits = |model: &str| {
            kind.is_none_or(|kind| self.model_kind(&candidate.profile, model) == Some(kind))
        };
        ModelChoices {
            models: self
                .profile_models
                .get(&candidate.profile)
                .into_iter()
                .flatten()
                .filter(|model| fits(model))
                .cloned()
                .collect(),
            profile_default: self
                .providers
                .get(&candidate.profile)
                .is_none_or(|profile| fits(&profile.default_model)),
        }
    }

    /// After candidate `index` of `route` moved to another profile, keeps its model
    /// override only when the new profile offers it with the route's kind. Otherwise the
    /// profile default is used, or the first fitting model when the default does not fit.
    pub fn refit_candidate(&mut self, route: &str, index: usize) {
        let choices = self.model_choices(route, index);
        let Some(candidate) = self
            .routes
            .get_mut(route)
            .and_then(|list| list.get_mut(index))
        else {
            return;
        };
        let fits = candidate
            .model
            .as_ref()
            .map_or(choices.profile_default, |model| {
                choices.models.contains(model)
            });
        if !fits {
            candidate.model = if choices.profile_default {
                None
            } else {
                choices.models.first().cloned()
            };
        }
    }

    /// The candidate "Add candidate" appends to `route`: the first profile's default, or
    /// an image model when the route already serves image models.
    pub fn new_candidate(&self, route: &str) -> RouteCandidateConfig {
        if self.route_kind(route, None) == Some(ModelKind::ImageGeneration)
            && let Some(candidate) = self.image_candidate()
        {
            return candidate;
        }
        RouteCandidateConfig {
            profile: self.profile_names.first().cloned().unwrap_or_default(),
            model: None,
            auto: false,
        }
    }

    /// The first enabled image model, preferring profiles that can generate images.
    fn image_candidate(&self) -> Option<RouteCandidateConfig> {
        let supported = |profile: &String| {
            self.providers
                .get(profile)
                .is_some_and(|config| config.provider_type.supports_image_generation())
        };
        let (preferred, others): (Vec<_>, Vec<_>) = self
            .profile_names
            .iter()
            .partition(|profile| supported(profile));
        preferred.into_iter().chain(others).find_map(|profile| {
            let model = self.profile_models.get(profile)?.iter().find(|model| {
                self.model_kind(profile, model) == Some(ModelKind::ImageGeneration)
            })?;
            Some(RouteCandidateConfig {
                profile: profile.clone(),
                model: Some(model.clone()),
                auto: false,
            })
        })
    }

    pub fn rename_route(&mut self, old: &str, name: &str) -> Result<(), ConfigError> {
        if old == name {
            return Ok(());
        }
        self.validate_name(name)?;
        self.route_name_edits
            .insert(old.to_string(), name.to_string());
        Ok(())
    }

    fn validate_name(&self, name: &str) -> Result<(), ConfigError> {
        if name.trim().is_empty()
            || self.routes.contains_key(name)
            || self.route_name_edits.values().any(|draft| draft == name)
        {
            return Err(ConfigError::InvalidField {
                path: "routing.routes".into(),
                message: "Enter a non-blank, unique logical model name".into(),
            });
        }
        Ok(())
    }

    /// 編集前の論理名から変更後の論理名への対応。未変更の名前は含めない。
    pub fn route_renames(&self) -> BTreeMap<String, String> {
        self.route_name_edits
            .iter()
            .filter(|(original, draft)| original != draft)
            .map(|(original, draft)| (original.clone(), draft.clone()))
            .collect()
    }

    /// 保存境界で検証し、空白だけのモデル指定を省略へ正規化する。
    pub fn validated_routing(&self) -> Result<RoutingConfig, ConfigError> {
        let mut routes = self.routes.clone();
        for (name, candidates) in &mut routes {
            if name.trim().is_empty() || candidates.is_empty() {
                return Err(ConfigError::InvalidField {
                    path: format!("routing.routes.{name}"),
                    message: "Each route needs a logical model name and at least one candidate"
                        .into(),
                });
            }
            for candidate in candidates {
                if candidate.profile.trim().is_empty()
                    || !self.profile_names.contains(&candidate.profile)
                {
                    return Err(ConfigError::InvalidField {
                        path: format!("routing.routes.{name}.profile"),
                        message: "Select an existing provider profile".into(),
                    });
                }
                if candidate
                    .model
                    .as_ref()
                    .is_some_and(|model| model.trim().is_empty())
                {
                    candidate.model = None;
                }
            }
        }
        // Kinds are checked under the names the chains still use, before renaming.
        let unrenamed = RoutingConfig {
            routes: routes.clone(),
            ..RoutingConfig::default()
        };
        if let Some(problem) = config::route_mixing_problems("", &unrenamed, &self.providers)
            .into_iter()
            .chain(config::chain_kind_problems(
                "",
                &self.agents,
                &unrenamed,
                &self.providers,
            ))
            .next()
        {
            return Err(problem);
        }
        let mut renamed = BTreeMap::new();
        for (name, candidates) in routes {
            let name = self.route_name_edits.get(&name).cloned().unwrap_or(name);
            if name.trim().is_empty() || renamed.insert(name.clone(), candidates).is_some() {
                return Err(ConfigError::InvalidField {
                    path: "routing.routes".into(),
                    message: "Enter a non-blank, unique logical model name".into(),
                });
            }
        }
        Ok(RoutingConfig {
            routes: renamed,
            cooldown: self.cooldown.clone(),
            fallback: self.fallback.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_renames_is_empty_without_edits() {
        let model = RoutingSettingsModel::default();
        assert!(model.route_renames().is_empty());
    }

    #[test]
    fn route_renames_skips_identity_edits() {
        let model = RoutingSettingsModel {
            route_name_edits: [("worker".into(), "worker".into())].into(),
            ..Default::default()
        };
        assert!(model.route_renames().is_empty());
    }

    #[test]
    fn route_renames_includes_only_real_renames() {
        let model = RoutingSettingsModel {
            route_name_edits: [
                ("unchanged".into(), "unchanged".into()),
                ("old".into(), "new".into()),
                ("new".into(), "old".into()),
            ]
            .into(),
            ..Default::default()
        };
        assert_eq!(
            model.route_renames(),
            [("old".into(), "new".into()), ("new".into(), "old".into())].into()
        );
    }

    #[test]
    fn prefill_inserts_named_route_with_first_profile_candidate() {
        // Given: 先頭プロファイルと明示的なロール使用先。
        let mut config = Config::default();
        config
            .providers
            .insert("first".into(), config::ProviderProfileConfig::default());
        config.agents.explorer.models = vec![config::ChainEntryConfig::route("role-model")];
        // When: 新しい論理名で設定を初期化する。
        let model = RoutingSettingsModel::seed_from_config_prefill(&config, "role-model");
        // Then: 既定モデルに委譲する候補が1件だけ追加される。
        assert_eq!(
            model.routes["role-model"],
            vec![RouteCandidateConfig {
                profile: "first".into(),
                model: None,
                auto: false,
            }]
        );
        assert_eq!(model.pending_new_route.as_deref(), Some("role-model"));
        assert_eq!(model.route_users["role-model"], vec!["explorer"]);
        assert_eq!(
            model.validated_routing().expect("routing").routes,
            model.routes
        );
    }

    #[test]
    fn seed_lists_roles_for_existing_routes() {
        // Given: 既存ルートとその論理名を使用するロール。
        let mut config = Config::default();
        config
            .routing
            .routes
            .insert("shared".into(), vec![RouteCandidateConfig::default()]);
        config.agents.explorer.models = vec![config::ChainEntryConfig::route("shared")];
        // When: 設定を初期化する。
        let model = RoutingSettingsModel::seed_from_config(&config);
        // Then: 使用ロールと明示ルートの存在が反映される。
        assert_eq!(model.route_users["shared"], vec!["explorer"]);
        assert!(!model.routes_empty);
    }

    #[test]
    fn prefill_rejects_blank_and_preserves_existing_route() {
        // Given: 既存ルートを持つ設定。
        let mut config = Config::default();
        config.routing.routes.insert(
            "existing".into(),
            vec![RouteCandidateConfig {
                profile: "original".into(),
                model: Some("custom".into()),
                auto: false,
            }],
        );
        // When: 重複名または空白名をプリフィルする。
        for name in ["existing", " "] {
            let model = RoutingSettingsModel::seed_from_config_prefill(&config, name);
            // Then: 既存ルートを壊さず検証エラーを表示する。
            assert_eq!(model.routes, config.routing.routes);
            assert_eq!(model.pending_new_route, None);
            assert!(model.validation_error.is_some());
        }
    }
}
