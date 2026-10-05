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
        self.project_dialog = ProjectDialog::Settings {
            name: record.name.clone(),
            project,
            directory: String::new(),
            error: None,
        };
    }

    pub fn project_dialog(&self) -> &ProjectDialog {
        &self.project_dialog
    }

    /// A picked folder only fills the open form; registration waits for its Add button.
    pub(super) fn apply_picked_folder(&mut self, picked: Result<Option<PathBuf>, String>) {
        let (path, error) = match &mut self.project_dialog {
            ProjectDialog::Closed => return,
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
            ProjectDialogAction::Add(path) => {
                self.add_project(path).map_err(error_text).map(|_| {
                    self.save_sidebar();
                    self.project_dialog = ProjectDialog::Closed;
                })
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
            ProjectDialogAction::SetPrimary(project) => {
                self.set_primary_project(project).map_err(error_text)
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

fn error_text(error: WorkbenchError) -> String {
    error.to_string()
}
