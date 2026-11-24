use super::WorkbenchState;
use super::role_profiles::SettingsWrite;
use crate::model::{
    role_profiles::{RoleProfileAction, RoleProfileJob, profile_view},
    routing_settings::RoutingSettingsModel,
    tasks::AgentRunSource,
};
use crate::panes::settings::SettingsTab;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub const fn routing_settings(&self) -> &RoutingSettingsModel {
        &self.routing_settings
    }

    pub const fn routing_settings_mut(&mut self) -> &mut RoutingSettingsModel {
        &mut self.routing_settings
    }

    pub(super) fn settings_save_in_progress(&self) -> bool {
        self.routing_settings.is_saving()
            || self.role_settings.is_saving()
            || self.provider_save_rx.is_some()
            || self.self_improvement_settings.is_saving()
    }

    pub(super) fn routing_load_options(&self) -> config::LoadOptions {
        self.production_model.as_ref().map_or_else(
            || self.settings_load_options.clone(),
            |(context, _)| context.load_options.clone(),
        )
    }

    pub fn open_routing_settings(&mut self) {
        self.open_settings_tab(SettingsTab::Routing, None);
        // A direct entry has nothing to return to.
        self.routing_settings.origin_role_settings = false;
    }

    /// Opens routing on the profile being edited in role settings, with `logical` added.
    /// The role draft stays untouched; a route of that name already in the draft is shown
    /// instead of duplicated.
    pub fn open_routing_settings_prefill(&mut self, logical: &str) {
        if self.settings_save_in_progress() {
            return;
        }
        let profile = self.session_profile();
        if self.settings.tab.is_none() {
            self.begin_settings_session();
        }
        if !self.settings.routing_seeded {
            self.seed_routing_tab(profile.as_deref());
        }
        self.settings.tab = Some(SettingsTab::Routing);
        if !self.settings.routing_seeded {
            return;
        }
        let agents = if self.settings.roles_seeded {
            self.role_settings.agents.clone()
        } else {
            self.routing_settings.agents.clone()
        };
        self.routing_settings.add_prefill_route(logical, &agents);
    }

    pub fn apply_routing_profile_action(&mut self, action: RoleProfileAction) {
        if self.settings_save_in_progress() || self.settings_has_unsaved_edits() {
            return;
        }
        let (job, write): (RoleProfileJob, Box<SettingsWrite>) = match action {
            RoleProfileAction::Select(name) => {
                if let Err(error) = self.select_settings_profile(&name) {
                    self.routing_settings.validation_error = Some(error);
                }
                return;
            }
            RoleProfileAction::Create(name) => {
                let name = match self.routing_settings.profiles.validate_new_name(&name) {
                    Ok(name) => name,
                    Err(error) => {
                        self.routing_settings.validation_error = Some(error);
                        return;
                    }
                };
                let source = self.routing_settings.profiles.selected.clone();
                let target = name.clone();
                (
                    RoleProfileJob::Created(name),
                    Box::new(move |path, current, context| {
                        super::role_profiles::create_profile(
                            path, current, context, &source, &target,
                        )
                    }),
                )
            }
            RoleProfileAction::Delete(name) => {
                let target = name.clone();
                (
                    RoleProfileJob::Deleted(name),
                    Box::new(move |path, _, context| {
                        super::role_profiles::delete_profile(path, context, &target)
                    }),
                )
            }
        };
        self.start_routing_job(job, write);
    }

    fn start_routing_job(&mut self, job: RoleProfileJob, write: Box<SettingsWrite>) {
        match self.spawn_settings_job(write) {
            Ok(rx) => {
                self.routing_settings.validation_error = None;
                self.routing_settings.job = job;
                self.routing_settings.save_rx = Some(rx);
            }
            Err(error) => self.routing_settings.validation_error = Some(error),
        }
    }

    pub fn submit_routing_settings(&mut self) {
        if self.settings_save_in_progress() || self.routing_settings.conflict.is_some() {
            return;
        }
        let routing = match self.routing_settings.validated_routing() {
            Ok(routing) => routing,
            Err(error) => {
                self.routing_settings.validation_error = Some(error.to_string());
                return;
            }
        };
        let renames = self.routing_settings.route_renames();
        let profile = self.routing_settings.profiles.selected.clone();
        self.start_routing_job(
            RoleProfileJob::Save,
            Box::new(move |path, current, context| {
                if renames.is_empty() {
                    return context
                        .save_role_profile_bindings(path, Some(&profile), Some(&routing), None)
                        .map_err(|error| error.to_string());
                }
                save_renamed_routes(path, current, context, &profile, &routing, &renames)
            }),
        );
    }

    pub fn poll_routing_save(&mut self) {
        let Some(rx) = self.routing_settings.save_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(config)) => {
                let job = std::mem::take(&mut self.routing_settings.job);
                match job {
                    RoleProfileJob::Save => self.finish_routing_save(&config),
                    RoleProfileJob::Created(name) => {
                        self.reseed_settings_tabs(&config, Some(&name), Some(SettingsTab::Routing));
                        self.push_notice(format!("Role profile '{name}' created"));
                    }
                    RoleProfileJob::Deleted(name) => {
                        self.reseed_settings_tabs(&config, None, Some(SettingsTab::Routing));
                        self.push_notice(format!("Role profile '{name}' deleted"));
                    }
                }
            }
            Ok(Err(error)) => self.routing_settings.validation_error = Some(error),
            Err(std::sync::mpsc::TryRecvError::Empty) => self.routing_settings.save_rx = Some(rx),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.routing_settings.validation_error =
                    Some("Routing settings worker stopped without a result".into());
            }
        }
    }

    /// Routing was written: the routing draft becomes the saved state, the roles tab learns
    /// the new routes and renames (its draft is otherwise untouched), and a modal that came
    /// from roles goes back to them.
    fn finish_routing_save(&mut self, config: &config::Config) {
        let return_to_roles = self.routing_settings.origin_role_settings;
        let profile = self.routing_settings.profiles.selected.clone();
        let renames = self.routing_settings.route_renames();
        let expanded = crate::model::settings_tabs::expanded_after_save(&self.routing_settings);
        let (view, picker) = self.profile_settings_from(config, Some(&profile));
        let roles = self
            .settings
            .roles_seeded
            .then_some(&mut self.role_settings);
        crate::model::settings_tabs::carry_routing_save(
            &mut self.routing_settings,
            roles,
            &view,
            picker,
            &renames,
            expanded,
        );
        if self.settings.roles_seeded {
            self.refresh_role_previews();
        }
        if return_to_roles && self.settings.tab == Some(SettingsTab::Routing) {
            self.settings.tab = Some(SettingsTab::AgentRoles);
            self.ensure_tab_seeded(SettingsTab::AgentRoles);
        }
        self.push_notice("Routing settings updated");
    }
}

