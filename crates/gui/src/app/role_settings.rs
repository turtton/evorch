use super::WorkbenchState;
use super::role_profiles::SettingsWrite;
use crate::model::{
    role_profiles::{RoleProfileAction, RoleProfileJob, RoleProfilePicker},
    role_settings::{RoleSettingsModel, categories_for_role},
    tasks::AgentRunSource,
};
use crate::panes::role_settings::{RoleSettingsAction, role_settings_modal};

impl<S: AgentRunSource> WorkbenchState<S> {
    pub const fn role_settings(&self) -> &RoleSettingsModel {
        &self.role_settings
    }
    pub const fn role_settings_mut(&mut self) -> &mut RoleSettingsModel {
        &mut self.role_settings
    }

    pub fn open_role_settings(&mut self) {
        self.open_role_settings_profile(None);
    }

    /// Opens the modal on `profile`, or the active project's profile when absent.
    pub fn open_role_settings_profile(&mut self, profile: Option<&str>) {
        if self.settings_save_in_progress() {
            return;
        }
        match self.load_profile_settings(profile) {
            Ok((config, picker)) => self.seed_role_settings(&config, picker),
            Err(error) => self.role_settings.error = Some(error),
        }
        self.provider_settings.open = false;
        self.close_theme_settings();
        self.routing_settings.open = false;
        self.sandbox_settings.open = false;
        self.self_improvement_settings.open = false;
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
                Some(RoleSettingsAction::Profile(action)) => self.apply_role_profile_action(action),
                None => {}
            }
        }
    }

    pub fn apply_role_profile_action(&mut self, action: RoleProfileAction) {
        if self.settings_save_in_progress() {
            return;
        }
        let (job, write): (RoleProfileJob, Box<SettingsWrite>) = match action {
            RoleProfileAction::Select(name) => return self.open_role_settings_profile(Some(&name)),
            RoleProfileAction::Create(name) => {
                let name = match self.role_settings.profiles.validate_new_name(&name) {
                    Ok(name) => name,
                    Err(error) => {
                        self.role_settings.error = Some(error);
                        return;
                    }
                };
                let source = self.role_settings.profiles.selected.clone();
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
        self.start_role_job(job, write);
    }

    fn start_role_job(&mut self, job: RoleProfileJob, write: Box<SettingsWrite>) {
        match self.spawn_settings_job(write) {
            Ok(rx) => {
                self.role_settings.error = None;
                self.role_settings.job = job;
                self.role_settings.save_rx = Some(rx);
            }
            Err(error) => self.role_settings.error = Some(error),
        }
    }

    fn seed_role_settings(&mut self, config: &config::Config, profiles: RoleProfilePicker) {
        use runtime::Role;
        let edits_effective = profiles.edits_effective();
        self.role_settings = RoleSettingsModel::seed_from_config(config);
        self.role_settings.profiles = profiles;
        if !edits_effective {
            // The runtime resolves the project's profile, not the one being edited.
            return;
        }
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
            .map(|(name, role)| (name.to_owned(), role, None))
            .chain(
                [("worker", Role::Worker), ("reviewer", Role::Reviewer)]
                    .into_iter()
                    .flat_map(|(role_key, role)| {
                        categories_for_role(role_key).map(move |category| {
                            (
                                format!("{role_key}.categories.{}", category.id.as_str()),
                                role,
                                Some(category.id.as_str()),
                            )
                        })
                    }),
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
                (name, resolved)
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
        let agents = self.role_settings.agents.clone();
        let profile = self.role_settings.profiles.selected.clone();
        self.start_role_job(
            RoleProfileJob::Save,
            Box::new(move |path, _| {
                config::save_role_profile_bindings(path, Some(&profile), None, Some(&agents))
                    .map_err(|error| error.to_string())
            }),
        );
    }

    pub fn poll_role_save(&mut self) {
        let Some(rx) = self.role_settings.save_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(config)) => {
                let job = std::mem::take(&mut self.role_settings.job);
                let preferred = match &job {
                    RoleProfileJob::Save => Some(self.role_settings.profiles.selected.clone()),
                    RoleProfileJob::Created(name) => Some(name.clone()),
                    RoleProfileJob::Deleted(_) => None,
                };
                let (view, picker) = self.profile_settings_from(&config, preferred.as_deref());
                self.seed_role_settings(&view, picker);
                self.role_settings.open = job != RoleProfileJob::Save;
                match job {
                    RoleProfileJob::Save => self.push_notice("Agent role settings updated"),
                    RoleProfileJob::Created(name) => {
                        self.push_notice(format!("Role profile '{name}' created"));
                    }
                    RoleProfileJob::Deleted(name) => {
                        self.push_notice(format!("Role profile '{name}' deleted"));
                    }
                }
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
