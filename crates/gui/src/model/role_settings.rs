use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::Receiver;

// The Core-client screens name these through the model, which owns the settings types.
pub use config::{CategoryBindingConfig, ChainEntryConfig, ChainPurpose, RoleBindingConfig};

/// 設定用の共通定義からカテゴリを取得する。shell 監査専用の reviewer 設定も含む。
pub fn categories_for_role(
    role: &str,
) -> impl Iterator<Item = config::agent_categories::SettingsCategory> + '_ {
    config::agent_categories::settings_categories().filter(move |category| category.role == role)
}

#[derive(Debug, Default)]
pub struct RoleSettingsModel {
    pub agents: config::AgentsConfig,
    /// The agents as last seeded or saved: edits are the difference from it.
    pub base_agents: config::AgentsConfig,
    pub logical_models: Vec<String>,
    pub route_names: BTreeSet<String>,
    /// route ごとに、チェーン要素で選べる推論強度。
    pub route_efforts: BTreeMap<String, Vec<String>>,
    /// 入力の routing スコープの route 表。チェーンの候補の種別 (ADR 0038) を決める。
    pub routes: BTreeMap<String, Vec<config::RouteCandidateConfig>>,
    /// route の候補の種別を決めるプロバイダ設定。
    pub providers: BTreeMap<String, config::ProviderProfileConfig>,
    pub routes_empty: bool,
    pub resolved_previews: BTreeMap<String, Option<String>>,
    pub error: Option<String>,
    /// The role (see [`ROLES`]) whose detail is shown, and the one a save or discard is for.
    pub selected: Option<&'static str>,
    /// Roles whose saved bindings were changed elsewhere while they had unsaved edits: they
    /// are not saved over the newer file, only discarded.
    pub conflicted: BTreeSet<&'static str>,
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
            routes: config.routing.routes.clone(),
            providers: config.providers.clone(),
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
            base_agents: config.agents.clone(),
            logical_models: names.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Whether the draft differs from what was seeded or last saved.
    pub fn is_dirty(&self) -> bool {
        self.agents != self.base_agents
    }

    /// Whether `role`'s chain, preset, generation or categories differ from the saved ones.
    pub fn role_is_dirty(&self, role: &RoleInfo) -> bool {
        !role_eq(&self.agents, &self.base_agents, role.address)
    }