/// Saves `routing` into `profile` and moves the profile's agent bindings to renamed routes.
fn save_renamed_routes(
    path: &std::path::Path,
    current: &config::Config,
    context: &config::LayeredSaveContext<'_>,
    profile: &str,
    routing: &config::RoutingConfig,
    renames: &std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    let loaded = profile_view(current, profile);
    let mut renamed_agents = loaded.agents.clone();
    config::types::agents::rename_route_refs(&mut renamed_agents, renames);

    // 保存先だけを仮置換し、上位レイヤー適用後も参照更新が有効か確認する。
    let mut candidate = match std::fs::read_to_string(path) {
        Ok(content) => {
            toml::from_str::<toml::Table>(&content).map_err(|error| error.to_string())?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(error) => return Err(error.to_string()),
    };
    let target = profile_table(&mut candidate, profile)?;
    // Values local to the save target are the baseline. Copying the
    // merged agents would persist unrelated overrides from other layers.
    let mut file_agents: config::AgentsConfig = target
        .get("agents")
        .cloned()
        .map(toml::Value::try_into)
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    config::types::agents::rename_route_refs(&mut file_agents, renames);
    // A chain referencing a renamed route is written to the save target in full, so the
    // rename also applies when the chain came from a lower layer.
    for (address, old_name) in config::types::agents::explicit_refs(&loaded.agents) {
        if renames.contains_key(&old_name) {
            let mut source = renamed_agents.clone();
            let chain = config::types::agents::chain_at_mut(&mut source, &address)
                .ok_or_else(|| format!("Unknown agent binding address: {address}"))?
                .clone();
            *config::types::agents::chain_at_mut(&mut file_agents, &address)
                .ok_or_else(|| format!("Unknown agent binding address: {address}"))? = chain;
        }
    }
    let prepared = context
        .prepare_role_profiles_bindings(
            path,
            &[config::RoleProfileBindings {
                profile: Some(profile),
                routing: Some(routing),
                agents: Some(&file_agents),
            }],
        )
        .map_err(|error| error.to_string())?;
    let effective = profile_view(prepared.config(), profile);
    let before: std::collections::BTreeSet<_> =
        config::types::agents::explicit_refs(&loaded.agents)
            .into_iter()
            .collect();
    let candidate_refs: std::collections::BTreeSet<_> =
        config::types::agents::explicit_refs(&effective.agents)
            .into_iter()
            .collect();
    let blocked: std::collections::BTreeSet<_> =
        config::types::agents::explicit_refs(&renamed_agents)
            .into_iter()
            .filter(|reference| !before.contains(reference) && !candidate_refs.contains(reference))
            .map(|(address, new_name)| format!("{address} -> {new_name}"))
            .collect();
    if !blocked.is_empty() {
        return Err(format!(
            "Route rename blocked: higher-priority config (config.d drop-in, EVORCH_* env, or CLI override) still pins agents binding(s) {} to the old route name. Remove that override or edit that layer directly. No changes were saved.",
            blocked.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    prepared.write().map_err(|error| error.to_string())
}

/// The table holding `profile`'s agents and routing inside a raw config table.
fn profile_table<'a>(
    root: &'a mut toml::Table,
    profile: &str,
) -> Result<&'a mut toml::Table, String> {
    if profile == config::DEFAULT_ROLE_PROFILE {
        return Ok(root);
    }
    root.entry("role_profiles")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| "role_profiles must be a table".to_owned())?
        .entry(profile)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| format!("role_profiles.{profile} must be a table"))
}
