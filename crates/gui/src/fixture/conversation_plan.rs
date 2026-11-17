//! Fixed conversation plans for headless capture and tests: no plan, a
//! hierarchy in mixed progress, and the same plan with a long selected task
//! and two pages of change history.

use workbench_protocol::namespaces::execution::plan::{
    PlanChange, PlanChangeCause, PlanHistoryChange, PlanHistoryEntry, PlanHistoryPage,
    PlanNodeDetail, PlanNodeView, PlanProgress, PlanStatusValues, PlanSummary, PlanTaskRead,
    PlanTaskStatus, PlanTreePage,
};
use workbench_protocol::namespaces::{ExecutionQuery, ExecutionReply};
use workbench_protocol::wire::{Revision, Sequence};

use crate::model::conversation_plan::{PlanReader, ReadFailure};

/// Which plan [`FixturePlanReader`] answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanScenario {
    /// The conversation has no task.
    Unplanned,
    /// Three parents and eight leaves in every state.
    Mixed,
    /// [`Self::Mixed`] with the blocked task selected and fourteen revisions
    /// of history.
    Detailed,
}

impl PlanScenario {
    pub const ALL: [Self; 3] = [Self::Unplanned, Self::Mixed, Self::Detailed];

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "unplanned" => Some(Self::Unplanned),
            "mixed" => Some(Self::Mixed),
            "detailed" => Some(Self::Detailed),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Unplanned => "unplanned",
            Self::Mixed => "mixed",
            Self::Detailed => "detailed",
        }
    }

    /// The task shown selected in the panel.
    pub const fn selected_task(self) -> Option<&'static str> {
        match self {
            Self::Detailed => Some(BLOCKED_TASK),
            _ => None,
        }
    }

    const fn revision(self) -> u64 {
        match self {
            Self::Unplanned => 0,
            Self::Mixed | Self::Detailed => HISTORY_LEN,
        }
    }
}

const PLAN_ID: &str = "plan-demo";
const HISTORY_LEN: u64 = 14;
/// 2026-10-09 13:00 UTC; revisions are saved three minutes apart.
const FIRST_SAVE_MS: i64 = 1_791_550_800_000;

fn saved_at(revision: u64) -> i64 {
    FIRST_SAVE_MS + (revision as i64 - 1) * 180_000
}
const BLOCKED_TASK: &str = "verify-capture";

type Row = (
    &'static str,
    Option<&'static str>,
    &'static str,
    u8,
    Option<PlanTaskStatus>,
);

const ROWS: [Row; 11] = [
    ("inspect", None, "Inspect the composer band", 1, None),
    (
        "inspect-goal",
        Some("inspect"),
        "Read the goal row",
        1,
        Some(PlanTaskStatus::Completed),
    ),
    (
        "inspect-theme",
        Some("inspect"),
        "Collect theme tokens",
        1,
        Some(PlanTaskStatus::Completed),
    ),
    ("band", None, "Build the progress band", 1, None),
    (
        "band-bar",
        Some("band"),
        "Draw the status bar",
        2,
        Some(PlanTaskStatus::Completed),
    ),
    (
        "band-open",
        Some("band"),
        "Open the plan panel",
        1,
        Some(PlanTaskStatus::InProgress),
    ),
    (
        "band-tooltips",
        Some("band"),
        "Describe every icon",
        1,
        Some(PlanTaskStatus::Pending),
    ),
    (
        "band-empty",
        Some("band"),
        "Show an empty plan",
        1,
        Some(PlanTaskStatus::Pending),
    ),
    ("verify", None, "Verify and capture", 1, None),
    (
        BLOCKED_TASK,
        Some("verify"),
        "Capture every theme",
        2,
        Some(PlanTaskStatus::Blocked),
    ),
    (
        "verify-tests",
        Some("verify"),
        "Run the headless tests",
        1,
        Some(PlanTaskStatus::Interrupted),
    ),
];

fn share(status: PlanTaskStatus, ppm: u32) -> PlanStatusValues {
    let mut values = PlanStatusValues {
        pending: 0,
        in_progress: 0,
        completed: 0,
        blocked: 0,
        interrupted: 0,
    };
    match status {
        PlanTaskStatus::Pending => values.pending = ppm,
        PlanTaskStatus::InProgress => values.in_progress = ppm,
        PlanTaskStatus::Completed => values.completed = ppm,
        PlanTaskStatus::Blocked => values.blocked = ppm,
        PlanTaskStatus::Interrupted => values.interrupted = ppm,
    }
    values
}