    pub fn dirty_roles(&self) -> Vec<&'static RoleInfo> {
        ROLES
            .iter()
            .filter(|role| self.role_is_dirty(role))
            .collect()
    }

    /// Puts `role` back to what is saved, leaving every other role's edits alone.
    pub fn discard_role(&mut self, role: &RoleInfo) {
        copy_role(&mut self.agents, &self.base_agents, role.address);
        self.conflicted.remove(role.address);
        self.error = None;
    }

    /// The route a chain head names, or the role's own name when it has no chain (the
    /// implicit lookup), as `AgentsConfig::binding_for` resolves it.
    pub fn effective_route<'a>(&'a self, role: &'a RoleInfo) -> &'a str {
        parts(&self.agents, role.address)
            .0
            .head_route()
            .unwrap_or(role.key)
    }

    /// What a route resolves to now: its first candidate, as `profile · model`.
    pub fn resolved_target(&self, route: &str) -> Option<String> {
        let candidate = self.routes.get(route)?.first()?;
        let model = candidate.model.clone().or_else(|| {
            self.providers
                .get(&candidate.profile)
                .map(|profile| profile.default_model.clone())
        });
        Some(match model {
            Some(model) if !model.is_empty() => format!("{} · {model}", candidate.profile),
            _ => candidate.profile.clone(),
        })
    }

    /// Takes the route and provider fields of `config` (a profile view) and leaves the
    /// agent draft alone.
    pub fn refresh_references(&mut self, config: &config::Config) {
        let fresh = Self::seed_from_config(config);
        self.route_names = fresh.route_names;
        self.routes = fresh.routes;
        self.providers = fresh.providers;
        self.route_efforts = fresh.route_efforts;
        self.routes_empty = fresh.routes_empty;
        let names: BTreeSet<String> = self
            .route_names
            .iter()
            .cloned()
            .chain(
                config::types::agents::explicit_refs(&self.agents)
                    .into_iter()
                    .map(|(_, name)| name),
            )
            .collect();
        self.logical_models = names.into_iter().collect();
    }

    pub const fn is_saving(&self) -> bool {
        self.save_rx.is_some()
    }

    /// Whether the chains of `purpose` can use `route`: every candidate has the kind the
    /// purpose needs (ADR 0038). Undefined and empty routes fit every purpose.
    pub fn route_fits(&self, route: &str, purpose: config::ChainPurpose) -> bool {
        self.routes
            .get(route)
            .is_none_or(|candidates| purpose.accepts_route(candidates, &self.providers))
    }

    /// The routes a chain of `purpose` offers, and the configured routes it rejects.
    pub fn chain_routes(&self, purpose: config::ChainPurpose) -> ChainRoutes {
        ChainRoutes {
            heads: self
                .logical_models
                .iter()
                .filter(|route| self.route_fits(route, purpose))
                .cloned()
                .collect(),
            existing: self
                .route_names
                .iter()
                .filter(|route| self.route_fits(route, purpose))
                .cloned()
                .collect(),
            misfits: self
                .routes
                .keys()
                .filter(|route| !self.route_fits(route, purpose))
                .cloned()
                .collect(),
        }
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
        let routing = config::RoutingConfig {
            routes: self.routes.clone(),
            ..Default::default()
        };
        if let Some(problem) =
            config::chain_kind_problems("", &self.agents, &routing, &self.providers)
                .into_iter()
                .next()
        {
            return Err(problem);
        }
        Ok(())
    }

    /// The checks of [`Self::validate`] for the binding of `role` and its categories only, so
    /// an invalid edit of another role does not stop this one from being saved.
    pub fn validate_role(&self, role: &RoleInfo) -> Result<(), config::ConfigError> {
        let (binding, categories) = parts(&self.agents, role.address);
        Self::validate_chain(&format!("agents.{}.models", role.address), &binding.models)?;
        validate_generation(&binding.generation, role.address)?;
        for (category, binding) in categories.into_iter().flatten() {
            self.agents.binding_for(role.key, Some(category))?;
            Self::validate_chain(
                &format!("agents.{}.categories.{category}.models", role.key),
                &binding.models,
            )?;
            validate_generation(
                &binding.generation,
                &format!("{}.categories.{category}", role.key),
            )?;
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

/// How the Agent Roles list groups the roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleGroup {
    Core,
    Specialist,
    System,
}

impl RoleGroup {
    pub const ALL: [Self; 3] = [Self::Core, Self::Specialist, Self::System];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Core => "Core agents",
            Self::Specialist => "Specialists",
            Self::System => "System",
        }
    }

    pub const fn singular(self) -> &'static str {
        match self {
            Self::Core => "Core agent",
            Self::Specialist => "Specialist",
            Self::System => "System",
        }
    }
}

/// One role of the Agent Roles list.
#[derive(Debug, PartialEq, Eq)]
pub struct RoleInfo {
    /// Where its binding is in the settings file (the address `explicit_refs` uses).
    pub address: &'static str,
    /// Its name as a route and a category owner.
    pub key: &'static str,
    pub label: &'static str,
    pub group: RoleGroup,
    pub blurb: &'static str,
}

impl RoleInfo {
    pub fn is_compactor(&self) -> bool {
        self.key == "compactor"
    }

    pub fn is_image_generator(&self) -> bool {
        self.key == "image_generator"
    }
}

const fn role(
    address: &'static str,
    key: &'static str,
    label: &'static str,
    group: RoleGroup,
    blurb: &'static str,
) -> RoleInfo {
    RoleInfo {
        address,
        key,
        label,
        group,
        blurb,
    }
}

/// Every role, in the order the list shows them within a group.
pub const ROLES: [RoleInfo; 11] = [
    role(
        "orchestrator",
        "orchestrator",
        "Orchestrator",
        RoleGroup::Core,
        "Plans the work and hands it to the other agents.",
    ),
    role(
        "explorer",
        "explorer",
        "Explorer",
        RoleGroup::Core,
        "Reads and searches the project without changing it.",
    ),
    role(
        "worker",
        "worker",
        "Worker",
        RoleGroup::Core,
        "Carries out the implementation. A category override takes precedence over this chain.",
    ),
    role(
        "reviewer",
        "reviewer",
        "Reviewer",
        RoleGroup::Core,
        "Checks the result against the request. A category override takes precedence over this chain.",
    ),
    role(
        "roles.web_researcher",
        "web_researcher",
        "WebResearcher",
        RoleGroup::Specialist,
        "Looks things up on the web.",
    ),
    role(
        "roles.planner",
        "planner",
        "Planner",
        RoleGroup::Specialist,
        "Writes the plan before the work starts.",
    ),
    role(
        "roles.advisor",
        "advisor",
        "Advisor",
        RoleGroup::Specialist,
        "A read-only consultant before a decision.",
    ),
    role(
        "roles.multimodal_looker",
        "multimodal_looker",
        "Multimodal Looker",
        RoleGroup::Specialist,
        "Reads images and other inputs that are not text.",
    ),
    role(
        "roles.evaluator",
        "evaluator",
        "Evaluator",
        RoleGroup::Specialist,
        "Scores a result against what was asked.",
    ),
    role(
        "roles.compactor",
        "compactor",
        "Compactor",
        RoleGroup::System,
        "Summarises long conversations. Without a chain it uses the running role's own.",
    ),
    role(
        "roles.image_generator",
        "image_generator",
        "Image generator",
        RoleGroup::System,
        "Generates images. Only image routes can be chosen; without a chain image generation is off.",
    ),
];

