//! Sidebar project modals: registering a root and editing one project.

use workspace_ui::ProjectId;

/// At most one project modal is open; drafts live only while it is open.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum ProjectDialog {
    #[default]
    Closed,
    Add {
        path: String,
        error: Option<String>,
    },
    Settings {
        project: ProjectId,
        name: String,
        /// Draft path for a new allowed directory.
        directory: String,
        /// The project's role profile selection; `default` when it selects none.
        role_profile: String,
        /// User-config profiles it can select, `default` first.
        role_profiles: Vec<String>,
        error: Option<String>,
    },
}

impl ProjectDialog {
    pub fn is_open(&self) -> bool {
        !matches!(self, Self::Closed)
    }

    pub fn set_error(&mut self, message: Option<String>) {
        match self {
            Self::Closed => {}
            Self::Add { error, .. } | Self::Settings { error, .. } => *error = message,
        }
    }
}
