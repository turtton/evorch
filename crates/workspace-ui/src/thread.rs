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
    Stopped,
    Running,
    Waiting,
    Done,
    Error,
}

/// Runtime event phases mirrored without importing runtime-owned types.
/// `Stopped` is operator-stopped and resumable; `Waiting` awaits input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadRunPhase {
    /// Operator-stopped, with history and workspace retained for resumption.
    Stopped,
    Pending,
    Running,
    Waiting,
    Done,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPreference {
    pub profile: String,
    pub model: Option<String>,
    /// Reasoning effort chosen with the explicit model; `None` uses the provider default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadChatRole {
    Worker,
    Orchestrator,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRecord {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    pub pinned: bool,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub created_at: i64,
    pub run_ids: Vec<String>,
    /// Current conversation root; historical roots and children remain in `run_ids`.
    #[serde(default)]
    pub root_run_id: Option<String>,
    pub branch: Option<String>,
    pub worktree_path: Option<PathBuf>,
    /// Inspected active root; without a worktree path this is populated only for Shared runs.
    #[serde(default)]
    pub active_root: Option<PathBuf>,
    #[serde(default)]
    pub model_preference: Option<ModelPreference>,
    #[serde(default)]
    pub chat_role: Option<ThreadChatRole>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub draft_input: String,
    #[serde(default)]
    pub parent_thread_id: Option<ThreadId>,
    /// Worker run that requested this independent orchestrator conversation.
    #[serde(default)]
    pub escalation_source_run_id: Option<String>,
    /// How this conversation branched from `parent_thread_id`, if it did.
    #[serde(default)]
    pub lineage: Option<ThreadLineage>,
    /// A rewound version kept for restoration but hidden behind its visible version.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub superseded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineageKind {
    /// A separate conversation shown as a child thread.
    Fork,
    /// A replacement version of the parent conversation.
    Rewind,
}

/// A completed-turn boundary in the run that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ForkPoint {
    pub run_id: String,
    /// Non-system message count published with the turn completion.
    pub context_len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadLineage {
    pub kind: LineageKind,
    /// `None` branches before the first turn.
    #[serde(default)]
    pub point: Option<ForkPoint>,
}

impl ThreadRecord {
    pub fn new(id: ThreadId, project_id: ProjectId, title: impl Into<String>) -> Self {
        Self {
            id,
            project_id,
            title: title.into(),
            pinned: false,
            archived: false,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
                .unwrap_or_default(),
            run_ids: Vec::new(),
            root_run_id: None,
            branch: None,
            worktree_path: None,
            active_root: None,
            model_preference: None,
            chat_role: None,
            draft_input: String::new(),
            parent_thread_id: None,
            escalation_source_run_id: None,
            lineage: None,
            superseded: false,
        }
    }

    pub fn partition_for_project<'a>(
        threads: &'a [Self],
        project: &ProjectId,
    ) -> (Vec<&'a Self>, Vec<&'a Self>) {
        let (mut archived, mut main): (Vec<_>, Vec<_>) = threads
            .iter()
            .filter(|thread| &thread.project_id == project && !thread.superseded)
            .partition(|thread| thread.archived);
        let newest_first = |left: &&Self, right: &&Self| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| left.title.cmp(&right.title))
                .then_with(|| left.id.cmp(&right.id))
        };
        main.sort_by(|left, right| {
            right
                .pinned
                .cmp(&left.pinned)
                .then_with(|| newest_first(left, right))
        });
        archived.sort_by(newest_first);
        (main, archived)
    }

    pub fn is_rewind(&self) -> bool {
        self.lineage
            .as_ref()
            .is_some_and(|lineage| lineage.kind == LineageKind::Rewind)
    }

    pub fn state(&self, phases: &BTreeMap<String, ThreadRunPhase>) -> ThreadState {
        // A conversation's status belongs to its current root, not every run in
        // its history. Failed children and superseded roots remain browsable.
        if let Some(root) = &self.root_run_id {
            return match phases.get(root) {
                Some(ThreadRunPhase::Stopped) => ThreadState::Stopped,
                Some(ThreadRunPhase::Pending | ThreadRunPhase::Running) => ThreadState::Running,
                Some(ThreadRunPhase::Waiting) => ThreadState::Waiting,
                Some(ThreadRunPhase::Done) => ThreadState::Done,
                Some(ThreadRunPhase::Error) => ThreadState::Error,
                None => ThreadState::Active,
            };
        }
        let phases = self.run_ids.iter().filter_map(|run_id| phases.get(run_id));
        let collected: Vec<&ThreadRunPhase> = phases.collect();
        if collected
            .iter()
            .any(|phase| matches!(phase, ThreadRunPhase::Stopped))
        {
            ThreadState::Stopped
        } else if collected
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

#[cfg(test)]
mod stop_tests {
    use super::*;

    #[test]
    fn stopped_is_distinct_resumable_state_even_while_children_run() {
        let mut thread =
            ThreadRecord::new(ThreadId::new("thread"), ProjectId::new("project"), "Title");
        thread.run_ids = vec!["root".into(), "child".into()];
        let phases = BTreeMap::from([
            ("root".into(), ThreadRunPhase::Stopped),
            ("child".into(), ThreadRunPhase::Running),
        ]);
        assert_eq!(thread.state(&phases), ThreadState::Stopped);
        assert_eq!(
            serde_json::to_string(&ThreadRunPhase::Stopped).unwrap(),
            "\"stopped\""
        );
    }
}
