use super::WorkbenchState;
use super::role_profiles::SettingsWrite;
use crate::model::{
    role_profiles::{RoleProfileAction, RoleProfileJob, RoleProfilePicker},
    role_settings::{RoleSettingsModel, categories_for_role},
    tasks::AgentRunSource,
};
use crate::panes::settings::SettingsTab;

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

    /// Opens the Agent roles tab on `profile`, or the active project's profile when absent.
    pub fn open_role_settings_profile(&mut self, profile: Option<&str>) {
        self.open_settings_tab(SettingsTab::AgentRoles, profile);
    }

    pub fn apply_role_profile_action(&mut self, action: RoleProfileAction) {
        if self.settings_save_in_progress() || self.settings_has_unsaved_edits() {
            return;
        }
        let (job, write): (RoleProfileJob, Box<SettingsWrite>) = match action {
            RoleProfileAction::Select(name) => {
                if let Err(error) = self.select_settings_profile(&name) {
                    self.role_settings.error = Some(error);
                }
                return;
            }
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

    pub(super) fn seed_role_settings(
        &mut self,
        config: &config::Config,
        profiles: RoleProfilePicker,
    ) {
        self.role_settings = RoleSettingsModel::seed_from_config(config);
        self.role_settings.profiles = profiles;
        self.refresh_role_previews();
    }

    /// Recomputes which model each role binding resolves to right now.
    pub(super) fn refresh_role_previews(&mut self) {
        use runtime::Role;
        if !self.role_settings.profiles.edits_effective() {
            // The runtime resolves the project's profile, not the one being edited.
            self.role_settings.resolved_previews.clear();
            return;
        }
        let roles = [
            ("Orchestrator", Role::Orchestrator),
            ("Explorer", Role::Explorer),
            ("Worker", Role::Worker),
            ("Reviewer", Role::Reviewer),
            ("WebResearcher", Role::WebResearcher),
            ("Planner", Role::Planner),
            ("Advisor", Role::Advisor),
            ("Multimodal Looker", Role::MultimodalLooker),
            ("Evaluator", Role::Evaluator),
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
                    .active_model()
                    .map(|model| {
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
            Box::new(move |path, _, context| {
                context
                    .save_role_profile_bindings(path, Some(&profile), None, Some(&agents))
                    .map_err(|error| error.to_string())
            }),
        );
    }

    pub fn poll_role_save(&mut self) {
        let Some(rx) = self.role_settings.save_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(config)) => match std::mem::take(&mut self.role_settings.job) {
                RoleProfileJob::Save => {
                    let selected = self.role_settings.profiles.selected.clone();
                    let (view, picker) = self.profile_settings_from(&config, Some(&selected));
                    self.seed_role_settings(&view, picker);
                    if self.settings.routing_seeded {
                        self.routing_settings.refresh_references(&view);
                    }
                    self.push_notice("Agent role settings updated");
                }
                RoleProfileJob::Created(name) => {
                    self.reseed_settings_tabs(&config, Some(&name), Some(SettingsTab::AgentRoles));
                    self.push_notice(format!("Role profile '{name}' created"));
                }
                RoleProfileJob::Deleted(name) => {
                    self.reseed_settings_tabs(&config, None, Some(SettingsTab::AgentRoles));
                    self.push_notice(format!("Role profile '{name}' deleted"));
                }
            },
            Ok(Err(error)) => self.role_settings.error = Some(error),
            Err(std::sync::mpsc::TryRecvError::Empty) => self.role_settings.save_rx = Some(rx),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.role_settings.error =
                    Some("Role settings worker stopped without a result".into());
            }
        }
    }
}
