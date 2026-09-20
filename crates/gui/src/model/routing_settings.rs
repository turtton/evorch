//! フォールバック順を保持するルーティング編集状態。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::Receiver;

use config::{Config, ConfigError, RouteCandidateConfig, RoutingConfig};

#[derive(Debug, Default)]
pub struct RoutingSettingsModel {
    pub open: bool,
    pub routes: BTreeMap<String, Vec<RouteCandidateConfig>>,
    pub route_users: BTreeMap<String, Vec<String>>,
    pub routes_empty: bool,
    pub pending_new_route: Option<String>,
    pub origin_role_settings: bool,
    pub expanded: BTreeSet<String>,
    pub profile_names: Vec<String>,
    pub profile_defaults: BTreeMap<String, String>,
    pub profile_models: BTreeMap<String, Vec<String>>,
    pub new_route_name: String,
    pub route_name_edits: BTreeMap<String, String>,
    pub validation_error: Option<String>,
    pub(crate) save_rx: Option<Receiver<Result<Config, String>>>,
}

impl RoutingSettingsModel {
    pub fn seed_from_config(config: &Config) -> Self {
        let mut profiles: Vec<_> = config.providers.iter().collect();
        profiles.sort_by_key(|(name, profile)| {
            use config::ProviderTypeConfig as Provider;
            let group = match profile.provider_type {
                Provider::AnthropicSubscription
                | Provider::OpenAiCodex
                | Provider::KimiSubscription => 0,
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
            routes_empty: config.routing.routes.is_empty(),
            profile_names: profiles.into_iter().map(|(name, _)| name.clone()).collect(),
            profile_defaults: config
                .providers
                .iter()
                .map(|(name, profile)| (name.clone(), profile.default_model.clone()))
                .collect(),
            profile_models: config
                .providers
                .iter()
                .map(|(name, profile)| {
                    (
                        name.clone(),
                        profile
                            .models
                            .iter()
                            .map(|model| model.id.clone())
                            .collect(),
                    )
                })
                .collect(),
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
                model.pending_new_route = Some(logical.into());
                model.route_users.insert(
                    logical.into(),
                    config::types::agents::roles_using(logical, &config.agents),
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
            }],
        );
        self.expanded.insert(name.into());
        Ok(())
    }

    pub fn rename_route(&mut self, old: &str, name: &str) -> Result<(), ConfigError> {
        if old == name {
            return Ok(());
        }
        self.validate_name(name)?;
        if let Some(candidates) = self.routes.remove(old) {
            self.routes.insert(name.into(), candidates);
            if self.expanded.remove(old) {
                self.expanded.insert(name.into());
            }
        }
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
        Ok(RoutingConfig { routes: renamed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefill_inserts_named_route_with_first_profile_candidate() {
        // Given: 先頭プロファイルと明示的なロール使用先。
        let mut config = Config::default();
        config
            .providers
            .insert("first".into(), config::ProviderProfileConfig::default());
        config.agents.explorer.logical_model = Some("role-model".into());
        // When: 新しい論理名で設定を初期化する。
        let model = RoutingSettingsModel::seed_from_config_prefill(&config, "role-model");
        // Then: 既定モデルに委譲する候補が1件だけ追加される。
        assert_eq!(
            model.routes["role-model"],
            vec![RouteCandidateConfig {
                profile: "first".into(),
                model: None
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
        config.agents.explorer.logical_model = Some("shared".into());
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
