use std::path::PathBuf;

use workspace_ui::ProjectId;

use super::{WorkbenchError, WorkbenchState};
use crate::model::project_dialog::ProjectDialog;
use crate::model::tasks::AgentRunSource;
use crate::panes::project_dialog::{ProjectDialogAction, project_dialog_modal};

impl<S: AgentRunSource> WorkbenchState<S> {
    pub fn open_add_project(&mut self) {
        self.project_dialog = ProjectDialog::Add {
            path: String::new(),
            error: None,
        };
    }

    pub fn open_project_settings(&mut self, project: ProjectId) {
        let Some(record) = self.sidebar.projects.iter().find(|p| p.id == project) else {
            return;
        };
        let root = record.repo_root.clone();
        let name = record.name.clone();
        let mut error = None;
        let role_profile = config::project_role_profile(&root)
            .unwrap_or_else(|failure| {
                error = Some(failure.to_string());
                None
            })
            .unwrap_or_else(|| config::DEFAULT_ROLE_PROFILE.to_owned());
        let role_profiles = config::Config::load_unresolved(&self.user_settings_options())
            .map(|config| config.role_profile_names())
            .unwrap_or_else(|failure| {
                error.get_or_insert(failure.to_string());
                vec![config::DEFAULT_ROLE_PROFILE.to_owned()]
            });
        self.project_dialog = ProjectDialog::Settings {
            name,
            project,
            directory: String::new(),
            role_profile,
            role_profiles,
            error,
        };
    }

    pub fn project_dialog(&self) -> &ProjectDialog {
        &self.project_dialog
    }

    /// A picked folder only fills the open form; registration waits for its Add button.
    pub(super) fn apply_picked_folder(&mut self, picked: Result<Option<PathBuf>, String>) {
        let (path, error) = match &mut self.project_dialog {
            ProjectDialog::Closed | ProjectDialog::Review { .. } => return,
            ProjectDialog::Add { path, error } => (path, error),
            ProjectDialog::Settings {
                directory, error, ..
            } => (directory, error),
        };
        match picked {
            Ok(Some(picked)) => {
                *path = picked.display().to_string();
                *error = None;
            }
            Ok(None) => {}
            Err(message) => *error = Some(message),
        }
    }

    pub(super) fn render_project_dialog(&mut self, ctx: &egui::Context) {
        if !self.project_dialog.is_open() {
            return;
        }
        let Some(action) = project_dialog_modal(
            ctx,
            &mut self.project_dialog,
            &self.sidebar,
            self.folder_picker.is_busy(),
        ) else {
            return;
        };
        let result = match action {
            ProjectDialogAction::Browse => {
                ctx.request_repaint();
                self.folder_picker.start()
            }
            ProjectDialogAction::Review(path) => self.review_project(&path),
            ProjectDialogAction::Confirm { path, trust } => self
                .add_project(path)
                .and_then(|project| self.set_project_trust(&project, trust))
                .map_err(error_text)
                .map(|()| self.project_dialog = ProjectDialog::Closed),
            ProjectDialogAction::Back => {
                if let ProjectDialog::Review { path, .. } = &self.project_dialog {
                    self.project_dialog = ProjectDialog::Add {
                        path: path.display().to_string(),
                        error: None,
                    };
                }
                Ok(())
            }
            ProjectDialogAction::SetProjectTrust { project, trust } => {
                self.set_project_trust(&project, trust).map_err(error_text)
            }
            ProjectDialogAction::Rename { project, name } => {
                self.rename_project(&project, &name).map_err(error_text)
            }
            ProjectDialogAction::AddDirectory { project, path } => self
                .add_allowed_directory(&project, path)
                .map_err(error_text)
                .map(|()| {
                    if let ProjectDialog::Settings { directory, .. } = &mut self.project_dialog {
                        directory.clear();
                    }
                }),
            ProjectDialogAction::SetRoleProfile { project, profile } => {
                self.set_project_role_profile(&project, &profile)
            }
            ProjectDialogAction::SetTrust {
                project,
                path,
                trust,
            } => self
                .set_allowed_trust(&project, path, trust)
                .map_err(error_text),
            ProjectDialogAction::Close => {
                self.project_dialog = ProjectDialog::Closed;
                Ok(())
            }
        };
        self.project_dialog.set_error(result.err());
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Shows what a project at `path` overrides before registering it.
    fn review_project(&mut self, path: &std::path::Path) -> Result<(), String> {
        let path = crate::model::project_path::expand_tilde(path, self.home_dir.as_deref())
            .map_err(|error| error.to_string())?;
        if !path.is_dir() {
            return Err(format!("{} is not a directory", path.display()));
        }
        let overrides = crate::model::project_dialog::ProjectOverrides::read(&path);
        self.project_dialog = ProjectDialog::Review {
            path,
            overrides,
            error: None,
        };
        Ok(())
    }

    /// Writes the selection to the project's config and recomposes when it is the active one.
    pub fn set_project_role_profile(
        &mut self,
        project: &ProjectId,
        profile: &str,
    ) -> Result<(), String> {
        let root = self
            .sidebar
            .projects
            .iter()
            .find(|record| &record.id == project)
            .map(|record| record.repo_root.clone())
            .ok_or_else(|| "Project no longer exists".to_owned())?;
        config::save_project_role_profile(&root, Some(profile))
            .map_err(|error| error.to_string())?;
        if let ProjectDialog::Settings { role_profile, .. } = &mut self.project_dialog {
            profile.clone_into(role_profile);
        }
        // Recompose whichever model serves that project, startup or not.
        if let Some((context, model)) = self.production_model.clone() {
            let startup = context.load_options.project_dir.as_deref() == Some(root.as_path());
            let projects = self.project_models.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            self.project_profile_rx = Some(rx);
            std::thread::spawn(move || {
                let result = if startup {
                    context.reload().map(|routed| model.replace(routed))
                } else {
                    projects.map_or(Ok(()), |projects| projects.reload(&root))
                };
                let _ = tx.send(result);
            });
        }
        Ok(())
    }

    pub(super) fn poll_project_role_profile(&mut self) {
        let Some(rx) = self.project_profile_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(())) => self.push_notice("Project role profile applied"),
            Ok(Err(error)) => {
                tracing::error!(%error, "project role profile recomposition failed");
                self.push_notice(format!("Project role profile not applied: {error}"));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => self.project_profile_rx = Some(rx),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
        }
    }

    /// Whether the active project's profile recomposition is still running.
    pub fn project_role_profile_pending(&self) -> bool {
        self.project_profile_rx.is_some()
    }
}

fn error_text(error: WorkbenchError) -> String {
    error.to_string()
}
