//! The settings file as text, for a client that edits it through whole-document saves: the
//! Routing and Agent roles drafts seeded from it, and the text each of their saves writes.
//!
//! The text is read on its own, on top of the built-in layer: drop-ins and the environment of
//! the machine that holds the file are not seen here, and the core checks every save in its
//! layered configuration.

use std::collections::BTreeMap;

use config::types::agents::{chain_at_mut, explicit_refs, rename_route_refs};
use config::{AgentsConfig, Config, DEFAULT_ROLE_PROFILE, RoleProfileBindings, RoutingConfig};

use super::role_profiles::{RoleProfilePicker, profile_view};
use super::role_settings::{ROLES, RoleInfo, RoleSettingsModel, copy_role, role_eq};
use super::routing_settings::RoutingSettingsModel;

#[derive(Clone)]
pub struct SettingsDocument {
    text: String,
    config: Config,
}

impl SettingsDocument {
    /// # Errors
    /// The text is not a configuration this build reads.
    pub fn parse(text: &str) -> Result<Self, String> {
        let config = Config::load_standalone_text(text).map_err(|error| error.to_string())?;
        Ok(Self {
            text: text.to_owned(),
            config,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    fn view(&self, preferred: Option<&str>) -> (Config, RoleProfilePicker) {
        let picker = RoleProfilePicker::new(&self.config, preferred, None);
        (profile_view(&self.config, &picker.selected), picker)
    }

    pub fn seed_routing(&self, preferred: Option<&str>) -> RoutingSettingsModel {
        let (view, picker) = self.view(preferred);
        let mut model = RoutingSettingsModel::seed_from_config(&view);
        model.profiles = picker;
        model
    }

    pub fn seed_roles(&self, preferred: Option<&str>) -> RoleSettingsModel {
        let (view, picker) = self.view(preferred);
        let mut model = RoleSettingsModel::seed_from_config(&view);
        model.profiles = picker;
        model
    }

    /// Takes what the file holds into a routing draft, keeping the edits of the routes that
    /// have some. A route with edits is kept as drafted unless the file's version of it is not
    /// the one the draft started from: it is then marked conflicted, and is only discarded.
    ///
    /// `saved` is a route this client just wrote, as (name it had, name it has now): it is what
    /// the file holds, so it is no longer an edit.
    pub fn adopt_routing(&self, model: &mut RoutingSettingsModel, saved: Option<(&str, &str)>) {
        let (view, picker) = self.view(Some(&model.profiles.selected));
        let fresh = view.routing.routes.clone();
        if let Some((old, new)) = saved {
            model.route_name_edits.remove(old);
            model.routes.remove(old);
            model.base_routes.remove(old);
            if let Some(candidates) = fresh.get(new) {
                model.routes.insert(new.into(), candidates.clone());
                model.base_routes.insert(new.into(), candidates.clone());
            }
            if model.selected.as_deref() == Some(old) {
                model.selected = Some(new.into());
            }
            if model.pending_new_route.as_deref() == Some(old) {
                model.pending_new_route = None;
            }
        }
        let drafted = model.dirty_routes();
        // A conflict stays until the draft is discarded (or comes to equal the file): saving
        // another route does not settle it, and the base below is overwritten by the file.
        let held = std::mem::take(&mut model.conflicted);
        let mut conflicted = std::collections::BTreeSet::new();
        for (name, candidates) in &fresh {
            if drafted.contains(name) {
                // A draft the file already holds (a save whose result was not seen) is no conflict.
                if model.routes.get(name) != Some(candidates)
                    && (held.contains(name) || model.base_routes.get(name) != Some(candidates))
                {
                    conflicted.insert(name.clone());
                }
            } else {
                model.routes.insert(name.clone(), candidates.clone());
            }
        }
        for name in &drafted {
            if !fresh.contains_key(name)
                && (model.base_routes.contains_key(name) || held.contains(name))
            {
                conflicted.insert(name.clone());
            }
        }
        model
            .routes
            .retain(|name, _| fresh.contains_key(name) || drafted.contains(name));
        model.base_routes = fresh;
        model.conflicted = conflicted;
        model.conflict = None;
        if model
            .selected
            .as_ref()
            .is_some_and(|name| !model.routes.contains_key(name))
        {
            model.selected = None;
        }
        model.refresh_references(&view);
        // The draft never edits these, and a save writes the whole `[routing]` table.
        model.cooldown = view.routing.cooldown;
        model.fallback = view.routing.fallback;
        model.profiles.names = picker.names;
    }

    /// The Agent roles counterpart of [`Self::adopt_routing`]. `renames` are the route renames a
    /// routing save just wrote into the file's bindings; the draft carries them too, its edits
    /// otherwise untouched. `saved` is the role this client just wrote.
    pub fn adopt_roles(
        &self,
        model: &mut RoleSettingsModel,
        renames: &BTreeMap<String, String>,
        saved: Option<&str>,
    ) {
        let (view, picker) = self.view(Some(&model.profiles.selected));
        rename_route_refs(&mut model.agents, renames);
        rename_route_refs(&mut model.base_agents, renames);
        if let Some(address) = saved {
            copy_role(&mut model.base_agents, &view.agents, address);
        }
        let dirty = model.dirty_roles();
        // As for routes: a conflict is kept until the role is discarded or matches the file.
        let held = std::mem::take(&mut model.conflicted);
        let mut conflicted = std::collections::BTreeSet::new();
        for role in ROLES.iter() {
            if dirty.contains(&role) {
                if !role_eq(&model.agents, &view.agents, role.address)
                    && (held.contains(role.address)
                        || !role_eq(&model.base_agents, &view.agents, role.address))
                {
                    conflicted.insert(role.address);
                }
            } else {
                copy_role(&mut model.agents, &view.agents, role.address);
            }
        }
        model.base_agents = view.agents.clone();
        model.conflicted = conflicted;
        model.refresh_references(&view);
        model.profiles.names = picker.names;
    }

    /// The text that saves the route drafted under `key`, and nothing else of the drafts: the
    /// file's routing with that route added, changed or renamed. A rename moves the profile's
    /// explicit role and category bindings with it in the same text, as the legacy save does,
    /// except that a binding pinned by a higher layer is refused by the core instead of here.
    ///
    /// # Errors
    /// The route does not validate, or the edited document does not.
    pub fn route_text(&self, model: &RoutingSettingsModel, key: &str) -> Result<String, String> {
        let profile = model.profiles.selected.as_str();
        let view = profile_view(&self.config, profile);
        let (name, candidates) = model
            .validated_route(key, &view.routing.routes)
            .map_err(|error| error.to_string())?;
        let renames: BTreeMap<String, String> =
            if name != key && view.routing.routes.contains_key(key) {
                [(key.to_owned(), name.clone())].into()
            } else {
                BTreeMap::new()
            };
        let mut routes = view.routing.routes.clone();
        routes.remove(key);
        routes.insert(name.clone(), candidates.clone());
        let routing = RoutingConfig {
            routes,
            ..view.routing.clone()
        };
        let alone = RoutingConfig {
            routes: [(name, candidates)].into(),
            ..RoutingConfig::default()
        };
        let mut agents = view.agents.clone();
        rename_route_refs(&mut agents, &renames);
        if let Some(problem) = config::route_mixing_problems("", &alone, &view.providers)
            .into_iter()
            .chain(config::chain_kind_problems(
                "",
                &agents,
                &alone,
                &view.providers,
            ))
            .next()
        {
            return Err(problem.to_string());
        }
        if renames.is_empty() {
            return self.bindings_text(profile, Some(&routing), None);
        }
        let agents = self.renamed_file_agents(profile, &renames)?;
        self.bindings_text(profile, Some(&routing), Some(&agents))
    }

    /// The text that deletes the saved route `key`; the bindings that name it stay, and show as
    /// having no route.
    ///
    /// # Errors
    /// The edited document does not validate.
    pub fn route_removed_text(
        &self,
        model: &RoutingSettingsModel,
        key: &str,
    ) -> Result<String, String> {
        let profile = model.profiles.selected.as_str();
        let mut routing = profile_view(&self.config, profile).routing;
        routing.routes.remove(key);
        self.bindings_text(profile, Some(&routing), None)
    }

    fn renamed_file_agents(
        &self,
        profile: &str,
        renames: &BTreeMap<String, String>,
    ) -> Result<AgentsConfig, String> {
        let loaded = profile_view(&self.config, profile);
        let mut renamed_agents = loaded.agents.clone();
        rename_route_refs(&mut renamed_agents, renames);
        let raw: toml::Table = toml::from_str(&self.text).map_err(|error| error.to_string())?;
        let target = if profile == DEFAULT_ROLE_PROFILE {
            Some(&raw)
        } else {
            raw.get("role_profiles")
                .and_then(toml::Value::as_table)
                .and_then(|profiles| profiles.get(profile))
                .and_then(toml::Value::as_table)
        };
        let mut file_agents: AgentsConfig = target
            .and_then(|table| table.get("agents"))
            .cloned()
            .map(toml::Value::try_into)
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_default();
        rename_route_refs(&mut file_agents, renames);
        // A chain that uses a renamed route is written to the file in full, so the rename
        // also reaches a chain the file inherits from the built-in layer.
        for (address, old_name) in explicit_refs(&loaded.agents) {
            if renames.contains_key(&old_name) {
                let unknown = || format!("Unknown agent binding address: {address}");
                let chain = chain_at_mut(&mut renamed_agents, &address)
                    .ok_or_else(unknown)?
                    .clone();
                *chain_at_mut(&mut file_agents, &address).ok_or_else(unknown)? = chain;
            }
        }
        Ok(file_agents)
    }

    /// The text that saves `role`'s binding (chain, preset, generation and categories) into
    /// the file's agents, and nothing else of the drafts.
    ///
    /// # Errors
    /// The role's draft does not validate, or the edited document does not.
    pub fn role_text(&self, model: &RoleSettingsModel, role: &RoleInfo) -> Result<String, String> {
        model
            .validate_role(role)
            .map_err(|error| error.to_string())?;
        let profile = model.profiles.selected.as_str();
        let view = profile_view(&self.config, profile);
        let problems = |agents: &AgentsConfig| {
            config::chain_kind_problems("", agents, &view.routing, &view.providers)
                .into_iter()
                .map(|problem| problem.to_string())
                .collect::<Vec<_>>()
        };
        let before = problems(&view.agents);
        let mut agents = view.agents.clone();
        copy_role(&mut agents, &model.agents, role.address);
        // A mismatch the file already has is not this role's to report.
        if let Some(problem) = problems(&agents)
            .into_iter()
            .find(|problem| !before.contains(problem))
        {
            return Err(problem);
        }
        self.bindings_text(profile, None, Some(&agents))
    }

    fn bindings_text(
        &self,
        profile: &str,
        routing: Option<&RoutingConfig>,
        agents: Option<&AgentsConfig>,
    ) -> Result<String, String> {
        config::role_profiles_bindings_text(
            &self.text,
            &[RoleProfileBindings {
                profile: Some(profile),
                routing,
                agents,
            }],
        )
        .map_err(|error| error.to_string())
    }

    /// # Errors
    /// `source` is gone, or the edited document does not validate.
    pub fn new_profile_text(&self, source: &str, name: &str) -> Result<String, String> {
        let profile = self
            .config
            .role_profile_config(Some(source))
            .ok_or_else(|| format!("Role profile '{source}' no longer exists"))?;
        config::role_profile_text(&self.text, name, &profile).map_err(|error| error.to_string())
    }

    /// # Errors
    /// The edited document does not validate.
    pub fn without_profile_text(&self, name: &str) -> Result<String, String> {
        config::without_role_profile_text(&self.text, name).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewProvider {
    OpenaiCompatible,
    Codex,
    ClaudeSubscription,
    Cursor,
    Kimi,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewProviderDefaults {
    pub name: String,
    pub base_url: String,
    pub models: Vec<String>,
    pub default_model: String,
}

const CODEX_DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

pub fn new_provider_defaults(
    kind: NewProvider,
    taken: impl Fn(&str) -> bool,
) -> NewProviderDefaults {
    use config::types::provider as defaults;
    let (prefix, base_url, models, default_model): (_, &str, &[&str], &str) = match kind {
        NewProvider::OpenaiCompatible => ("openai-compat", "", &[], ""),
        NewProvider::Codex => (
            "codex",
            CODEX_DEFAULT_BASE_URL,
            defaults::CODEX_DEFAULT_MODELS,
            defaults::CODEX_DEFAULT_MODEL,
        ),
        NewProvider::ClaudeSubscription => (
            "claude",
            defaults::CLAUDE_DEFAULT_BASE_URL,
            defaults::CLAUDE_DEFAULT_MODELS,
            defaults::CLAUDE_DEFAULT_MODEL,
        ),
        NewProvider::Cursor => (
            "cursor",
            defaults::CURSOR_DEFAULT_BASE_URL,
            defaults::CURSOR_DEFAULT_MODELS,
            defaults::CURSOR_DEFAULT_MODEL,
        ),
        NewProvider::Kimi => (
            "kimi",
            defaults::KIMI_DEFAULT_BASE_URL,
            defaults::KIMI_DEFAULT_MODELS,
            defaults::KIMI_DEFAULT_MODEL,
        ),
    };
    let mut name = prefix.to_owned();
    let mut suffix = 2;
    while taken(&name) {
        name = format!("{prefix}-{suffix}");
        suffix += 1;
    }
    NewProviderDefaults {
        name,
        base_url: base_url.to_owned(),
        models: models.iter().map(|id| (*id).to_owned()).collect(),
        default_model: default_model.to_owned(),
    }
}

#[cfg(test)]
#[path = "settings_document_tests.rs"]
mod tests;
