use super::WorkbenchState;
use crate::model::{routing_settings::RoutingSettingsModel, tasks::AgentRunSource};

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
    }

    pub(super) fn routing_load_options(&self) -> config::LoadOptions {
        self.production_model.as_ref().map_or_else(
            || config::LoadOptions {
                project_dir: self
                    .provider_settings_path
                    .as_ref()
                    .and_then(|path| path.parent())
                    .map(std::path::Path::to_path_buf),
                read_env: false,
                ..Default::default()
            },
            |(context, _)| context.load_options.clone(),
        )
    }

    pub fn open_routing_settings(&mut self) {
        if self.settings_save_in_progress() {
            return;
        }
        self.routing_settings.origin_role_settings = false;
        match config::Config::load(&self.routing_load_options()) {
            Ok(config) => self.routing_settings = RoutingSettingsModel::seed_from_config(&config),
            Err(error) => self.routing_settings.validation_error = Some(error.to_string()),
        }
        self.provider_settings.open = false;
        self.sandbox_settings.open = false;
        self.close_theme_settings();
        self.role_settings.open = false;
        self.routing_settings.open = true;
    }

    pub fn open_routing_settings_prefill(&mut self, logical: &str) {
        if self.settings_save_in_progress() {
            return;
        }
        self.routing_settings.origin_role_settings = true;
        match config::Config::load(&self.routing_load_options()) {
            Ok(config) => {
                self.routing_settings =
                    RoutingSettingsModel::seed_from_config_prefill(&config, logical);
            }
            Err(error) => self.routing_settings.validation_error = Some(error.to_string()),
        }
        self.provider_settings.open = false;
        self.sandbox_settings.open = false;
        self.close_theme_settings();
        self.role_settings.open = false;
        self.routing_settings.open = true;
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
        let Some(path) = self.provider_settings_path.clone() else {
            self.routing_settings.validation_error =
                Some("No project config path is configured".into());
            return;
        };
        let renames = self.routing_settings.route_renames();
        let options = self.routing_load_options();
        let production = self.production_model.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.routing_settings.validation_error = None;
        self.routing_settings.save_rx = Some(rx);
        std::thread::spawn(move || {
            let saved = if renames.is_empty() {
                config::save_routing(&path, &routing).map_err(|error| error.to_string())
            } else {
                config::Config::load(&options)
                    .map_err(|error| error.to_string())
                    .and_then(|loaded| {
                        let mut renamed_agents = loaded.agents.clone();
                        config::types::agents::rename_logical_model_refs(
                            &mut renamed_agents,
                            &renames,
                        );

                        // 保存先だけを仮置換し、上位レイヤー適用後も参照更新が有効か確認する。
                        let mut candidate = match std::fs::read_to_string(&path) {
                            Ok(content) => toml::from_str::<toml::Table>(&content)
                                .map_err(|error| error.to_string())?,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                toml::Table::new()
                            }
                            Err(error) => return Err(error.to_string()),
                        };
                        // Project-local values are the save baseline. Copying the merged
                        // effective agents would persist unrelated user/env/CLI overrides.
                        let mut project_agents: config::AgentsConfig = candidate
                            .get("agents")
                            .cloned()
                            .map(toml::Value::try_into)
                            .transpose()
                            .map_err(|error| error.to_string())?
                            .unwrap_or_default();
                        config::types::agents::rename_logical_model_refs(
                            &mut project_agents,
                            &renames,
                        );
                        for (address, old_name) in config::types::agents::explicit_refs(&loaded.agents) {
                            if let Some(new_name) = renames.get(&old_name) {
                                set_project_agent_ref(&mut project_agents, &address, new_name)?;
                            }
                        }
                        candidate.insert(
                            "routing".into(),
                            toml::Value::try_from(&routing).map_err(|error| error.to_string())?,
                        );
                        candidate.insert(
                            "agents".into(),
                            toml::Value::try_from(&project_agents)
                                .map_err(|error| error.to_string())?,
                        );
                        let mut candidate_options = options.clone();
                        candidate_options.file_overrides = std::collections::BTreeMap::from([
                            (path.clone(), toml::Value::Table(candidate)),
                        ]);
                        let effective = config::Config::load(&candidate_options)
                            .map_err(|error| error.to_string())?;
                        let before: std::collections::BTreeMap<_, _> =
                            config::types::agents::explicit_refs(&loaded.agents)
                                .into_iter()
                                .collect();
                        let candidate_refs: std::collections::BTreeMap<_, _> =
                            config::types::agents::explicit_refs(&effective.agents)
                                .into_iter()
                                .collect();
                        let blocked: Vec<_> =
                            config::types::agents::explicit_refs(&renamed_agents)
                                .into_iter()
                                .filter(|(address, new_name)| {
                                    before.get(address) != Some(new_name)
                                        && candidate_refs.get(address) != Some(new_name)
                                })
                                .map(|(address, new_name)| format!("{address} -> {new_name}"))
                                .collect();
                        if !blocked.is_empty() {
                            return Err(format!(
                                "Route rename blocked: higher-priority config (config.d drop-in, EVORCH_* env, or CLI override) still pins agents binding(s) {} to the old route name. Remove that override or edit that layer directly. No changes were saved.",
                                blocked.join(", ")
                            ));
                        }
                        config::save_routing_and_agents(&path, &routing, &project_agents)
                            .map_err(|error| error.to_string())
                    })
            };
            let result = saved.and_then(|()| {
                if let Some((context, model)) = production {
                    model.replace(context.reload()?);
                }
                config::Config::load(&options).map_err(|error| error.to_string())
            });
            if let Err(error) = &result {
                tracing::error!(%error, "routing settings update or recomposition failed");
            }
            let _ = tx.send(result);
        });
    }

    pub fn poll_routing_save(&mut self) {
        let Some(rx) = self.routing_settings.save_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(config)) => {
                let return_to_roles = self.routing_settings.origin_role_settings;
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
                self.routing_settings = RoutingSettingsModel::seed_from_config(&config);
                self.routing_settings.expanded = expanded;
                self.routing_settings.open = true;
                if return_to_roles {
                    self.open_role_settings();
                }
                self.push_notice("Routing settings updated");
            }
            Ok(Err(error)) => self.routing_settings.validation_error = Some(error),
            Err(std::sync::mpsc::TryRecvError::Empty) => self.routing_settings.save_rx = Some(rx),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.routing_settings.validation_error =
                    Some("Routing settings worker stopped without a result".into());
            }
        }
    }
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
        "roles.librarian" => &mut agents.roles.librarian.logical_model,
        "roles.planner" => &mut agents.roles.planner.logical_model,
        "roles.oracle" => &mut agents.roles.oracle.logical_model,
        "roles.multimodal_looker" => &mut agents.roles.multimodal_looker.logical_model,
        address => {
            let category = address
                .strip_prefix("worker.categories.")
                .ok_or_else(|| format!("Unknown agent binding address: {address}"))?;
            &mut agents
                .worker
                .categories
                .entry(category.to_owned())
                .or_default()
                .logical_model
        }
    };
    *binding = Some(logical_model.to_owned());
    Ok(())
}
