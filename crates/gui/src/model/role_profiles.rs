//! Role profile selection shared by the role and routing settings modals.

use config::{Config, DEFAULT_ROLE_PROFILE, is_valid_role_profile_name};

/// Which user-config role profile a settings modal edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleProfilePicker {
    /// `default` first, then the named profiles.
    pub names: Vec<String>,
    pub selected: String,
    /// Draft name for "New profile".
    pub new_name: String,
    pub project: Option<ProjectRoleProfile>,
}

impl Default for RoleProfilePicker {
    fn default() -> Self {
        Self {
            names: vec![DEFAULT_ROLE_PROFILE.to_owned()],
            selected: DEFAULT_ROLE_PROFILE.to_owned(),
            new_name: String::new(),
            project: None,
        }
    }
}

/// The active project's profile selection, as written in its project config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRoleProfile {
    pub project: String,
    /// `None` when the project does not select a profile.
    pub requested: Option<String>,
}

/// What a profile control asked for. Selection discards unsaved edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleProfileAction {
    Select(String),
    /// Copy the selected profile under a new name.
    Create(String),
    Delete(String),
}

/// What a settings worker did, so the poll can keep the modal open for profile edits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RoleProfileJob {
    #[default]
    Save,
    Created(String),
    Deleted(String),
}

impl RoleProfilePicker {
    /// `preferred` wins when it exists; otherwise the project's effective profile is edited.
    pub fn new(
        config: &Config,
        preferred: Option<&str>,
        project: Option<ProjectRoleProfile>,
    ) -> Self {
        let names = config.role_profile_names();
        let mut picker = Self {
            names,
            project,
            ..Self::default()
        };
        picker.selected = preferred
            .filter(|name| picker.names.iter().any(|known| known == name))
            .map_or_else(|| picker.effective().to_owned(), str::to_owned);
        picker
    }

    /// The profile the active project's runs use, after the unknown-name fallback.
    pub fn effective(&self) -> &str {
        self.project
            .as_ref()
            .and_then(|project| project.requested.as_deref())
            .filter(|name| self.names.iter().any(|known| known == name))
            .unwrap_or(DEFAULT_ROLE_PROFILE)
    }

    pub fn edits_effective(&self) -> bool {
        self.selected == self.effective()
    }

    pub fn can_delete(&self) -> bool {
        self.selected != DEFAULT_ROLE_PROFILE
    }

    /// The trimmed new profile name, or why it cannot be created.
    pub fn validate_new_name(&self, name: &str) -> Result<String, String> {
        let name = name.trim();
        if name == DEFAULT_ROLE_PROFILE || !is_valid_role_profile_name(name) {
            return Err(
                "Profile names use 1-64 lowercase letters, digits, '-' or '_' and cannot be \"default\""
                    .into(),
            );
        }
        if self.names.iter().any(|known| known == name) {
            return Err(format!("Role profile '{name}' already exists"));
        }
        Ok(name.to_owned())
    }

    /// Explains which profile the active project runs with.
    pub fn notice(&self) -> Option<String> {
        let project = self.project.as_ref()?;
        if project.requested.is_none() && self.names.len() == 1 {
            return None;
        }
        let effective = self.effective();
        let head = match project.requested.as_deref() {
            Some(requested) if requested != effective => format!(
                "Project {} selects role profile '{requested}', which does not exist, so it uses '{effective}'.",
                project.project
            ),
            _ => format!(
                "Project {} uses role profile '{effective}'.",
                project.project
            ),
        };
        let tail = if self.edits_effective() {
            "You are editing the profile it uses.".to_owned()
        } else {
            format!("Edits to '{}' do not affect it.", self.selected)
        };
        Some(format!("{head} {tail}"))
    }
}

/// The config with `profile`'s agents and routing in place of the top-level ones.
pub fn profile_view(config: &Config, profile: &str) -> Config {
    let mut view = config.clone();
    if let Some(selected) = config.role_profile_config(Some(profile)) {
        view.agents = selected.agents;
        view.routing = selected.routing;
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(names: &[&str]) -> Config {
        let mut config = Config::default();
        for name in names {
            config
                .role_profiles
                .insert((*name).into(), config::RoleProfileConfig::default());
        }
        config
    }

    fn project(requested: Option<&str>) -> Option<ProjectRoleProfile> {
        Some(ProjectRoleProfile {
            project: "demo".into(),
            requested: requested.map(Into::into),
        })
    }

    #[test]
    fn new_prefers_existing_choice_then_project_effective_profile() {
        let config = config_with(&["fast"]);
        assert_eq!(
            RoleProfilePicker::new(&config, None, project(Some("fast"))).selected,
            "fast"
        );
        assert_eq!(
            RoleProfilePicker::new(&config, Some("missing"), project(None)).selected,
            "default"
        );
        assert_eq!(
            RoleProfilePicker::new(&config, Some("default"), project(Some("fast"))).selected,
            "default"
        );
    }

    #[test]
    fn notice_reports_effective_profile_and_unknown_fallback() {
        let config = config_with(&["fast"]);
        let picker = RoleProfilePicker::new(&config, Some("default"), project(Some("fast")));
        assert_eq!(
            picker.notice().as_deref(),
            Some("Project demo uses role profile 'fast'. Edits to 'default' do not affect it.")
        );
        let picker = RoleProfilePicker::new(&config, None, project(Some("gone")));
        assert_eq!(
            picker.notice().as_deref(),
            Some(
                "Project demo selects role profile 'gone', which does not exist, so it uses 'default'. You are editing the profile it uses."
            )
        );
        assert_eq!(
            RoleProfilePicker::new(&Config::default(), None, project(None)).notice(),
            None
        );
    }

    #[test]
    fn new_name_rejects_reserved_invalid_and_duplicate_names() {
        let picker = RoleProfilePicker::new(&config_with(&["fast"]), None, None);
        for name in ["default", "Fast", "", "fast"] {
            assert!(picker.validate_new_name(name).is_err(), "{name}");
        }
        assert_eq!(picker.validate_new_name(" cheap ").as_deref(), Ok("cheap"));
    }
}
