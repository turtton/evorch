use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ProjectId;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ThreadId(String);

impl ThreadId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl fmt::Display for ThreadId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadState {
    Active,
    Paused,
    Running,
    Waiting,
    Done,
    Error,
}

/// Runtime event phases mirrored without importing runtime-owned types.
/// `Paused` is operator-set only; `Waiting` is not paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadRunPhase {
    Pending,
    Running,
    Waiting,
    Done,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRecord {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    pub pinned: bool,
    pub paused: bool,
    pub run_ids: Vec<String>,
    pub branch: Option<String>,
    pub worktree_path: Option<PathBuf>,
}

impl ThreadRecord {
    pub fn new(id: ThreadId, project_id: ProjectId, title: impl Into<String>) -> Self {
        Self {
            id,
            project_id,
            title: title.into(),
            pinned: false,
            paused: false,
            run_ids: Vec::new(),
            branch: None,
            worktree_path: None,
        }
    }

    pub fn state(&self, phases: &BTreeMap<String, ThreadRunPhase>) -> ThreadState {
        if self.paused {
            return ThreadState::Paused;
        }
        let phases = self.run_ids.iter().filter_map(|run_id| phases.get(run_id));
        let collected: Vec<&ThreadRunPhase> = phases.collect();
        if collected
            .iter()
            .any(|phase| matches!(phase, ThreadRunPhase::Error))
        {
            ThreadState::Error
        } else if collected
            .iter()
            .any(|phase| matches!(phase, ThreadRunPhase::Pending | ThreadRunPhase::Running))
        {
            ThreadState::Running
        } else if collected
            .iter()
            .any(|phase| matches!(phase, ThreadRunPhase::Waiting))
        {
            ThreadState::Waiting
        } else if !self.run_ids.is_empty()
            && collected.len() == self.run_ids.len()
            && collected
                .iter()
                .all(|phase| matches!(phase, ThreadRunPhase::Done))
        {
            ThreadState::Done
        } else {
            ThreadState::Active
        }
    }
}
