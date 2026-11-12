//! What the GUI shows of a lane (ADR 0044): its brief, policy, branch/worktree
//! declaration and the status of its latest Report, all read from the lane ledger.
//!
//! `ready` is shown as an **unverified** claim: a lane saying it is done is not a
//! verified close (ADR 0044 §9), and no later phase's verification exists yet.

use event_bus::lane::{LanePolicy, LaneReport, LaneStatus};
use storage::lane_delivery::{LaneWakeGuard, QuestionCapacity, QuestionCapacityBlocked};
use storage::lanes::LaneRecord;

/// Where a lane's questions and notices go in Phase 1c (ADR 0044 §7, §8), as one line for the
/// lane card. `delivery_bound` is whether this window installed the runtime's durable delivery
/// host: without it the runtime routes every question to the user and notifies nobody, so the
/// words must not promise more than that.
#[must_use]
pub const fn routing_notice(policy: LanePolicy, delivery_bound: bool) -> &'static str {
    match (delivery_bound, policy) {
        (false, _) => {
            "Lane delivery is not running in this window: this lane's questions come to you, and its lead is neither notified nor woken."
        }
        (true, LanePolicy::Attached) => {
            "Questions go to the lead first, which answers them or passes them to you. Questions, ready and failed reports are saved for the lead and wake it once it has finished."
        }
        (true, LanePolicy::Summary) => {
            "Questions come to you. The lead only receives a short ready or failed report; questions do not wake it."
        }
        (true, LanePolicy::Detached) => {
            "Questions come to you. This lane is detached: its lead receives no reports and is never woken."
        }
    }
}

/// What the runtime keeps about delivery for a lane, as last read. An observation only: the
/// runtime and storage own the outbox, guard and Stop, and this holds no state of its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaneDeliveryView {
    /// The bounded diagnostic left when a question could not be created for capacity.
    pub capacity_blocked: Option<QuestionCapacityBlocked>,
    /// The lane's per-source wake guard.
    pub guard: LaneWakeGuard,
    /// The lead's destination was stopped by the user: notices are held, never lost.
    pub lead_stopped: bool,
}

impl LaneDeliveryView {
    /// Why question creation is blocked, in one sentence; `None` when it is not.
    #[must_use]
    pub fn capacity_line(&self) -> Option<String> {
        let blocked = self.capacity_blocked.as_ref()?;
        let scope = match blocked.capacity {
            QuestionCapacity::Requester => "this lane already has",
            QuestionCapacity::Global => "the workbench already has",
        };
        Some(format!(
            "Question capacity reached: {scope} {} unanswered questions, so new ones are refused ({} attempt(s)). Answer some to continue; nothing is retried automatically.",
            blocked.capacity.limit(),
            blocked.attempts
        ))
    }

    /// Why the lead is no longer woken for this lane, in one sentence; `None` when it is.
    #[must_use]
    pub fn suppressed_line(&self) -> Option<String> {
        self.guard.suppressed.then(|| {
            format!(
                "Wake paused: {} wakes in a row brought no new progress. Its notices are kept; resume to deliver them again.",
                self.guard.no_progress_batches
            )
        })
    }
}

/// A lane's ledger record with its latest Report, as last read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneDetail {
    pub record: LaneRecord,
    pub latest_report: Option<LaneReport>,
    pub delivery: LaneDeliveryView,
    /// The close that stands for the lane, when its lead or a person ended it.
    pub close: Option<event_bus::lane::LaneCloseResult>,
}

/// The status as the UI words it: `ready` is only a claim.
#[must_use]
pub const fn status_label(status: LaneStatus) -> &'static str {
    match status {
        LaneStatus::Working => "working",
        LaneStatus::Waiting => "waiting",
        LaneStatus::Blocked => "blocked",
        LaneStatus::Ready => "ready (unverified)",
        LaneStatus::Failed => "failed",
    }
}

impl LaneDetail {
    /// The status of the latest Report; `None` before the lane reported.
    #[must_use]
    pub fn status(&self) -> Option<LaneStatus> {
        self.latest_report.as_ref().map(|report| report.status)
    }

    /// One short line for a tooltip or the row: policy and latest status.
    #[must_use]
    pub fn headline(&self) -> String {
        let policy = self.record.policy.as_str();
        match self.status() {
            Some(status) => format!("Lane · {policy} · {}", status_label(status)),
            None => format!("Lane · {policy} · no report yet"),
        }
    }

    /// The branch, worktree and notes the lane declared (a declaration only: nothing
    /// is reserved or granted by it), one `label: value` per item.
    #[must_use]
    pub fn workspace_lines(&self) -> Vec<String> {
        let workspace = &self.record.brief.workspace;
        [
            ("Branch", workspace.branch.as_deref()),
            ("Worktree", workspace.worktree.as_deref()),
            ("Notes", workspace.notes.as_deref()),
        ]
        .into_iter()
        .filter_map(|(label, value)| Some(format!("{label}: {}", value?.trim())))
        .collect()
    }
}
