use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::Receiver;

/// 設定用の共通定義からカテゴリを取得する。shell 監査専用の reviewer 設定も含む。
pub fn categories_for_role(
    role: &str,
) -> impl Iterator<Item = config::agent_categories::SettingsCategory> + '_ {
    config::agent_categories::settings_categories().filter(move |category| category.role == role)
}

#[derive(Debug, Default)]
pub struct RoleSettingsModel {
    pub open: bool,
    pub agents: config::AgentsConfig,
    pub logical_models: Vec<String>,
    pub route_names: BTreeSet<String>,
    /// route ごとに、チェーン要素で選べる推論強度。
    pub route_efforts: BTreeMap<String, Vec<String>>,
    pub routes_empty: bool,
    pub resolved_previews: BTreeMap<String, Option<String>>,
    pub error: Option<String>,
    pub profiles: super::role_profiles::RoleProfilePicker,
    pub(crate) job: super::role_profiles::RoleProfileJob,
    pub(crate) save_rx: Option<Receiver<Result<config::Config, String>>>,
}

impl RoleSettingsModel {
    pub fn seed_from_config(config: &config::Config) -> Self {
        let names: BTreeSet<String> = config
            .routing
            .routes
            .keys()
            .cloned()
            .chain(
                config::types::agents::explicit_refs(&config.agents)
                    .into_iter()
                    .map(|(_, name)| name),
            )
            .collect();
        Self {
            route_names: config.routing.routes.keys().cloned().collect(),
            route_efforts: config
                .routing
                .routes
                .keys()
                .map(|route| {
                    (
                        route.clone(),
                        super::routing_settings::route_effort_choices(config, route),
                    )
                })
                .collect(),
            routes_empty: config.routing.routes.is_empty(),
            agents: config.agents.clone(),
            logical_models: names.into_iter().collect(),
            ..Self::default()
        }
    }

    pub const fn is_saving(&self) -> bool {
        self.save_rx.is_some()
    }

    pub fn validate(&self) -> Result<(), config::ConfigError> {
        for (role, binding) in bindings(&self.agents) {
            Self::validate_chain(&format!("agents.{role}.models"), &binding.models)?;
            validate_generation(&binding.generation, role)?;
        }
        for (role, categories) in [
            ("worker", &self.agents.worker.categories),
            ("reviewer", &self.agents.reviewer.categories),
        ] {
            for (category, binding) in categories {
                // 設定側で許可される内部カテゴリも保存時に検証する。
                self.agents.binding_for(role, Some(category))?;
                Self::validate_chain(
                    &format!("agents.{role}.categories.{category}.models"),
                    &binding.models,
                )?;
                validate_generation(
                    &binding.generation,
                    &format!("{role}.categories.{category}"),
                )?;
            }
        }
        Ok(())
    }

    fn validate_chain(
        path: &str,
        models: &[config::ChainEntryConfig],
    ) -> Result<(), config::ConfigError> {
        if models.iter().any(|entry| entry.route.trim().is_empty()) {
            return Err(config::ConfigError::InvalidField {
                path: path.into(),
                message: "Enter a non-blank route or inherit the default".into(),
            });
        }
        Ok(())
    }
}

pub fn bindings(agents: &config::AgentsConfig) -> [(&str, &config::RoleBindingConfig); 9] {
    [
        ("orchestrator", &agents.orchestrator),
        ("explorer", &agents.explorer),
        ("worker", &agents.worker),
        ("reviewer", &agents.reviewer.base),
        ("roles.web_researcher", &agents.roles.web_researcher),
        ("roles.planner", &agents.roles.planner),
        ("roles.advisor", &agents.roles.advisor),
        ("roles.multimodal_looker", &agents.roles.multimodal_looker),
        ("roles.compactor", &agents.roles.compactor),
    ]
}

fn validate_generation(
    value: &config::GenerationOverridesConfig,
    path: &str,
) -> Result<(), config::ConfigError> {
    for (name, valid) in [
        (
            "temperature",
            value
                .temperature
                .is_none_or(|v| v.is_finite() && (0.0..=2.0).contains(&v)),
        ),
        (
            "top_p",
            value
                .top_p
                .is_none_or(|v| v.is_finite() && (0.0..=1.0).contains(&v)),
        ),
        ("max_tokens", value.max_tokens != Some(0)),
    ] {
        if !valid {
            return Err(config::ConfigError::InvalidField {
                path: format!("agents.{path}.generation.{name}"),
                message: "Generation override is out of range".into(),
            });
        }
    }
    Ok(())
}
