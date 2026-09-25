#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ComposerRole {
    #[default]
    Worker,
    Orchestrator,
}

impl ComposerRole {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Orchestrator => "orchestrator",
        }
    }
}

impl super::ComposerModel {
    pub const fn toggle_role(&mut self) {
        self.role = match self.role {
            ComposerRole::Worker => ComposerRole::Orchestrator,
            ComposerRole::Orchestrator => ComposerRole::Worker,
        };
    }

    pub fn parse_submission<'a>(&self, raw: &'a str) -> super::ComposerInput<'a> {
        super::parse_input(raw)
    }
}

impl From<workspace_ui::ThreadChatRole> for ComposerRole {
    fn from(role: workspace_ui::ThreadChatRole) -> Self {
        match role {
            workspace_ui::ThreadChatRole::Worker => Self::Worker,
            workspace_ui::ThreadChatRole::Orchestrator => Self::Orchestrator,
        }
    }
}

impl From<ComposerRole> for workspace_ui::ThreadChatRole {
    fn from(role: ComposerRole) -> Self {
        match role {
            ComposerRole::Worker => Self::Worker,
            ComposerRole::Orchestrator => Self::Orchestrator,
        }
    }
}