/// A role's own binding and, for the two roles that have them, its category overrides.
type RoleParts<'a> = (
    &'a config::RoleBindingConfig,
    Option<&'a BTreeMap<String, config::CategoryBindingConfig>>,
);

pub fn parts<'a>(agents: &'a config::AgentsConfig, address: &str) -> RoleParts<'a> {
    match address {
        "orchestrator" => (&agents.orchestrator, None),
        "explorer" => (&agents.explorer, None),
        "worker" => (&agents.worker.base, Some(&agents.worker.categories)),
        "reviewer" => (&agents.reviewer.base, Some(&agents.reviewer.categories)),
        "roles.web_researcher" => (&agents.roles.web_researcher, None),
        "roles.planner" => (&agents.roles.planner, None),
        "roles.advisor" => (&agents.roles.advisor, None),
        "roles.multimodal_looker" => (&agents.roles.multimodal_looker, None),
        "roles.evaluator" => (&agents.roles.evaluator, None),
        "roles.compactor" => (&agents.roles.compactor, None),
        _ => (&agents.roles.image_generator, None),
    }
}

/// The mutable binding of `address` and its category overrides.
pub fn parts_mut<'a>(
    agents: &'a mut config::AgentsConfig,
    address: &str,
) -> (
    &'a mut config::RoleBindingConfig,
    Option<&'a mut BTreeMap<String, config::CategoryBindingConfig>>,
) {
    match address {
        "orchestrator" => (&mut agents.orchestrator, None),
        "explorer" => (&mut agents.explorer, None),
        "worker" => (&mut agents.worker.base, Some(&mut agents.worker.categories)),
        "reviewer" => (
            &mut agents.reviewer.base,
            Some(&mut agents.reviewer.categories),
        ),
        "roles.web_researcher" => (&mut agents.roles.web_researcher, None),
        "roles.planner" => (&mut agents.roles.planner, None),
        "roles.advisor" => (&mut agents.roles.advisor, None),
        "roles.multimodal_looker" => (&mut agents.roles.multimodal_looker, None),
        "roles.evaluator" => (&mut agents.roles.evaluator, None),
        "roles.compactor" => (&mut agents.roles.compactor, None),
        _ => (&mut agents.roles.image_generator, None),
    }
}

/// Whether `address` is bound the same way in `a` and `b`, categories included.
pub fn role_eq(a: &config::AgentsConfig, b: &config::AgentsConfig, address: &str) -> bool {
    parts(a, address) == parts(b, address)
}

/// Copies the binding of `address` (categories included) from `src` into `dst`.
pub fn copy_role(dst: &mut config::AgentsConfig, src: &config::AgentsConfig, address: &str) {
    let (binding, categories) = parts(src, address);
    let (target, target_categories) = parts_mut(dst, address);
    *target = binding.clone();
    if let (Some(target), Some(categories)) = (target_categories, categories) {
        *target = categories.clone();
    }
}

/// The routes one chain editor offers (ADR 0038).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChainRoutes {
    /// Names the head may pick, including routes not created yet.
    pub heads: Vec<String>,
    /// Configured routes a fallback entry may pick.
    pub existing: BTreeSet<String>,
    /// Configured routes whose candidates do not fit the chain; an entry naming one is
    /// shown as an error.
    pub misfits: BTreeSet<String>,
}

pub fn bindings(agents: &config::AgentsConfig) -> [(&str, &config::RoleBindingConfig); 11] {
    [
        ("orchestrator", &agents.orchestrator),
        ("explorer", &agents.explorer),
        ("worker", &agents.worker),
        ("reviewer", &agents.reviewer.base),
        ("roles.web_researcher", &agents.roles.web_researcher),
        ("roles.planner", &agents.roles.planner),
        ("roles.advisor", &agents.roles.advisor),
        ("roles.multimodal_looker", &agents.roles.multimodal_looker),
        ("roles.evaluator", &agents.roles.evaluator),
        ("roles.compactor", &agents.roles.compactor),
        ("roles.image_generator", &agents.roles.image_generator),
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
