//! A conversation's plan: the task tree, the batch operations on it, and the rules the
//! storage enforces (ADR-less MVP, "会話の計画と進捗", PR1: private foundation).
//!
//! This module is the pure part: no SQL. [`PlanTree`] applies a batch to a copy of the
//! tree and either returns the new tree with its normalized changes or refuses the whole
//! batch with a [`PlanRejection`]. The transaction that stores the result is in
//! `repo::conversation_plan`; the public calls are on [`crate::StorageHandle`].
//!
//! # Rules
//!
//! - A **leaf** holds a state (`pending`, `in_progress`, `completed`, `blocked`,
//!   `interrupted`); a **parent** holds none and its progress is derived. A task that gets
//!   its first child loses its state. A task that loses its last child has none until the
//!   same batch sets one explicitly, so an old `completed` never comes back by itself.
//! - `interrupted` is runtime-only: the model's batch is refused when it names it
//!   ([`PlanRejection::InterruptedIsRuntimeOnly`]); the runtime's reconciliation
//!   ([`crate::StorageHandle::reconcile_conversation_plan`]) is the only way to set it.
//! - Weights are integers in `1..=100`, relative among siblings (the top level counts as the
//!   children of a virtual parent). Progress is an integer in parts per million.
//! - A batch is all or nothing. A batch whose final tree equals the tree it started from
//!   changes nothing: no revision, no event.
//! - Limits ([`PLAN_MAX_NODES`] and friends) are checked on the final tree and refuse the
//!   batch; nothing is cut off silently.
//! - Task IDs are `<plan id>:<number>`; the number never repeats within a plan, not even
//!   after the plan is cleared.

use std::collections::{HashMap, HashSet};
use std::fmt;

