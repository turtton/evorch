use std::collections::BTreeSet;
use std::sync::mpsc::Receiver;

pub const CATEGORIES: [&str; 6] = [
    "quick",
    "deep",
    "high-reasoning",
    "visual",
    "writing",
    "research",
];

#[derive(Debug, Default)]
pub struct RoleSettingsModel {
    pub open: bool,
    pub agents: config::AgentsConfig,
    pub logical_models: Vec<String>,
    pub error: Option<String>,
    pub(crate) save_rx: Option<Receiver<Result<config::Config, String>>>,
}

impl RoleSettingsModel {
    pub fn seed_from_config(config: &config::Config) -> Self {
        let mut names: BTreeSet<String> = config.routing.routes.keys().cloned().collect();
        if config.routing.routes.is_empty() {
            for profile in config.providers.values() {
                names.extend(
                    profile
                        .models
                        .iter()
                        .filter(|entry| entry.enabled)
                        .map(|entry| entry.id.clone()),
                );
            }
            names.extend(["orchestrator", "explorer", "worker", "reviewer"].map(String::from));
            for (_, binding) in bindings(&config.agents) {
                names.extend(binding.logical_model.clone());
                names.extend(
                    binding
                        .categories
                        .values()
                        .filter_map(|binding| binding.logical_model.clone()),
                );
            }
        }
        Self {
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
            self.validate_model(
                &format!("agents.{role}.logical_model"),
                binding.logical_model.as_deref(),
            )?;
            validate_generation(&binding.generation, role)?;
            for (category, binding) in &binding.categories {
                if !CATEGORIES.contains(&category.as_str()) {
                    return Err(config::ConfigError::UnknownCategory {
                        role: role.into(),
                        category: category.clone(),
                    });
                }
                self.validate_model(
                    &format!("agents.{role}.categories.{category}.logical_model"),
                    binding.logical_model.as_deref(),
                )?;
                validate_generation(
                    &binding.generation,
                    &format!("{role}.categories.{category}"),
                )?;
            }
        }
        Ok(())
    }

    fn validate_model(&self, path: &str, value: Option<&str>) -> Result<(), config::ConfigError> {
        if let Some(value) = value
            && (value.trim().is_empty() || !self.logical_models.iter().any(|name| name == value))
        {
            return Err(config::ConfigError::InvalidField {
                path: path.into(),
                message: "Select a known logical model or inherit the default".into(),
            });
        }
        Ok(())
    }
}

pub fn bindings(agents: &config::AgentsConfig) -> [(&str, &config::RoleBindingConfig); 7] {
    [
        ("orchestrator", &agents.orchestrator),
        ("explorer", &agents.explorer),
        ("worker", &agents.worker),
        ("reviewer", &agents.reviewer),
        ("roles.planner", &agents.roles.planner),
        ("roles.oracle", &agents.roles.oracle),
        ("roles.multimodal_looker", &agents.roles.multimodal_looker),
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