/// The server's figures for the parents: shares of their leaves' weight.
fn parent_shares(id: &str) -> PlanStatusValues {
    match id {
        "inspect" => share(PlanTaskStatus::Completed, 1_000_000),
        "band" => PlanStatusValues {
            completed: 400_000,
            in_progress: 200_000,
            pending: 400_000,
            ..PlanStatusValues::default()
        },
        _ => PlanStatusValues {
            blocked: 666_667,
            interrupted: 333_333,
            ..PlanStatusValues::default()
        },
    }
}

fn progress() -> PlanProgress {
    PlanProgress {
        node_count: 11,
        leaf_count: 8,
        leaf_counts: PlanStatusValues {
            pending: 2,
            in_progress: 1,
            completed: 3,
            blocked: 1,
            interrupted: 1,
        },
        weighted_ppm: PlanStatusValues {
            pending: 200_000,
            in_progress: 100_000,
            completed: 400_000,
            blocked: 200_000,
            interrupted: 100_000,
        },
    }
}

fn nodes() -> Vec<PlanNodeView> {
    ROWS.iter()
        .map(|&(task_id, parent, title, weight, status)| {
            let siblings = ROWS.iter().filter(|row| row.1 == parent);
            let position = siblings.take_while(|row| row.0 != task_id).count();
            PlanNodeView {
                task_id: task_id.into(),
                parent_id: parent.map(str::to_owned),
                position: u32::try_from(position).unwrap_or(u32::MAX),
                depth: u32::from(parent.is_some()),
                title: title.into(),
                weight,
                status,
                child_count: u32::try_from(
                    ROWS.iter().filter(|row| row.1 == Some(task_id)).count(),
                )
                .unwrap_or(u32::MAX),
                progress_ppm: status
                    .map_or_else(|| parent_shares(task_id), |status| share(status, 1_000_000)),
            }
        })
        .collect()
}

fn detail(node: PlanNodeView, long: bool) -> PlanNodeDetail {
    let (description, result, blocker) = if long && node.task_id == BLOCKED_TASK {
        (
            "Capture the composer band and the plan panel in the Graphite, High Contrast \
             and Tokyo Night themes, once for a conversation without a plan, once for a plan in \
             mixed progress and once with a long selected task and its change history. \
             Each capture must keep the band on one line at the default window width and \
             the detail sections readable without horizontal scrolling."
                .to_owned(),
            "Six of the nine captures are done; the band and the tree read the same \
             figures as the server's summary."
                .to_owned(),
            "The capture host lost its display while rendering the Tokyo Night theme; the \
             remaining captures wait until the display is back."
                .to_owned(),
        )
    } else {
        (format!("{}.", node.title), String::new(), String::new())
    };
    let child_ids = ROWS
        .iter()
        .filter(|row| row.1 == Some(node.task_id.as_str()))
        .map(|row| row.0.to_owned())
        .collect();
    PlanNodeDetail {
        task_id: node.task_id,
        parent_id: node.parent_id,
        position: node.position,
        depth: node.depth,
        title: node.title,
        description,
        result,
        blocker,
        weight: node.weight,
        status: node.status,
        child_ids,
        progress_ppm: node.progress_ppm,
    }
}

fn update(task_id: &str, status: PlanTaskStatus) -> PlanChange {
    PlanChange::Updated {
        task_id: task_id.into(),
        title: None,
        description: None,
        result: None,
        blocker: None,
        weight: None,
        status: Some(status),
    }
}

fn added(row: &Row) -> PlanChange {
    PlanChange::Added {
        task_id: row.0.into(),
        parent_id: row.1.map(str::to_owned),
        position: 0,
        title: row.2.into(),
        description: String::new(),
        weight: row.3,
        status: PlanTaskStatus::Pending,
    }
}