use event_bus::{
    PLAN_PPM_SCALE, PlanChange, PlanProgress, PlanStatusValues, PlanTaskStatus, PlanWeight,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Most tasks (parents included) in one plan.
pub const PLAN_MAX_NODES: usize = 128;
/// Deepest nesting; a top-level task has depth 1.
pub const PLAN_MAX_DEPTH: usize = 8;
/// Most operations in one model `plan_update` batch.
pub const PLAN_MAX_OPERATIONS: usize = 32;
/// Most operations in one runtime reconciliation: one per leaf, so as many as the plan has
/// tasks. The model's [`PLAN_MAX_OPERATIONS`] does not apply to it.
pub const PLAN_MAX_RECONCILE_OPERATIONS: usize = PLAN_MAX_NODES;
/// Most bytes of the plan as the sum of its tasks' serialized details.
pub const PLAN_MAX_BYTES: usize = 128 * 1024;
/// The size a tool result is held to.
pub const PLAN_TOOL_RESULT_MAX_BYTES: usize = 16 * 1024;
/// What a `plan_read` task result carries besides the task: its `plan_id` and `revision`
/// (about 80 bytes at their widest), the workbench reply envelope around the tool result,
/// and a margin. A task whose detail is at most [`PLAN_NODE_DETAIL_MAX_BYTES`] therefore
/// still fits in [`PLAN_TOOL_RESULT_MAX_BYTES`] once wrapped.
pub const PLAN_TASK_RESULT_RESERVE_BYTES: usize = 2 * 1024;
/// Most bytes of one task's serialized detail, checked when the task is written.
pub const PLAN_NODE_DETAIL_MAX_BYTES: usize =
    PLAN_TOOL_RESULT_MAX_BYTES - PLAN_TASK_RESULT_RESERVE_BYTES;
/// The serialized size a tree page stops at (at least one task is always returned).
pub const PLAN_PAGE_MAX_BYTES: usize = 12 * 1024;
/// Tasks in a tree page when the caller names no limit.
pub const PLAN_PAGE_DEFAULT_NODES: u32 = 50;
/// Most tasks in a tree page.
pub const PLAN_PAGE_MAX_NODES: u32 = 100;
/// Entries in a history page when the caller names no limit.
pub const PLAN_HISTORY_DEFAULT_ENTRIES: u32 = 10;
/// Most entries in a history page.
pub const PLAN_HISTORY_MAX_ENTRIES: u32 = 20;
/// Most bytes of a title.
pub const PLAN_TITLE_MAX_BYTES: usize = 256;
/// Most bytes of a description.
pub const PLAN_DESCRIPTION_MAX_BYTES: usize = 4096;
/// Most bytes of a result.
pub const PLAN_RESULT_MAX_BYTES: usize = 4096;
/// Most bytes of a blocker.
pub const PLAN_BLOCKER_MAX_BYTES: usize = 2048;
/// Most bytes of a `client_ref`.
pub const PLAN_CLIENT_REF_MAX_BYTES: usize = 64;

/// The revision kind named by [`crate::StorageError::StaleRevision`] for a plan.
pub const PLAN_REVISION_KIND: &str = "conversation_plan";

/// The plan identity of a thread: derived from the thread that first had the plan. It
/// stays when the plan moves to another thread (handoff) and differs for a fork.
#[must_use]
pub fn plan_id_for_thread(thread_id: &str) -> String {
    let digest = Sha256::digest(thread_id.as_bytes());
    let mut id = String::from("plan-");
    for byte in &digest[..8] {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

/// Why a plan request was refused. Every variant is about the request or the state it met;
/// none carries task text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanRejection {
    /// A batch without operations.
    EmptyBatch,
    TooManyOperations {
        actual: usize,
        max: usize,
    },
    /// `clear` must be the only operation of its batch.
    ClearNotAlone,
    /// An operation names a task this plan does not have.
    UnknownTask(String),
    /// An operation names a task of another plan.
    ForeignTask(String),
    /// A `client_ref` that no earlier operation of this batch added.
    UnknownClientRef(String),
    DuplicateClientRef(String),
    InvalidClientRef,
    /// A text field is empty where it must not be, or longer than its limit.
    InvalidText {
        field: &'static str,
        reason: &'static str,
    },
    /// An update that names no field.
    EmptyUpdate,
    InterruptedIsRuntimeOnly,
    /// A state was set on a task that has subtasks.
    ParentStatusNotSettable(String),
    /// A task that ends the batch as a leaf has no state.
    LeafStatusMissing(String),
    /// A task cannot move below itself or one of its subtasks.
    Cycle(String),
    PositionOutOfRange {
        position: u32,
        len: usize,
    },
    TooManyNodes {
        actual: usize,
        max: usize,
    },
    TooDeep {
        actual: usize,
        max: usize,
    },
    NodeDetailTooLarge {
        task_id: String,
        actual: usize,
        max: usize,
    },
    PlanTooLarge {
        actual: usize,
        max: usize,
    },
    /// The plan this thread would get already belongs to another thread.
    BindingConflict,
    /// The cursor belongs to another plan or cannot be read.
    InvalidCursor,
    /// The cursor was made at an older revision: read the first page again.
    StaleCursor {
        cursor_revision: u64,
        current: u64,
    },
}

impl fmt::Display for PlanRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyBatch => write!(formatter, "the batch has no operations"),
            Self::TooManyOperations { actual, max } => {
                write!(formatter, "{actual} operations exceed the limit of {max}")
            }
            Self::ClearNotAlone => write!(formatter, "clear must be the only operation"),
            Self::UnknownTask(id) => write!(formatter, "unknown task {id}"),
            Self::ForeignTask(id) => write!(formatter, "task {id} belongs to another plan"),
            Self::UnknownClientRef(name) => {
                write!(
                    formatter,
                    "client_ref {name} is not added by an earlier operation"
                )
            }
            Self::DuplicateClientRef(name) => write!(formatter, "client_ref {name} is used twice"),
            Self::InvalidClientRef => write!(
                formatter,
                "client_ref must be 1..={PLAN_CLIENT_REF_MAX_BYTES} bytes"
            ),
            Self::InvalidText { field, reason } => write!(formatter, "{field}: {reason}"),
            Self::EmptyUpdate => write!(formatter, "an update names no field"),
            Self::InterruptedIsRuntimeOnly => {
                write!(
                    formatter,
                    "the state interrupted is set by the runtime only"
                )
            }
            Self::ParentStatusNotSettable(id) => {
                write!(formatter, "task {id} has subtasks; its state is derived")
            }
            Self::LeafStatusMissing(id) => write!(
                formatter,
                "task {id} is a leaf without a state; set one in the same batch"
            ),
            Self::Cycle(id) => write!(formatter, "task {id} cannot move below itself"),
            Self::PositionOutOfRange { position, len } => {
                write!(formatter, "position {position} is beyond {len} siblings")
            }
            Self::TooManyNodes { actual, max } => {
                write!(formatter, "{actual} tasks exceed the limit of {max}")
            }
            Self::TooDeep { actual, max } => {
                write!(formatter, "depth {actual} exceeds the limit of {max}")
            }
            Self::NodeDetailTooLarge {
                task_id,
                actual,
                max,
            } => write!(
                formatter,
                "task {task_id} would take {actual} bytes; one task may take {max}"
            ),
            Self::PlanTooLarge { actual, max } => {
                write!(
                    formatter,
                    "the plan would take {actual} bytes; the limit is {max}"
                )
            }
            Self::BindingConflict => write!(formatter, "the plan belongs to another thread"),
            Self::InvalidCursor => write!(formatter, "the cursor is not valid for this plan"),
            Self::StaleCursor {
                cursor_revision,
                current,
            } => write!(
                formatter,
                "the cursor is from revision {cursor_revision}, the plan is at {current}: read the first page again"
            ),
        }
    }
}

