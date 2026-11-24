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
use super::role_settings::RoleSettingsModel;
use super::routing_settings::RoutingSettingsModel;
use super::settings_tabs;

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

    pub fn reseed_routing(&self, model: &mut RoutingSettingsModel) {
        let (view, picker) = self.view(Some(&model.profiles.selected));
        settings_tabs::reseed_routing(model, &view, picker);
    }

    pub fn routes_unchanged(&self, model: &RoutingSettingsModel) -> bool {
        self.view(Some(&model.profiles.selected)).0.routing.routes == model.base_routes
    }

    pub fn agents_unchanged(&self, model: &RoleSettingsModel) -> bool {
        self.view(Some(&model.profiles.selected)).0.agents == model.base_agents
    }

    pub fn refresh_routing(&self, model: &mut RoutingSettingsModel) {
        let (view, picker) = self.view(Some(&model.profiles.selected));
        model.refresh_references(&view);
        // The draft never edits these, and its save writes the whole `[routing]` table.
        model.cooldown = view.routing.cooldown;
        model.fallback = view.routing.fallback;
        model.profiles.names = picker.names;
    }

    pub fn refresh_roles(&self, model: &mut RoleSettingsModel) {
        let (view, picker) = self.view(Some(&model.profiles.selected));
        model.refresh_references(&view);
        model.profiles.names = picker.names;
    }

    pub fn carry_routing_save(
        &self,
        routing: &mut RoutingSettingsModel,
        roles: Option<&mut RoleSettingsModel>,
    ) {
        let renames = routing.route_renames();
        let expanded = settings_tabs::expanded_after_save(routing);
        let (view, picker) = self.view(Some(&routing.profiles.selected));
        settings_tabs::carry_routing_save(routing, roles, &view, picker, &renames, expanded);
    }

    /// The text that saves `model`'s routing into its profile. A renamed route moves the
    /// profile's agent bindings with it, as the legacy save does, except that a binding pinned
    /// by a higher layer is refused by the core instead of here.
    ///
    /// # Errors
    /// The draft does not validate, or the edited document does not.
    pub fn routing_text(&self, model: &RoutingSettingsModel) -> Result<String, String> {
        let routing = model
            .validated_routing()
            .map_err(|error| error.to_string())?;
        let renames = model.route_renames();
        let profile = model.profiles.selected.as_str();
        if renames.is_empty() {
            return self.bindings_text(profile, Some(&routing), None);
        }
        let agents = self.renamed_file_agents(profile, &renames)?;
        self.bindings_text(profile, Some(&routing), Some(&agents))
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

    /// # Errors
    /// The draft does not validate, or the edited document does not.
    pub fn agents_text(&self, model: &RoleSettingsModel) -> Result<String, String> {
        model.validate().map_err(|error| error.to_string())?;
        self.bindings_text(&model.profiles.selected, None, Some(&model.agents))
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
