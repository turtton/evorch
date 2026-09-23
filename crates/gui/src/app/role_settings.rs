use super::WorkbenchState;
use crate::model::{role_settings::RoleSettingsModel, tasks::AgentRunSource};
use crate::panes::role_settings::{RoleSettingsAction, role_settings_modal};

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
        if self.settings_save_in_progress() {
            return;
        }
        match config::Config::load(&self.role_load_options()) {
            Ok(config) => self.seed_role_settings(&config),
            Err(error) => self.role_settings.error = Some(error.to_string()),
        }
        self.provider_settings.open = false;
        self.close_theme_settings();
        self.routing_settings.open = false;
        self.sandbox_settings.open = false;
        self.role_settings.open = true;
    }

    pub(super) fn render_role_settings(&mut self, ctx: &egui::Context) {
        if self.role_settings.open && !self.routing_settings.open {
            match role_settings_modal(ctx, &mut self.role_settings) {
                Some(RoleSettingsAction::Save) => self.submit_role_settings(),
                Some(RoleSettingsAction::Cancel) => self.role_settings.open = false,
                Some(RoleSettingsAction::CreateRoute(logical)) => {
                    self.open_routing_settings_prefill(&logical)
                }
                None => {}
            }
        }
    }

    fn seed_role_settings(&mut self, config: &config::Config) {
        use runtime::Role;
        self.role_settings = RoleSettingsModel::seed_from_config(config);
        let roles = [
            ("Orchestrator", Role::Orchestrator),
            ("Explorer", Role::Explorer),
            ("Worker", Role::Worker),
            ("Reviewer", Role::Reviewer),
            ("WebResearcher", Role::WebResearcher),
            ("Planner", Role::Planner),
            ("Oracle", Role::Oracle),
            ("Multimodal Looker", Role::MultimodalLooker),
        ];
        let rows = roles
            .into_iter()
            .map(|(name, role)| (name, role, None))
            .chain(
                crate::model::role_settings::CATEGORIES
                    .into_iter()
                    .map(|category| (category, Role::Worker, Some(category))),
            );
        self.role_settings.resolved_previews = rows
            .map(|(name, role, category)| {
                let resolved = self
                    .production_model
                    .as_ref()
                    .map(|(_, model)| {
                        runtime::AgentModel::selected_model(model.as_ref(), role, category)
                    })
                    .filter(|selected| !selected.starts_with("unresolved:"));
                (name.into(), resolved)
            })
            .collect();
    }

    pub fn submit_role_settings(&mut self) {
        if self.settings_save_in_progress() {
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
                self.seed_role_settings(&config);
                self.role_settings.open = false;
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
