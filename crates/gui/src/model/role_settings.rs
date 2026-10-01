use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::Receiver;

/// 公開委譲カテゴリを設定側の共通定義から取得する。内部カテゴリは編集欄に出さない。
pub fn categories_for_role(
    role: &str,
) -> impl Iterator<Item = config::agent_categories::PublicCategory> + '_ {
    config::agent_categories::public_categories().filter(move |category| category.role == role)
}

/// モデルに effort_levels が未設定のときに提示する共通の推論強度一覧。
pub const DEFAULT_EFFORT_LEVELS: [&str; 7] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Default)]
pub struct RoleSettingsModel {
    pub open: bool,
    pub agents: config::AgentsConfig,
    pub logical_models: Vec<String>,
    pub route_names: BTreeSet<String>,
    pub routes_empty: bool,
    pub resolved_previews: BTreeMap<String, Option<String>>,
    pub effort_choices: BTreeMap<String, Vec<String>>,
    pub error: Option<String>,
    pub(crate) save_rx: Option<Receiver<Result<config::Config, String>>>,
}

/// 論理モデル名から提示する推論強度の選択肢を引く。未登録なら共通既定一覧。
pub fn effort_options(
    choices: &BTreeMap<String, Vec<String>>,
    logical_model: Option<&str>,
) -> Vec<String> {
    logical_model
        .and_then(|name| choices.get(name))
        .cloned()
        .unwrap_or_else(|| {
            DEFAULT_EFFORT_LEVELS
                .map(str::to_owned)
                .into_iter()
                .collect()
        })
}

fn effort_choices_for(config: &config::Config, name: &str) -> Vec<String> {
    let entry = if let Some(candidates) = config.routing.routes.get(name) {
        candidates.first().and_then(|candidate| {
            let profile = config.providers.get(&candidate.profile)?;
            let model = candidate.model.as_deref().unwrap_or(&profile.default_model);
            profile.models.iter().find(|entry| entry.id == model)
        })
    } else {
        config
            .providers
            .values()
            .find_map(|profile| profile.models.iter().find(|entry| entry.id == name))
    };
    entry
        .and_then(|entry| entry.effort_levels.clone())
        .unwrap_or_else(|| {
            DEFAULT_EFFORT_LEVELS
                .map(str::to_owned)
                .into_iter()
                .collect()
        })
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
            routes_empty: config.routing.routes.is_empty(),
            agents: config.agents.clone(),
            logical_models: names.iter().cloned().collect(),
            effort_choices: names
                .iter()
                .map(|name| (name.clone(), effort_choices_for(config, name)))
                .collect(),
            ..Self::default()
        }
    }

    pub const fn is_saving(&self) -> bool {
        self.save_rx.is_some()
    }

    pub fn validate(&self) -> Result<(), config::ConfigError> {
        for (role, binding) in bindings(&self.agents) {
            Self::validate_model(
                &format!("agents.{role}.logical_model"),
                binding.logical_model.as_deref(),
            )?;
            validate_generation(&binding.generation, role)?;
        }
        for (role, categories) in [
            ("worker", &self.agents.worker.categories),
            ("reviewer", &self.agents.reviewer.categories),
        ] {
            for (category, binding) in categories {
                // 設定側で許可される内部カテゴリも保存時に検証する。
                self.agents.binding_for(role, Some(category))?;
                Self::validate_model(
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

    fn validate_model(path: &str, value: Option<&str>) -> Result<(), config::ConfigError> {
        if let Some(value) = value
            && value.trim().is_empty()
        {
            return Err(config::ConfigError::InvalidField {
                path: path.into(),
                message: "Enter a non-blank logical model or inherit the default".into(),
            });
        }
        Ok(())
    }
}

pub fn bindings(agents: &config::AgentsConfig) -> [(&str, &config::RoleBindingConfig); 8] {
    [
        ("orchestrator", &agents.orchestrator),
        ("explorer", &agents.explorer),
        ("worker", &agents.worker),
        ("reviewer", &agents.reviewer.base),
        ("roles.web_researcher", &agents.roles.web_researcher),
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
