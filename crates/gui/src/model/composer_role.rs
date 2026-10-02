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
        if self.role_locked {
            return;
        }
        self.role = match self.role {
            ComposerRole::Worker => ComposerRole::Orchestrator,
            ComposerRole::Orchestrator => ComposerRole::Worker,
        };
    }

    pub fn restore_thread_role(&mut self, thread: &workspace_ui::ThreadRecord) {
        let role = if thread.escalation_source_run_id.is_some() {
            Some(ComposerRole::Orchestrator)
        } else {
            thread.chat_role.map(Into::into)
        };
        self.role = role.unwrap_or_default();
        self.role_locked = role.is_some();
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