/// A task named by an operation: one the plan has, or one an earlier operation of the same
/// batch added (by its `client_ref`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRef {
    Id(String),
    Ref(String),
}

/// One operation of a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PlanOperation {
    /// Adds a task below `parent` (`None`: top level) at `position` (`None`: last).
    Add {
        client_ref: String,
        #[serde(default)]
        parent: Option<TaskRef>,
        #[serde(default)]
        position: Option<u32>,
        title: String,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        weight: Option<PlanWeight>,
        /// A leaf's first state; `pending` when absent.
        #[serde(default)]
        status: Option<PlanTaskStatus>,
    },
    /// Sets the named fields. An empty `description`, `result` or `blocker` clears it.
    Update {
        task: TaskRef,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        result: Option<String>,
        #[serde(default)]
        blocker: Option<String>,
        #[serde(default)]
        weight: Option<PlanWeight>,
        #[serde(default)]
        status: Option<PlanTaskStatus>,
    },
    /// Moves a task with its subtasks below `parent` (`None`: top level) at `position`
    /// (`None`: last).
    Move {
        task: TaskRef,
        #[serde(default)]
        parent: Option<TaskRef>,
        #[serde(default)]
        position: Option<u32>,
    },
    /// Deletes a task with its subtasks.
    Remove { task: TaskRef },
    /// Deletes every task. Must be the only operation of the batch.
    Clear,
}

/// A batch of operations on the plan of `thread_id`, made by the conversation's model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanUpdate {
    /// The thread the plan is bound to. The caller resolves it; the model never names it.
    pub thread_id: String,
    /// The revision the caller read (0: the plan does not exist yet).
    pub expected_revision: u64,
    /// The run that made the request, kept in the change event for audit. Never an authority.
    #[serde(default)]
    pub source_run_id: Option<String>,
    pub operations: Vec<PlanOperation>,
}

/// What a plan request did.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanApplied {
    /// `None` while the plan was never created (a request that changed nothing on an
    /// unplanned thread).
    pub plan_id: Option<String>,
    /// The revision after the request (unchanged when `changed` is false).
    pub revision: u64,
    pub changed: bool,
    pub progress: PlanProgress,
    /// `client_ref` to task ID of the tasks the batch added and kept.
    pub created: Vec<(String, String)>,
    /// The stored change event, when `changed`. The caller publishes it after this call
    /// returned: the plan transaction has committed by then.
    pub event: Option<crate::StoredEvent>,
}

/// The plan of a thread at a glance.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanSummary {
    pub thread_id: String,
    /// `None` until the first change.
    pub plan_id: Option<String>,
    /// 0 until the first change; kept after `clear`.
    pub revision: u64,
    pub progress: PlanProgress,
    /// Saved change event's Unix milliseconds, not a fresh read-time clock.
    /// `None` until the first change; preserved across rebind and no-op updates.
    pub updated_at_unix_ms: Option<i64>,
}

/// One task as a tree page lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanNodeView {
    pub task_id: String,
    pub parent_id: Option<String>,
    pub position: u32,
    pub depth: u32,
    pub title: String,
    pub weight: PlanWeight,
    /// `None` for a task with subtasks.
    pub status: Option<PlanTaskStatus>,
    pub child_count: u32,
    /// The task's share by state, in parts per million.
    pub progress_ppm: PlanStatusValues,
}

