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
        let options = self.routing_load_options();
        let production = self.production_model.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.routing_settings.validation_error = None;
        self.routing_settings.save_rx = Some(rx);
        std::thread::spawn(move || {
            let result = config::save_routing(&path, &routing)
                .map_err(|error| error.to_string())
                .and_then(|()| {
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
