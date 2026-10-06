use super::WorkbenchState;
use super::role_profiles::SettingsWrite;
use crate::model::{
    role_profiles::{RoleProfileAction, RoleProfileJob, profile_view},
    routing_settings::RoutingSettingsModel,
    tasks::AgentRunSource,
};

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
        if self.settings_save_in_progress() {
            return;
        }
        self.routing_settings.origin_role_settings = false;
        match self.load_profile_settings(None) {
            Ok((config, picker)) => {
                self.routing_settings = RoutingSettingsModel::seed_from_config(&config);
                self.routing_settings.profiles = picker;
            }
            Err(error) => self.routing_settings.validation_error = Some(error),
        }
        self.show_routing_settings();
    }

    /// Opens routing on the profile being edited in role settings, with `logical` added.
    pub fn open_routing_settings_prefill(&mut self, logical: &str) {
        if self.settings_save_in_progress() {
            return;
        }
        self.routing_settings.origin_role_settings = true;
        let profile = self.role_settings.profiles.selected.clone();
        match self.load_profile_settings(Some(&profile)) {
            Ok((config, picker)) => {
                self.routing_settings =
                    RoutingSettingsModel::seed_from_config_prefill(&config, logical);
                self.routing_settings.profiles = picker;
            }
            Err(error) => self.routing_settings.validation_error = Some(error),
        }
        self.show_routing_settings();
    }

    fn show_routing_settings(&mut self) {
        self.provider_settings.open = false;
        self.sandbox_settings.open = false;
        self.self_improvement_settings.open = false;
        self.close_theme_settings();
        self.role_settings.open = false;
        self.routing_settings.open = true;
    }

    /// Re-seeds the open modal from `config`, keeping where it was opened from.
    fn reseed_routing(&mut self, config: &config::Config, preferred: Option<&str>) {
        let origin = self.routing_settings.origin_role_settings;
        let (view, picker) = self.profile_settings_from(config, preferred);
        self.routing_settings = RoutingSettingsModel::seed_from_config(&view);
        self.routing_settings.profiles = picker;
        self.routing_settings.origin_role_settings = origin;
    }

    pub fn apply_routing_profile_action(&mut self, action: RoleProfileAction) {
        if self.settings_save_in_progress() {
            return;
        }
        let (job, write): (RoleProfileJob, Box<SettingsWrite>) = match action {
            RoleProfileAction::Select(name) => {
                match config::Config::load_unresolved(&self.user_settings_options()) {
                    Ok(config) => self.reseed_routing(&config, Some(&name)),
                    Err(error) => {
                        self.routing_settings.validation_error = Some(error.to_string());
                    }
                }
                self.routing_settings.open = true;
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
                    Box::new(move |path, current| {
                        super::role_profiles::create_profile(path, current, &source, &target)
                    }),
                )
            }
            RoleProfileAction::Delete(name) => {
                let target = name.clone();
                (
                    RoleProfileJob::Deleted(name),
                    Box::new(move |path, _| super::role_profiles::delete_profile(path, &target)),
                )
            }
        };
        self.start_routing_job(job, write);
    }

    fn start_routing_job(&mut self, job: RoleProfileJob, write: Box<SettingsWrite>) {
        let closes = job == RoleProfileJob::Save;
        match self.spawn_settings_job(write) {
            Ok(rx) => {
                self.routing_settings.validation_error = None;
                self.routing_settings.job = job;
                self.routing_settings.save_rx = Some(rx);
                if closes {
                    self.routing_settings.open = false;
                }
            }
            Err(error) => self.routing_settings.validation_error = Some(error),
        }
    }

    pub fn submit_routing_settings(&mut self) {
        if self.settings_save_in_progress() {
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
        let options = self.user_settings_options();
        self.start_routing_job(
            RoleProfileJob::Save,
            Box::new(move |path, current| {
                if renames.is_empty() {
                    return config::save_role_profile_bindings(
                        path,
                        Some(&profile),
                        Some(&routing),
                        None,
                    )
                    .map_err(|error| error.to_string());
                }
                save_renamed_routes(path, current, &options, &profile, &routing, &renames)
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
                    RoleProfileJob::Save => {
                        let return_to_roles = self.routing_settings.origin_role_settings;
                        let profile = self.routing_settings.profiles.selected.clone();
                        let expanded = self
                            .routing_settings
                            .expanded
                            .iter()
                            .map(|name| {
                                self.routing_settings
                                    .route_name_edits
                                    .get(name)
                                    .unwrap_or(name)
                                    .clone()
                            })
                            .collect();
                        self.reseed_routing(&config, Some(&profile));
                        self.routing_settings.origin_role_settings = false;
                        self.routing_settings.expanded = expanded;
                        if return_to_roles {
                            self.open_role_settings_profile(Some(&profile));
                        }
                        self.push_notice("Routing settings updated");
                    }
                    RoleProfileJob::Created(name) => {
                        self.reseed_routing(&config, Some(&name));
                        self.routing_settings.open = true;
                        self.push_notice(format!("Role profile '{name}' created"));
                    }
                    RoleProfileJob::Deleted(name) => {
                        self.reseed_routing(&config, None);
                        self.routing_settings.open = true;
                        self.push_notice(format!("Role profile '{name}' deleted"));
                    }
                }
            }
            Ok(Err(error)) => {
                self.routing_settings.validation_error = Some(error);
                self.routing_settings.open = true;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => self.routing_settings.save_rx = Some(rx),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.routing_settings.validation_error =
                    Some("Routing settings worker stopped without a result".into());
                self.routing_settings.open = true;
            }
        }
    }
}