/// Revisions 1..=14, oldest first.
fn history() -> Vec<PlanHistoryEntry> {
    use PlanTaskStatus::{Blocked, Completed, InProgress, Interrupted};
    let mut changes: Vec<(PlanChangeCause, Vec<PlanChange>)> = vec![(
        PlanChangeCause::ModelUpdate,
        ROWS.iter().map(added).collect(),
    )];
    let steps = [
        ("inspect-goal", InProgress),
        ("inspect-goal", Completed),
        ("inspect-theme", InProgress),
        ("inspect-theme", Completed),
        ("band-bar", InProgress),
        ("band-bar", Completed),
        ("verify-tests", InProgress),
        ("verify-tests", Interrupted),
        (BLOCKED_TASK, InProgress),
        (BLOCKED_TASK, Blocked),
        ("band-open", InProgress),
    ];
    for (task, status) in steps {
        let cause = if status == Interrupted {
            PlanChangeCause::RuntimeInterrupt
        } else {
            PlanChangeCause::ModelUpdate
        };
        changes.push((cause, vec![update(task, status)]));
    }
    changes.push((
        PlanChangeCause::ModelUpdate,
        vec![PlanChange::Moved {
            task_id: "band-empty".into(),
            parent_id: Some("band".into()),
            position: 3,
        }],
    ));
    changes.push((
        PlanChangeCause::ModelUpdate,
        vec![PlanChange::SubtreeRemoved {
            task_id: "band-draft".into(),
            task_ids: vec!["band-draft".into()],
        }],
    ));
    changes
        .into_iter()
        .zip(1..)
        .map(|((cause, changes), revision)| PlanHistoryEntry {
            revision: Revision(revision),
            event_id: Sequence(100 + revision),
            change: Some(PlanHistoryChange {
                cause,
                source_run_id: Some("run-2".into()),
                changes,
                progress: progress(),
            }),
            updated_at_unix_ms: Some(saved_at(revision)),
        })
        .collect()
}

/// Answers every plan query with the scenario's plan, for any conversation.
pub struct FixturePlanReader {
    scenario: PlanScenario,
}

impl FixturePlanReader {
    pub const fn new(scenario: PlanScenario) -> Self {
        Self { scenario }
    }

    fn planned(&self) -> bool {
        self.scenario != PlanScenario::Unplanned
    }
}

impl PlanReader for FixturePlanReader {
    fn read(&self, query: &ExecutionQuery) -> Result<ExecutionReply, ReadFailure> {
        let revision = Revision(self.scenario.revision());
        let plan_id = self.planned().then(|| PLAN_ID.to_owned());
        let progress = if self.planned() {
            progress()
        } else {
            PlanProgress::default()
        };
        match query {
            ExecutionQuery::GetConversationPlan(target) => {
                Ok(ExecutionReply::ConversationPlan(Box::new(PlanSummary {
                    thread: target.thread.clone(),
                    plan_id,
                    revision,
                    progress,
                    updated_at_unix_ms: (revision.0 > 0).then(|| saved_at(revision.0)),
                })))
            }
            ExecutionQuery::ReadConversationPlanTree(_) => Ok(
                ExecutionReply::ConversationPlanTree(Box::new(PlanTreePage {
                    plan_id,
                    revision,
                    progress,
                    nodes: if self.planned() { nodes() } else { Vec::new() },
                    next_cursor: None,
                })),
            ),
            ExecutionQuery::GetConversationPlanTask(query) => {
                let node = nodes()
                    .into_iter()
                    .find(|node| node.task_id == query.task_id)
                    .filter(|_| self.planned())
                    .ok_or(ReadFailure::NotFound)?;
                Ok(ExecutionReply::ConversationPlanTask(Box::new(
                    PlanTaskRead {
                        plan_id: PLAN_ID.into(),
                        revision,
                        node: detail(node, self.scenario == PlanScenario::Detailed),
                    },
                )))
            }
            ExecutionQuery::ReadConversationPlanHistory(query) => {
                let after = query.after_revision.map_or(0, |revision| revision.0);
                let limit = query.limit.map_or(usize::MAX, |limit| limit as usize);
                let all: Vec<_> = if self.planned() {
                    history()
                        .into_iter()
                        .filter(|entry| entry.revision.0 > after)
                        .collect()
                } else {
                    Vec::new()
                };
                let more = all.len() > limit;
                let entries: Vec<_> = all.into_iter().take(limit).collect();
                let next_after_revision = more
                    .then(|| entries.last().map(|entry| entry.revision))
                    .flatten();
                Ok(ExecutionReply::ConversationPlanHistory(Box::new(
                    PlanHistoryPage {
                        plan_id,
                        revision,
                        entries,
                        next_after_revision,
                    },
                )))
            }
            _ => Err(ReadFailure::Other("not a plan query".into())),
        }
    }
}
