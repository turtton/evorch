use super::WorkbenchState;
use crate::model::{role_settings::RoleSettingsModel, tasks::AgentRunSource};

impl<S: AgentRunSource> WorkbenchState<S> {
    pub const fn role_settings(&self) -> &RoleSettingsModel {
        &self.role_settings
    }
    pub const fn role_settings_mut(&mut self) -> &mut RoleSettingsModel {
        &mut self.role_settings
    }

    fn role_load_options(&self) -> config::LoadOptions {
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

    pub fn open_role_settings(&mut self) {
        if self.role_settings.is_saving() {
            return;
        }
        match config::Config::load(&self.role_load_options()) {
            Ok(config) => self.role_settings = RoleSettingsModel::seed_from_config(&config),
            Err(error) => self.role_settings.error = Some(error.to_string()),
        }
        self.provider_settings.open = false;
        self.role_settings.open = true;
    }

    pub fn submit_role_settings(&mut self) {
        if self.role_settings.is_saving() || self.provider_save_rx.is_some() {
            return;
        }
        if let Err(error) = self.role_settings.validate() {
            self.role_settings.error = Some(error.to_string());
            return;
        }
        let Some(path) = self.provider_settings_path.clone() else {
            self.role_settings.error = Some("No project config path is configured".into());
            return;
        };
        let agents = self.role_settings.agents.clone();
        let options = self.role_load_options();
        let production = self.production_model.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.role_settings.error = None;
        self.role_settings.save_rx = Some(rx);
        std::thread::spawn(move || {
            let result = config::save_agent_bindings(&path, &agents)
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    if let Some((context, model)) = production {
                        model.replace(context.reload()?);
                    }
                    config::Config::load(&options).map_err(|error| error.to_string())
                });
            if let Err(error) = &result {
                tracing::error!(%error, "role settings update or recomposition failed");
            }
            let _ = tx.send(result);
        });
    }

    pub fn poll_role_save(&mut self) {
        let Some(rx) = self.role_settings.save_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(config)) => {
                self.role_settings = RoleSettingsModel::seed_from_config(&config);
                self.role_settings.open = true;
                self.push_notice("Agent role settings updated");
            }
            Ok(Err(error)) => self.role_settings.error = Some(error),
            Err(std::sync::mpsc::TryRecvError::Empty) => self.role_settings.save_rx = Some(rx),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.role_settings.error =
                    Some("Role settings worker stopped without a result".into())
            }
        }
    }
}
