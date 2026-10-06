//! Role profile plumbing shared by the role and routing settings modals.
//!
//! Both modals edit the user config only: projects can merely select a profile.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

use super::WorkbenchState;
use crate::model::role_profiles::{ProjectRoleProfile, RoleProfilePicker, profile_view};
use crate::model::tasks::AgentRunSource;

pub(super) type SettingsJobRx = Receiver<Result<config::Config, String>>;
/// Edits the user config file given the config loaded just before the write.
pub(super) type SettingsWrite = dyn FnOnce(&Path, &config::Config) -> Result<(), String> + Send;

impl<S: AgentRunSource> WorkbenchState<S> {
    /// The runtime's load options without the project layer or save simulations.
    pub(super) fn user_settings_options(&self) -> config::LoadOptions {
        let mut options = self.routing_load_options();
        options.project_dir = None;
        options.file_overrides.clear();
        options
    }

    /// The project directory whose config the runtime composes from.
    pub(super) fn config_project_dir(&self) -> Option<PathBuf> {
        self.routing_load_options().project_dir
    }

    fn project_role_profile_state(&self, user: &config::Config) -> Option<ProjectRoleProfile> {
        let directory = self.config_project_dir()?;
        let selected = config::project_role_profile(&directory).unwrap_or_else(|error| {
            tracing::warn!(%error, "project role profile unreadable; assuming none");
            None
        });
        let project = self
            .sidebar
            .projects
            .iter()
            .find(|project| project.repo_root == directory)
            .map_or_else(
                || {
                    directory.file_name().map_or_else(
                        || directory.display().to_string(),
                        |name| name.to_string_lossy().into_owned(),
                    )
                },
                |project| project.name.clone(),
            );
        Some(ProjectRoleProfile {
            project,
            requested: selected.or_else(|| user.role_profile.clone()),
        })
    }

    /// The user config viewed through the picked profile, plus the picker itself.
    pub(super) fn load_profile_settings(
        &self,
        preferred: Option<&str>,
    ) -> Result<(config::Config, RoleProfilePicker), String> {
        let config = config::Config::load_unresolved(&self.user_settings_options())
            .map_err(|error| error.to_string())?;
        Ok(self.profile_settings_from(&config, preferred))
    }

    pub(super) fn profile_settings_from(
        &self,
        config: &config::Config,
        preferred: Option<&str>,
    ) -> (config::Config, RoleProfilePicker) {
        let picker =
            RoleProfilePicker::new(config, preferred, self.project_role_profile_state(config));
        (profile_view(config, &picker.selected), picker)
    }

    /// Writes the user config off the UI thread, recomposes the runtime, then reloads it.
    pub(super) fn spawn_settings_job(
        &self,
        write: Box<SettingsWrite>,
    ) -> Result<SettingsJobRx, String> {
        let path = self
            .provider_settings_path
            .clone()
            .ok_or_else(|| "No config path is configured".to_owned())?;
        let options = self.user_settings_options();
        let production = self.production_model.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    config::Config::load_unresolved(&options).map_err(|error| error.to_string())
                })
                .and_then(|current| write(&path, &current))
                .and_then(|()| {
                    if let Some((context, model)) = production {
                        model.replace(context.reload()?);
                    }
                    config::Config::load_unresolved(&options).map_err(|error| error.to_string())
                });
            if let Err(error) = &result {
                tracing::error!(%error, "settings update or recomposition failed");
            }
            let _ = tx.send(result);
        });
        Ok(rx)
    }
}

/// Copies `source` under `name` in the user config.
pub(super) fn create_profile(
    path: &Path,
    current: &config::Config,
    source: &str,
    name: &str,
) -> Result<(), String> {
    let profile = current
        .role_profile_config(Some(source))
        .ok_or_else(|| format!("Role profile '{source}' no longer exists"))?;
    config::save_role_profile(path, name, &profile).map_err(|error| error.to_string())
}

pub(super) fn delete_profile(path: &Path, name: &str) -> Result<(), String> {
    config::delete_role_profile(path, name).map_err(|error| error.to_string())
}
