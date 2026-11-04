//! What the GUI shows of a lane (ADR 0044): its brief, policy, branch/worktree
//! declaration and the status of its latest Report, all read from the lane ledger.
//!
//! Phase 1b shows `ready` as an **unverified** claim: a lane saying it is done is not a
//! verified close (ADR 0044 §9), and no later phase's verification exists yet.

use event_bus::lane::{LaneReport, LaneStatus};
use storage::lanes::LaneRecord;

/// A lane's ledger record with its latest Report, as last read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneDetail {
    pub record: LaneRecord,
    pub latest_report: Option<LaneReport>,
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
