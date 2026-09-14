//! フォールバック順を保持するルーティング編集状態。

use std::collections::BTreeMap;
use std::sync::mpsc::Receiver;

use config::{Config, ConfigError, RouteCandidateConfig, RoutingConfig};

#[derive(Debug, Default)]
pub struct RoutingSettingsModel {
    pub open: bool,
    pub routes: BTreeMap<String, Vec<RouteCandidateConfig>>,
    pub profile_names: Vec<String>,
    pub profile_models: BTreeMap<String, Vec<String>>,
    pub new_route_name: String,
    pub route_name_edits: BTreeMap<String, String>,
    pub validation_error: Option<String>,
    pub(crate) save_rx: Option<Receiver<Result<Config, String>>>,
}

impl RoutingSettingsModel {
    pub fn seed_from_config(config: &Config) -> Self {
        Self {
            routes: config.routing.routes.clone(),
            profile_names: config.providers.keys().cloned().collect(),
            profile_models: config
                .providers
                .iter()
                .map(|(name, profile)| {
                    (
                        name.clone(),
                        profile
                            .models
                            .iter()
                            .filter(|model| model.enabled)
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

    pub fn add_route(&mut self, name: &str) -> Result<(), ConfigError> {
        self.validate_name(name)?;
        self.routes.insert(
            name.into(),
            vec![RouteCandidateConfig {
                profile: self.profile_names.first().cloned().unwrap_or_default(),
                model: None,
            }],
        );
        Ok(())
    }

    pub fn rename_route(&mut self, old: &str, name: &str) -> Result<(), ConfigError> {
        if old == name {
            return Ok(());
        }
        self.validate_name(name)?;
        if let Some(candidates) = self.routes.remove(old) {
            self.routes.insert(name.into(), candidates);
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
