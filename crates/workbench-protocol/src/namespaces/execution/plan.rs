//! Read-only conversation plan views. The DB is authoritative, not event arrival order.
use crate::{
    ids::ThreadId,
    validation::{ValidationError, check_bytes},
    wire::{Revision, Sequence},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTaskStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
    /// The run that worked on the task ended without finishing it. Only the runtime sets it
    /// (the model's update is refused); the model may move the task out of it.
    Interrupted,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStatusValues {
    pub pending: u32,
    pub in_progress: u32,
    pub completed: u32,
    pub blocked: u32,
    pub interrupted: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanProgress {
    /// Every task, parents included.
    pub node_count: u32,
    /// Tasks without subtasks.
    pub leaf_count: u32,
    /// Leaves by state.
    pub leaf_counts: PlanStatusValues,
    /// The share of the plan's weight by state, in parts per million.
    pub weighted_ppm: PlanStatusValues,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanChangeCause {
    /// The conversation's model updated the plan.
    ModelUpdate,
    /// The runtime reconciled tasks left `in_progress` by a run that ended.
    RuntimeInterrupt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum PlanChange {
    /// A task was added at `position` among the children of `parent_id` (`None`: top level).
    /// A leaf that receives its first child loses its own state.
    Added {
        task_id: String,
        parent_id: Option<String>,
        position: u32,
        title: String,
        description: String,
        weight: u8,
        status: PlanTaskStatus,
    },
    /// Fields of a task changed to these values.
    Updated {
        task_id: String,
        title: Option<String>,
        description: Option<String>,
        result: Option<String>,
        blocker: Option<String>,
        weight: Option<u8>,
        status: Option<PlanTaskStatus>,
    },
    /// A task (with its subtasks) moved to `position` among the children of `parent_id`.
    Moved {
        task_id: String,
        parent_id: Option<String>,
        position: u32,
    },
    /// A task and all its subtasks were deleted. `task_ids` lists them, `task_id` first.
    SubtreeRemoved {
        task_id: String,
        task_ids: Vec<String>,
    },
    /// Every task was deleted.
    Cleared { removed: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanNodeView {
    pub task_id: String,
    pub parent_id: Option<String>,
    pub position: u32,
    pub depth: u32,
    pub title: String,
    pub weight: u8,
    /// `None` for a task with subtasks.
    pub status: Option<PlanTaskStatus>,
    pub child_count: u32,
    /// The task's share by state, in parts per million.
    pub progress_ppm: PlanStatusValues,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanNodeDetail {
    pub task_id: String,
    pub parent_id: Option<String>,
    pub position: u32,
    pub depth: u32,
    pub title: String,
    pub description: String,
    pub result: String,
    pub blocker: String,
    pub weight: u8,
    pub status: Option<PlanTaskStatus>,
    pub child_ids: Vec<String>,
    pub progress_ppm: PlanStatusValues,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanTreeQuery {
    pub thread: ThreadId,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanTaskQuery {
    pub thread: ThreadId,
    pub task_id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanHistoryQuery {
    pub thread: ThreadId,
    #[serde(default)]
    pub after_revision: Option<Revision>,
    #[serde(default)]
    pub limit: Option<u32>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub thread: ThreadId,
    pub plan_id: Option<String>,
    pub revision: Revision,
    pub progress: PlanProgress,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTreePage {
    pub plan_id: Option<String>,
    pub revision: Revision,
    pub progress: PlanProgress,
    pub nodes: Vec<PlanNodeView>,
    pub next_cursor: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTaskRead {
    pub plan_id: String,
    pub revision: Revision,
    pub node: PlanNodeDetail,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanHistoryChange {
    pub cause: PlanChangeCause,
    pub source_run_id: Option<String>,
    pub changes: Vec<PlanChange>,
    pub progress: PlanProgress,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanHistoryEntry {
    pub revision: Revision,
    pub event_id: Sequence,
    pub change: Option<PlanHistoryChange>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanHistoryPage {
    pub plan_id: Option<String>,
    pub revision: Revision,
    pub entries: Vec<PlanHistoryEntry>,
    pub next_after_revision: Option<Revision>,
}

pub(super) fn limit(value: Option<u32>, max: u32) -> Result<(), ValidationError> {
    if value.is_some_and(|v| v == 0 || v > max) {
        return Err(ValidationError::OutOfRange {
            field: "limit",
            reason: "outside plan page range",
        });
    }
    Ok(())
}
pub(super) fn bounded<T: Serialize>(value: &T, max: usize) -> Result<(), ValidationError> {
    let json = serde_json::to_string(value).expect("serializable plan view");
    check_bytes("conversation_plan", &json, max, true)
}

fn range(field: &'static str, valid: bool) -> Result<(), ValidationError> {
    if valid {
        Ok(())
    } else {
        Err(ValidationError::OutOfRange {
            field,
            reason: "outside plan limits",
        })
    }
}
impl PlanProgress {
    pub fn validate(&self) -> Result<(), ValidationError> {
        range(
            "node_count",
            self.node_count <= 128 && self.leaf_count <= self.node_count,
        )?;
        let c = &self.leaf_counts;
        range(
            "leaf_counts",
            u64::from(c.pending)
                + u64::from(c.in_progress)
                + u64::from(c.completed)
                + u64::from(c.blocked)
                + u64::from(c.interrupted)
                == u64::from(self.leaf_count),
        )?;
        let p = &self.weighted_ppm;
        range(
            "weighted_ppm",
            [
                p.pending,
                p.in_progress,
                p.completed,
                p.blocked,
                p.interrupted,
            ]
            .into_iter()
            .all(|v| v <= 1_000_000),
        )
    }
}
impl PlanNodeView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        crate::ids::validate_id("task_id", &self.task_id)?;
        self.parent_id
            .as_ref()
            .map_or(Ok(()), |id| crate::ids::validate_id("parent_id", id))?;
        check_bytes("title", &self.title, 256, false)?;
        range(
            "node",
            (1..=100).contains(&self.weight)
                && self.depth <= 8
                && self.child_count <= 128
                && self.status.is_none() == (self.child_count > 0),
        )
    }
}
impl PlanSummary {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.progress.validate()?;
        bounded(self, 16 * 1024)
    }
}
impl PlanTreePage {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.progress.validate()?;
        crate::validation::check_count("nodes", self.nodes.len(), 100)?;
        for node in &self.nodes {
            node.validate()?;
        }
        bounded(self, 16 * 1024)
    }
}
impl PlanTaskRead {
    pub fn validate(&self) -> Result<(), ValidationError> {
        crate::ids::validate_id("task_id", &self.node.task_id)?;
        crate::validation::check_count("child_ids", self.node.child_ids.len(), 128)?;
        check_bytes("title", &self.node.title, 256, false)?;
        check_bytes("description", &self.node.description, 4096, true)?;
        check_bytes("result", &self.node.result, 4096, true)?;
        check_bytes("blocker", &self.node.blocker, 2048, true)?;
        range(
            "node",
            (1..=100).contains(&self.node.weight)
                && self.node.depth <= 8
                && self.node.status.is_none() != self.node.child_ids.is_empty(),
        )?;
        bounded(self, 16 * 1024)
    }
}
impl PlanHistoryPage {
    pub fn validate(&self) -> Result<(), ValidationError> {
        crate::validation::check_count("entries", self.entries.len(), 20)?;
        let mut previous = None;
        for entry in &self.entries {
            range(
                "revision",
                entry.revision.0 > 0
                    && entry.revision <= self.revision
                    && previous.is_none_or(|p| entry.revision > p),
            )?;
            previous = Some(entry.revision);
            if let Some(change) = &entry.change {
                change.progress.validate()?;
                crate::validation::check_count("changes", change.changes.len(), 128)?;
            }
        }
        bounded(self, 256 * 1024)
    }
}
