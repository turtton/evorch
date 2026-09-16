#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
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
        match (super::parse_input(raw), self.role) {
            (super::ComposerInput::Chat(args), ComposerRole::Orchestrator) => {
                super::ComposerInput::Command {
                    spec: &super::GOAL_COMMAND,
                    args,
                }
            }
            (parsed, _) => parsed,
        }
    }
}
