//! Sidebar project modals: registering a root and editing one project.

use std::path::{Path, PathBuf};

use workspace_ui::ProjectId;

/// What a project would change about how its agents run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProjectOverrides {
    /// The user-config role profile `.evorch/config.toml` selects.
    pub role_profile: Option<String>,
    /// `AGENTS.md` rules at the project root.
    pub agents_md: bool,
    /// Skills under `.evorch/skills` and `.agents/skills`.
    pub skills: Vec<String>,
}

impl ProjectOverrides {
    pub fn read(root: &Path) -> Self {
        let role_profile = config::project_role_profile(root).unwrap_or_else(|error| {
            tracing::warn!(%error, "project role profile unreadable");
            None
        });
        let mut skills: Vec<String> = [".evorch/skills", ".agents/skills"]
            .into_iter()
            .filter_map(|directory| std::fs::read_dir(root.join(directory)).ok())
            .flatten()
            .flatten()
            .filter(|entry| entry.path().join("SKILL.md").is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        skills.sort();
        skills.dedup();
        Self {
            role_profile,
            agents_md: root.join("AGENTS.md").is_file(),
            skills,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.role_profile.is_none() && !self.agents_md && self.skills.is_empty()
    }
}

/// At most one project modal is open; drafts live only while it is open.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum ProjectDialog {
    #[default]
    Closed,
    Add {
        path: String,
        error: Option<String>,
    },
    /// Confirms what a new project overrides before it is registered.
    Review {
        path: PathBuf,
        overrides: ProjectOverrides,
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
            Self::Add { error, .. } | Self::Review { error, .. } | Self::Settings { error, .. } => {
                *error = message;
            }
        }
    }
}
