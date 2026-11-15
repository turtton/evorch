//! Process-local observation, not a durable restart or replay capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellJobIdentity {
    pub(crate) id: String,
    pub(crate) owner: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellJobState {
    Running,
    Terminal,
}

impl ShellJobIdentity {
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// A bounded, immutable terminal outcome. Reading it does not acknowledge it.
#[derive(Clone, Debug)]
pub struct ShellJobResult {
    pub snapshot: ShellJobSnapshot,
    pub result: crate::ToolResult,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellJobSnapshot {
    pub identity: ShellJobIdentity,
    pub owner_run: String,
    pub call_id: Option<String>,
    pub state: ShellJobState,
    pub exit_code: Option<i32>,
    pub observed: bool,
    /// Independent of observation: an error outcome is not proof of reap.
    pub teardown_confirmed: bool,
}
