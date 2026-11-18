//! Process-local observation, not a durable restart or replay capability.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Debug)]
pub struct ShellJobIdentity {
    pub(crate) id: String,
    pub(crate) owner: String,
    pub(crate) teardown: Arc<AtomicBool>,
}

impl PartialEq for ShellJobIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.owner == other.owner
    }
}
impl Eq for ShellJobIdentity {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellJobState {
    Running,
    Terminal,
}

impl ShellJobIdentity {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether the supervisor confirmed terminal process teardown. This proof
    /// survives removal of the job's registry entry; it grants no replay/control.
    pub fn teardown_confirmed(&self) -> bool {
        self.teardown.load(Ordering::Acquire)
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