/// A page of the tree in display (pre-)order, bound to one revision.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanTreePage {
    pub plan_id: Option<String>,
    pub revision: u64,
    pub progress: PlanProgress,
    pub nodes: Vec<PlanNodeView>,
    /// Pass it back for the next page; valid only while the revision is unchanged.
    pub next_cursor: Option<String>,
}

/// One task with all its text.
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
    pub weight: PlanWeight,
    pub status: Option<PlanTaskStatus>,
    pub child_ids: Vec<String>,
    pub progress_ppm: PlanStatusValues,
}

/// A task detail and the revision it was read at.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanTaskRead {
    pub plan_id: String,
    pub revision: u64,
    pub node: PlanNodeDetail,
}

/// One revision of the history.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanHistoryEntry {
    pub revision: u64,
    pub event_id: i64,
    /// `None` when retention reclaimed the event while the index row stayed.
    pub event: Option<event_bus::ConversationPlanEvent>,
    /// Saved event's Unix milliseconds; `None` if retention reclaimed the event.
    pub updated_at_unix_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanHistoryPage {
    pub plan_id: Option<String>,
    pub revision: u64,
    /// In ascending revision order.
    pub entries: Vec<PlanHistoryEntry>,
    /// The revision to pass as `after_revision` for the next page.
    pub next_after_revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Node {
    pub id: String,
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub title: String,
    pub description: String,
    pub result: String,
    pub blocker: String,
    pub weight: PlanWeight,
    /// `Some` exactly for a leaf, once a batch is final.
    pub status: Option<PlanTaskStatus>,
}

/// A task's place in the flattened (pre-order) tree.
#[derive(Debug, Clone)]
pub(crate) struct Flat {
    pub id: String,
    pub position: u32,
    pub depth: u32,
    pub share: PlanStatusValues,
}

/// A validated tree, flattened, with its aggregate and size.
#[derive(Debug)]
pub(crate) struct Finalized {
    pub flat: Vec<Flat>,
    pub progress: PlanProgress,
    pub plan_bytes: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct PlanTree {
    pub plan_id: String,
    pub nodes: HashMap<String, Node>,
    pub roots: Vec<String>,
    /// The number the next task ID gets.
    pub next_seq: u64,
}

pub(crate) struct Applied {
    pub changes: Vec<PlanChange>,
    pub created: Vec<(String, String)>,
}

fn check_text(
    field: &'static str,
    text: &str,
    max: usize,
    required: bool,
) -> Result<(), PlanRejection> {
    if required && text.trim().is_empty() {
        return Err(PlanRejection::InvalidText {
            field,
            reason: "must not be empty",
        });
    }
    if text.len() > max {
        return Err(PlanRejection::InvalidText {
            field,
            reason: "is too long",
        });
    }
    Ok(())
}

impl PlanTree {
    pub(crate) fn empty(plan_id: String) -> Self {
        Self {
            plan_id,
            nodes: HashMap::new(),
            roots: Vec::new(),
            next_seq: 1,
        }
    }

    /// Whether two trees hold the same tasks in the same places (the ID counter aside).
    pub(crate) fn same_tasks(&self, other: &Self) -> bool {
        self.roots == other.roots && self.nodes == other.nodes
    }

    fn resolve(
        &self,
        task: &TaskRef,
        refs: &HashMap<String, String>,
    ) -> Result<String, PlanRejection> {
        match task {
            TaskRef::Id(id) => {
                let own = id
                    .split_once(':')
                    .is_some_and(|(plan, _)| plan == self.plan_id);
                if !own && id.contains(':') {
                    return Err(PlanRejection::ForeignTask(id.clone()));
                }
                if own && self.nodes.contains_key(id) {
                    Ok(id.clone())
                } else {
                    Err(PlanRejection::UnknownTask(id.clone()))
                }
            }
            TaskRef::Ref(name) => {
                let id = refs
                    .get(name)
                    .ok_or_else(|| PlanRejection::UnknownClientRef(name.clone()))?;
                if self.nodes.contains_key(id) {
                    Ok(id.clone())
                } else {
                    Err(PlanRejection::UnknownTask(name.clone()))
                }
            }
        }
    }

    fn siblings(&self, parent: Option<&String>) -> &Vec<String> {
        match parent {
            None => &self.roots,
            Some(id) => &self.nodes[id].children,
        }
    }

    fn siblings_mut(&mut self, parent: Option<&String>) -> &mut Vec<String> {
        match parent {
            None => &mut self.roots,
            Some(id) => {
                &mut self
                    .nodes
                    .get_mut(id)
                    .expect("a resolved parent exists")
                    .children
            }
        }
    }

    fn is_below(&self, candidate: &str, ancestor: &str) -> bool {
        let mut at = Some(candidate);
        while let Some(id) = at {
            if id == ancestor {
                return true;
            }
            at = self.nodes.get(id).and_then(|node| node.parent.as_deref());
        }
        false
    }

    /// A parent holds no state of its own.
    fn became_parent(&mut self, id: &str) {
        if let Some(node) = self.nodes.get_mut(id) {
            node.status = None;
        }
    }

    fn subtree(&self, id: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![id.to_owned()];
        while let Some(next) = stack.pop() {
            out.push(next.clone());
            for child in self.nodes[&next].children.iter().rev() {
                stack.push(child.clone());
            }
        }
        out
    }

    /// Applies `operations` to a copy of the tree. The copy is the result; the receiver is
    /// untouched whatever happens. The result is validated as a whole ([`Self::finalize`]).
    pub(crate) fn apply(
        &self,
        operations: &[PlanOperation],
        allow_interrupted: bool,
    ) -> Result<(Self, Applied), PlanRejection> {
        if operations.is_empty() {
            return Err(PlanRejection::EmptyBatch);
        }
        // Only the runtime's reconciliation may name more tasks than a model batch may.
        let max_operations = if allow_interrupted {
            PLAN_MAX_RECONCILE_OPERATIONS
        } else {
            PLAN_MAX_OPERATIONS
        };
        if operations.len() > max_operations {
            return Err(PlanRejection::TooManyOperations {
                actual: operations.len(),
                max: max_operations,
            });
        }
        if operations.len() > 1
            && operations
                .iter()
                .any(|operation| matches!(operation, PlanOperation::Clear))
        {
            return Err(PlanRejection::ClearNotAlone);
        }
        let mut work = self.clone();
        let mut refs: HashMap<String, String> = HashMap::new();
        let mut seen_refs: HashSet<String> = HashSet::new();
        let mut changes = Vec::new();
        for operation in operations {
            work.apply_one(
                operation,
                allow_interrupted,
                &mut refs,
                &mut seen_refs,
                &mut changes,
            )?;
        }
        let mut created: Vec<(String, String)> = refs
            .into_iter()
            .filter(|(_, id)| work.nodes.contains_key(id))
            .collect();
        created.sort_by_key(|(_, id)| {
            id.rsplit_once(':')
                .and_then(|(_, number)| number.parse::<u64>().ok())
                .unwrap_or(u64::MAX)
        });
        Ok((work, Applied { changes, created }))
    }

    fn apply_one(
        &mut self,
        operation: &PlanOperation,
        allow_interrupted: bool,
        refs: &mut HashMap<String, String>,
        seen_refs: &mut HashSet<String>,
        changes: &mut Vec<PlanChange>,
    ) -> Result<(), PlanRejection> {
        match operation {
            PlanOperation::Add {
                client_ref,
                parent,
                position,
                title,
                description,
                weight,
                status,
            } => {
                if client_ref.is_empty() || client_ref.len() > PLAN_CLIENT_REF_MAX_BYTES {
                    return Err(PlanRejection::InvalidClientRef);
                }
                if !seen_refs.insert(client_ref.clone()) {
                    return Err(PlanRejection::DuplicateClientRef(client_ref.clone()));
                }
                let title = title.trim().to_owned();
                check_text("title", &title, PLAN_TITLE_MAX_BYTES, true)?;
                let description = description.clone().unwrap_or_default();
                check_text(
                    "description",
                    &description,
                    PLAN_DESCRIPTION_MAX_BYTES,
                    false,
                )?;
                if !allow_interrupted && *status == Some(PlanTaskStatus::Interrupted) {
                    return Err(PlanRejection::InterruptedIsRuntimeOnly);
                }
                let parent_id = parent
                    .as_ref()
                    .map(|task| self.resolve(task, refs))
                    .transpose()?;
                let len = self.siblings(parent_id.as_ref()).len();
                let position = match position {
                    None => len,
                    Some(position) if (*position as usize) <= len => *position as usize,
                    Some(position) => {
                        return Err(PlanRejection::PositionOutOfRange {
                            position: *position,
                            len,
                        });
                    }
                };
                let id = format!("{}:{}", self.plan_id, self.next_seq);
                self.next_seq += 1;
                let weight = weight.unwrap_or_default();
                let status = status.unwrap_or(PlanTaskStatus::Pending);
                self.nodes.insert(
                    id.clone(),
                    Node {
                        id: id.clone(),
                        parent: parent_id.clone(),
                        children: Vec::new(),
                        title: title.clone(),
                        description: description.clone(),
                        result: String::new(),
                        blocker: String::new(),
                        weight,
                        status: Some(status),
                    },
                );
                self.siblings_mut(parent_id.as_ref())
                    .insert(position, id.clone());
                if let Some(parent_id) = &parent_id {
                    self.became_parent(parent_id);
                }
                refs.insert(client_ref.clone(), id.clone());
                changes.push(PlanChange::Added {
                    task_id: id,
                    parent_id,
                    position: u32::try_from(position).unwrap_or(u32::MAX),
                    title,
                    description,
                    weight,
                    status,
                });
            }
            PlanOperation::Update {
                task,
                title,
                description,
                result,
                blocker,
                weight,
                status,
            } => {
                if title.is_none()
                    && description.is_none()
                    && result.is_none()
                    && blocker.is_none()
                    && weight.is_none()
                    && status.is_none()
                {
                    return Err(PlanRejection::EmptyUpdate);
                }
                let id = self.resolve(task, refs)?;
                if let Some(text) = title {
                    check_text("title", text.trim(), PLAN_TITLE_MAX_BYTES, true)?;
                }
                if let Some(text) = description {
                    check_text("description", text, PLAN_DESCRIPTION_MAX_BYTES, false)?;
                }
                if let Some(text) = result {
                    check_text("result", text, PLAN_RESULT_MAX_BYTES, false)?;
                }
                if let Some(text) = blocker {
                    check_text("blocker", text, PLAN_BLOCKER_MAX_BYTES, false)?;
                }
                if let Some(status) = status {
                    if !allow_interrupted && *status == PlanTaskStatus::Interrupted {
                        return Err(PlanRejection::InterruptedIsRuntimeOnly);
                    }
                    if !self.nodes[&id].children.is_empty() {
                        return Err(PlanRejection::ParentStatusNotSettable(id));
                    }
                }
                let node = self.nodes.get_mut(&id).expect("resolved task exists");
                let mut change = PlanChange::Updated {
                    task_id: id,
                    title: None,
                    description: None,
                    result: None,
                    blocker: None,
                    weight: None,
                    status: None,
                };
                let mut touched = false;
                if let PlanChange::Updated {
                    title: c_title,
                    description: c_description,
                    result: c_result,
                    blocker: c_blocker,
                    weight: c_weight,
                    status: c_status,
                    ..
                } = &mut change
                {
                    if let Some(text) = title {
                        let text = text.trim();
                        if node.title != text {
                            node.title = text.to_owned();
                            *c_title = Some(text.to_owned());
                            touched = true;
                        }
                    }
                    if let Some(text) = description
                        && &node.description != text
                    {
                        node.description.clone_from(text);
                        *c_description = Some(text.clone());
                        touched = true;
                    }
                    if let Some(text) = result
                        && &node.result != text
                    {
                        node.result.clone_from(text);
                        *c_result = Some(text.clone());
                        touched = true;
                    }
                    if let Some(text) = blocker
                        && &node.blocker != text
                    {
                        node.blocker.clone_from(text);
                        *c_blocker = Some(text.clone());
                        touched = true;
                    }
                    if let Some(weight) = weight
                        && node.weight != *weight
                    {
                        node.weight = *weight;
                        *c_weight = Some(*weight);
                        touched = true;
                    }
                    if let Some(status) = status
                        && node.status != Some(*status)
                    {
                        node.status = Some(*status);
                        *c_status = Some(*status);
                        touched = true;
                    }
                }
                if touched {
                    changes.push(change);
                }
            }
            PlanOperation::Move {
                task,
                parent,
                position,
            } => {
                let id = self.resolve(task, refs)?;
                let new_parent = parent
                    .as_ref()
                    .map(|task| self.resolve(task, refs))
                    .transpose()?;
                if let Some(new_parent) = &new_parent
                    && self.is_below(new_parent, &id)
                {
                    return Err(PlanRejection::Cycle(id));
                }
                let old_parent = self.nodes[&id].parent.clone();
                let old_index = self
                    .siblings(old_parent.as_ref())
                    .iter()
                    .position(|sibling| *sibling == id)
                    .expect("a task is among its parent's children");
                self.siblings_mut(old_parent.as_ref()).remove(old_index);
                let len = self.siblings(new_parent.as_ref()).len();
                let position = match position {
                    None => len,
                    Some(position) if (*position as usize) <= len => *position as usize,
                    Some(position) => {
                        return Err(PlanRejection::PositionOutOfRange {
                            position: *position,
                            len,
                        });
                    }
                };
                if old_parent == new_parent && position == old_index {
                    self.siblings_mut(old_parent.as_ref()).insert(old_index, id);
                    return Ok(());
                }
                self.siblings_mut(new_parent.as_ref())
                    .insert(position, id.clone());
                self.nodes
                    .get_mut(&id)
                    .expect("resolved task exists")
                    .parent
                    .clone_from(&new_parent);
                if let Some(old_parent) = &old_parent
                    && self.nodes[old_parent].children.is_empty()
                {
                    self.nodes
                        .get_mut(old_parent)
                        .expect("a parent exists")
                        .status = None;
                }
                if let Some(new_parent) = &new_parent {
                    self.became_parent(new_parent);
                }
                changes.push(PlanChange::Moved {
                    task_id: id,
                    parent_id: new_parent,
                    position: u32::try_from(position).unwrap_or(u32::MAX),
                });
            }
            PlanOperation::Remove { task } => {
                let id = self.resolve(task, refs)?;
                let ids = self.subtree(&id);
                let parent = self.nodes[&id].parent.clone();
                self.siblings_mut(parent.as_ref())
                    .retain(|sibling| *sibling != id);
                for removed in &ids {
                    self.nodes.remove(removed);
                }
                if let Some(parent) = &parent
                    && self.nodes[parent].children.is_empty()
                {
                    self.nodes.get_mut(parent).expect("a parent exists").status = None;
                }
                changes.push(PlanChange::SubtreeRemoved {
                    task_id: id,
                    task_ids: ids,
                });
            }
            PlanOperation::Clear => {
                let removed = self.nodes.len();
                if removed > 0 {
                    self.nodes.clear();
                    self.roots.clear();
                    changes.push(PlanChange::Cleared {
                        removed: u32::try_from(removed).unwrap_or(u32::MAX),
                    });
                }
            }
        }
        Ok(())
    }

    fn max_depth(&self) -> usize {
        let mut deepest = 0;
        let mut stack: Vec<(&String, usize)> = self.roots.iter().map(|id| (id, 1)).collect();
        while let Some((id, depth)) = stack.pop() {
            deepest = deepest.max(depth);
            for child in &self.nodes[id].children {
                stack.push((child, depth + 1));
            }
        }
        deepest
    }

    fn walk(
        &self,
        ids: &[String],
        depth: u32,
        out: &mut Vec<Flat>,
        leaves: &mut PlanStatusValues,
    ) -> PlanStatusValues {
        let mut weighted = [0_u64; 5];
        let mut total = 0_u64;
        for (position, id) in ids.iter().enumerate() {
            let node = &self.nodes[id];
            let index = out.len();
            out.push(Flat {
                id: id.clone(),
                position: u32::try_from(position).unwrap_or(u32::MAX),
                depth,
                share: PlanStatusValues::default(),
            });
            let share = match node.status {
                Some(status) if node.children.is_empty() => {
                    *leaves.slot(status) += 1;
                    let mut share = PlanStatusValues::default();
                    *share.slot(status) = PLAN_PPM_SCALE;
                    share
                }
                _ => self.walk(&node.children, depth + 1, out, leaves),
            };
            out[index].share = share;
            let weight = u64::from(node.weight.get());
            total += weight;
            for (slot, status) in PlanTaskStatus::ALL.into_iter().enumerate() {
                weighted[slot] += weight * u64::from(share.get(status));
            }
        }
        let mut aggregate = PlanStatusValues::default();
        for (slot, status) in PlanTaskStatus::ALL.into_iter().enumerate() {
            let share = weighted[slot].checked_div(total).unwrap_or(0);
            *aggregate.slot(status) = u32::try_from(share).unwrap_or(u32::MAX);
        }
        aggregate
    }

    /// Flattens the tree in pre-order and computes the aggregate. Assumes a valid tree.
    pub(crate) fn flatten(&self) -> (Vec<Flat>, PlanProgress) {
        let mut flat = Vec::with_capacity(self.nodes.len());
        let mut leaf_counts = PlanStatusValues::default();
        let weighted_ppm = self.walk(&self.roots, 1, &mut flat, &mut leaf_counts);
        let leaf_count = PlanTaskStatus::ALL
            .into_iter()
            .map(|status| leaf_counts.get(status))
            .sum();
        let progress = PlanProgress {
            node_count: u32::try_from(self.nodes.len()).unwrap_or(u32::MAX),
            leaf_count,
            leaf_counts,
            weighted_ppm,
        };
        (flat, progress)
    }

    pub(crate) fn detail(&self, flat: &Flat) -> PlanNodeDetail {
        let node = &self.nodes[&flat.id];
        PlanNodeDetail {
            task_id: node.id.clone(),
            parent_id: node.parent.clone(),
            position: flat.position,
            depth: flat.depth,
            title: node.title.clone(),
            description: node.description.clone(),
            result: node.result.clone(),
            blocker: node.blocker.clone(),
            weight: node.weight,
            status: node.status,
            child_ids: node.children.clone(),
            progress_ppm: flat.share,
        }
    }

    /// Checks the limits and the leaf/parent state rule on the final tree.
    pub(crate) fn finalize(&self) -> Result<Finalized, PlanRejection> {
        if self.nodes.len() > PLAN_MAX_NODES {
            return Err(PlanRejection::TooManyNodes {
                actual: self.nodes.len(),
                max: PLAN_MAX_NODES,
            });
        }
        let depth = self.max_depth();
        if depth > PLAN_MAX_DEPTH {
            return Err(PlanRejection::TooDeep {
                actual: depth,
                max: PLAN_MAX_DEPTH,
            });
        }
        let (flat, progress) = self.flatten();
        let mut plan_bytes = 0_usize;
        for entry in &flat {
            let node = &self.nodes[&entry.id];
            if node.children.is_empty() && node.status.is_none() {
                return Err(PlanRejection::LeafStatusMissing(node.id.clone()));
            }
            let size = serde_json::to_string(&self.detail(entry))
                .map(|text| text.len())
                .unwrap_or(usize::MAX);
            if size > PLAN_NODE_DETAIL_MAX_BYTES {
                return Err(PlanRejection::NodeDetailTooLarge {
                    task_id: node.id.clone(),
                    actual: size,
                    max: PLAN_NODE_DETAIL_MAX_BYTES,
                });
            }
            plan_bytes += size;
        }
        if plan_bytes > PLAN_MAX_BYTES {
            return Err(PlanRejection::PlanTooLarge {
                actual: plan_bytes,
                max: PLAN_MAX_BYTES,
            });
        }
        Ok(Finalized {
            flat,
            progress,
            plan_bytes,
        })
    }

    /// The operations that the runtime's reconciliation applies: every `in_progress` leaf
    /// becomes `interrupted`, in display order.
    pub(crate) fn interrupt_operations(&self) -> Vec<PlanOperation> {
        let (flat, _) = self.flatten();
        flat.iter()
            .filter(|entry| {
                self.nodes[&entry.id].status == Some(PlanTaskStatus::InProgress)
                    && self.nodes[&entry.id].children.is_empty()
            })
            .map(|entry| PlanOperation::Update {
                task: TaskRef::Id(entry.id.clone()),
                title: None,
                description: None,
                result: None,
                blocker: None,
                weight: None,
                status: Some(PlanTaskStatus::Interrupted),
            })
            .collect()
    }
}

/// Every text of a batch with the field it is checked as, for the secret guard.
pub(crate) fn operation_texts(operations: &[PlanOperation]) -> Vec<(&'static str, &str)> {
    let mut texts = Vec::new();
    for operation in operations {
        match operation {
            PlanOperation::Add {
                title, description, ..
            } => {
                texts.push(("title", title.as_str()));
                if let Some(text) = description {
                    texts.push(("description", text.as_str()));
                }
            }
            PlanOperation::Update {
                title,
                description,
                result,
                blocker,
                ..
            } => {
                for (field, text) in [
                    ("title", title),
                    ("description", description),
                    ("result", result),
                    ("blocker", blocker),
                ] {
                    if let Some(text) = text {
                        texts.push((field, text.as_str()));
                    }
                }
            }
            _ => {}
        }
    }
    texts
}

#[cfg(test)]
mod tests;
