//! フォールバック順を保持するルーティング編集状態。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::Receiver;

pub use config::RouteCandidateConfig;
use config::{Config, ConfigError, ModelKind, RoutingConfig};

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
    pub routes: BTreeMap<String, Vec<RouteCandidateConfig>>,
    /// The routes as last seeded or saved: edits are the difference from it.
    pub base_routes: BTreeMap<String, Vec<RouteCandidateConfig>>,
    /// Why saving is disabled: an in-app write changed the saved routing under unsaved edits.
    pub conflict: Option<String>,
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
    /// The routes added in this session, in the order they were added (the last is the newest).
    pub added: Vec<String>,
    /// The route whose card is open, which a save or discard is for (Model Settings shows one).
    pub selected: Option<String>,
    /// Routes whose saved candidates were changed elsewhere while they had unsaved edits:
    /// they are not saved over the newer file, only discarded.
    pub conflicted: BTreeSet<String>,
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
    /// The roles and categories whose settings name `route` explicitly, as settings addresses.
    pub fn explicit_users(&self, route: &str) -> Vec<String> {
        config::types::agents::roles_using(route, &self.agents)
    }

    /// The roles and categories that only reach `route` because it is named like the role.
    pub fn implicit_users(&self, route: &str) -> Vec<String> {
        config::types::agents::implicit_roles_using(route, &self.agents)
    }

    /// Whether the candidates serve both chat and image models.
    pub fn mixes_kinds(&self, candidates: &[RouteCandidateConfig]) -> bool {
        config::types::routing::route_model_kinds(candidates, &self.providers).len() > 1
    }

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
            base_routes: config.routing.routes.clone(),
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

    /// Whether the draft differs from what was seeded or last saved, renames included.
    pub fn is_dirty(&self) -> bool {
        self.routes != self.base_routes || !self.route_renames().is_empty()
    }

    /// Whether the route drafted under `key` is new, changed or renamed.
    pub fn route_is_dirty(&self, key: &str) -> bool {
        let renamed = self
            .route_name_edits
            .get(key)
            .is_some_and(|draft| draft != key);
        match (self.routes.get(key), self.base_routes.get(key)) {
            (Some(draft), Some(saved)) => draft != saved || renamed,
            (Some(_), None) => true,
            _ => false,
        }
    }

    /// The keys of the routes with unsaved edits.
    pub fn dirty_routes(&self) -> Vec<String> {
        self.routes
            .keys()
            .filter(|key| self.route_is_dirty(key))
            .cloned()
            .collect()
    }

    /// Whether the route has never been saved (it is not in the saved routes).
    pub fn route_is_new(&self, key: &str) -> bool {
        !self.base_routes.contains_key(key)
    }

    /// The name `key` has now: the rename typed for it, else itself.
    pub fn shown_name<'a>(&'a self, key: &'a str) -> &'a str {
        self.route_name_edits.get(key).map_or(key, String::as_str)
    }

    /// Puts the route back to what is saved (a new one is dropped), leaving the other
    /// routes' edits alone.
    pub fn discard_route(&mut self, key: &str) {
        self.route_name_edits.remove(key);
        self.conflicted.remove(key);
        match self.base_routes.get(key) {
            Some(saved) => {
                self.routes.insert(key.into(), saved.clone());
            }
            None => {
                self.routes.remove(key);
                if self.pending_new_route.as_deref() == Some(key) {
                    self.pending_new_route = None;
                }
            }
        }
        if self.selected.as_deref() == Some(key) && !self.routes.contains_key(key) {
            self.selected = None;
        }
        self.validation_error = None;
    }

    /// Adds a route under a name that is free, to be renamed and saved. It is the open one.
    pub fn add_draft_route(&mut self) -> String {
        let taken = |name: &str| {
            self.routes.contains_key(name)
                || self.route_name_edits.values().any(|draft| draft == name)
        };
        let mut name = String::from("new-route");
        let mut suffix = 2;
        while taken(&name) {
            name = format!("new-route-{suffix}");
            suffix += 1;
        }
        self.add_route(&name).expect("a free name is valid");
        self.pending_new_route = Some(name.clone());
        self.selected = Some(name.clone());
        name
    }

    /// The name draft of `key`, checked against the other routes and drafts.
    ///
    /// # Errors
    /// The name is blank, or another route or draft has it.
    pub fn set_route_name(&mut self, key: &str, name: &str) -> Result<(), ConfigError> {
        let previous = self.route_name_edits.remove(key);
        let name = name.trim();
        if name == key {
            return Ok(());
        }
        let result = self.rename_route(key, name);
        if result.is_err()
            && let Some(previous) = previous
        {
            self.route_name_edits.insert(key.into(), previous);
        }
        result
    }

    /// Takes the saved-state fields of `config` (a profile view) and leaves the draft alone.
    pub fn refresh_references(&mut self, config: &Config) {
        let fresh = Self::seed_from_config(config);
        self.route_users = fresh.route_users;
        self.implicit_route_users = fresh.implicit_route_users;
        self.profile_names = fresh.profile_names;
        self.profile_models = fresh.profile_models;
        self.providers = fresh.providers;
        self.agents = fresh.agents;
    }

    pub fn seed_from_config_prefill(config: &Config, logical: &str) -> Self {
        let mut model = Self::seed_from_config(config);
        model.add_prefill_route(logical, &config.agents);
        model
    }

    /// Adds `logical` as the route a role binding asks for and returns to roles after saving.
    /// An existing route is shown instead of duplicated.
    pub fn add_prefill_route(&mut self, logical: &str, agents: &config::AgentsConfig) {
        self.origin_role_settings = true;
        if self.routes.contains_key(logical) {
            self.expanded.insert(logical.into());
            return;
        }
        match self.add_route(logical) {
            Ok(()) => {
                // A route created for the image generator starts on an image model.
                if agents
                    .image_generator_chain()
                    .iter()
                    .any(|entry| entry.route == logical)
                    && let Some(candidate) = self.image_candidate()
                {
                    self.routes.insert(logical.into(), vec![candidate]);
                }
                self.pending_new_route = Some(logical.into());
                self.route_users.insert(
                    logical.into(),
                    config::types::agents::roles_using(logical, agents),
                );
                self.implicit_route_users.insert(
                    logical.into(),
                    config::types::agents::implicit_roles_using(logical, agents),
                );
            }
            Err(error) => self.validation_error = Some(error.to_string()),
        }
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
        self.added.push(name.into());
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
        self.choices_for_profile(&candidate.profile, self.route_kind(route, Some(index)))
    }

    fn choices_for_profile(&self, profile: &str, kind: Option<ModelKind>) -> ModelChoices {
        let fits =
            |model: &str| kind.is_none_or(|kind| self.model_kind(profile, model) == Some(kind));
        ModelChoices {
            models: self
                .profile_models
                .get(profile)
                .into_iter()
                .flatten()
                .filter(|model| fits(model))
                .cloned()
                .collect(),
            profile_default: self
                .providers
                .get(profile)
                .is_some_and(|profile| fits(&profile.default_model)),
        }
    }

    /// Changes the profile only if it can serve the route's kind. A missing fitting
    /// override is not `None`: that would silently select an incompatible default.
    /// Returns whether the change was accepted, leaving the candidate intact on failure.
    pub fn change_candidate_profile(&mut self, route: &str, index: usize, profile: &str) -> bool {
        let Some(original) = self.routes.get(route).and_then(|list| list.get(index)) else {
            return false;
        };
        let kind = self
            .route_kind(route, Some(index))
            .or_else(|| original.kind(&self.providers));
        let choices = self.choices_for_profile(profile, kind);
        let model = if original
            .model
            .as_ref()
            .map_or(choices.profile_default, |model| {
                choices.models.contains(model)
            }) {
            original.model.clone()
        } else if choices.profile_default {
            None
        } else if let Some(model) = choices.models.first() {
            Some(model.clone())
        } else {
            return false;
        };
        let candidate = &mut self.routes.get_mut(route).expect("route exists")[index];
        candidate.profile = profile.into();
        candidate.model = model;
        true
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

    /// The name and candidates `key` would be saved with: its draft alone, checked against the
    /// saved routes (`saved`) and the names the other drafts have, so another route's unsaved
    /// edits cannot stop this one.
    ///
    /// # Errors
    /// The name is blank or taken, there is no candidate, or a candidate has no existing profile.
    pub fn validated_route(
        &self,
        key: &str,
        saved: &BTreeMap<String, Vec<RouteCandidateConfig>>,
    ) -> Result<(String, Vec<RouteCandidateConfig>), ConfigError> {
        let name = self.shown_name(key).trim().to_owned();
        let draft = self
            .routes
            .get(key)
            .ok_or_else(|| ConfigError::InvalidField {
                path: format!("routing.routes.{key}"),
                message: "This route no longer exists".into(),
            })?;
        let taken = (name != key && saved.contains_key(&name))
            || self
                .routes
                .keys()
                .any(|other| other != key && self.shown_name(other) == name);
        if name.is_empty() || taken {
            return Err(ConfigError::InvalidField {
                path: "routing.routes".into(),
                message: "Enter a non-blank, unique logical model name".into(),
            });
        }
        if draft.is_empty() {
            return Err(ConfigError::InvalidField {
                path: format!("routing.routes.{name}"),
                message: "Each route needs a logical model name and at least one candidate".into(),
            });
        }
        let mut candidates = draft.clone();
        for candidate in &mut candidates {
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
        Ok((name, candidates))
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
        // When: 重複名をプリフィルする。
        let model = RoutingSettingsModel::seed_from_config_prefill(&config, "existing");
        // Then: 既存ルートを重複追加せず展開して見せる。
        assert_eq!(model.routes, config.routing.routes);
        assert_eq!(model.pending_new_route, None);
        assert!(model.validation_error.is_none());
        assert!(model.expanded.contains("existing"));
        // When: 空白名をプリフィルする。
        let model = RoutingSettingsModel::seed_from_config_prefill(&config, " ");
        // Then: 既存ルートを壊さず検証エラーを表示する。
        assert_eq!(model.routes, config.routing.routes);
        assert_eq!(model.pending_new_route, None);
        assert!(model.validation_error.is_some());
    }
}