/// Saves `routing` into `profile` and moves the profile's agent bindings to renamed routes.
fn save_renamed_routes(
    path: &std::path::Path,
    current: &config::Config,
    options: &config::LoadOptions,
    profile: &str,
    routing: &config::RoutingConfig,
    renames: &std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    let loaded = profile_view(current, profile);
    let mut renamed_agents = loaded.agents.clone();
    config::types::agents::rename_logical_model_refs(&mut renamed_agents, renames);

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
    config::types::agents::rename_logical_model_refs(&mut file_agents, renames);
    for (address, old_name) in config::types::agents::explicit_refs(&loaded.agents) {
        if let Some(new_name) = renames.get(&old_name) {
            set_project_agent_ref(&mut file_agents, &address, new_name)?;
        }
    }
    target.insert(
        "routing".into(),
        toml::Value::try_from(routing).map_err(|error| error.to_string())?,
    );
    target.insert(
        "agents".into(),
        toml::Value::try_from(&file_agents).map_err(|error| error.to_string())?,
    );
    let mut candidate_options = options.clone();
    candidate_options.file_overrides =
        std::collections::BTreeMap::from([(path.to_path_buf(), toml::Value::Table(candidate))]);
    let effective = profile_view(
        &config::Config::load_unresolved(&candidate_options).map_err(|error| error.to_string())?,
        profile,
    );
    let before: std::collections::BTreeMap<_, _> =
        config::types::agents::explicit_refs(&loaded.agents)
            .into_iter()
            .collect();
    let candidate_refs: std::collections::BTreeMap<_, _> =
        config::types::agents::explicit_refs(&effective.agents)
            .into_iter()
            .collect();
    let blocked: Vec<_> = config::types::agents::explicit_refs(&renamed_agents)
        .into_iter()
        .filter(|(address, new_name)| {
            before.get(address) != Some(new_name) && candidate_refs.get(address) != Some(new_name)
        })
        .map(|(address, new_name)| format!("{address} -> {new_name}"))
        .collect();
    if !blocked.is_empty() {
        return Err(format!(
            "Route rename blocked: higher-priority config (config.d drop-in, EVORCH_* env, or CLI override) still pins agents binding(s) {} to the old route name. Remove that override or edit that layer directly. No changes were saved.",
            blocked.join(", ")
        ));
    }
    config::save_role_profile_bindings(path, Some(profile), Some(routing), Some(&file_agents))
        .map_err(|error| error.to_string())
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

fn set_project_agent_ref(
    agents: &mut config::AgentsConfig,
    address: &str,
    logical_model: &str,
) -> Result<(), String> {
    let binding = match address {
        "orchestrator" => &mut agents.orchestrator.logical_model,
        "explorer" => &mut agents.explorer.logical_model,
        "worker" => &mut agents.worker.base.logical_model,
        "reviewer" => &mut agents.reviewer.logical_model,
        "roles.web_researcher" => &mut agents.roles.web_researcher.logical_model,
        "roles.planner" => &mut agents.roles.planner.logical_model,
        "roles.oracle" => &mut agents.roles.oracle.logical_model,
        "roles.multimodal_looker" => &mut agents.roles.multimodal_looker.logical_model,
        address => {
            let (categories, category) =
                if let Some(category) = address.strip_prefix("worker.categories.") {
                    (&mut agents.worker.categories, category)
                } else if let Some(category) = address.strip_prefix("reviewer.categories.") {
                    (&mut agents.reviewer.categories, category)
                } else {
                    return Err(format!("Unknown agent binding address: {address}"));
                };
            &mut categories
                .entry(category.to_owned())
                .or_default()
                .logical_model
        }
    };
    *binding = Some(logical_model.to_owned());
    Ok(())
}
