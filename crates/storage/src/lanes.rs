//! The lane ledger (ADR 0044 Phase 1b): the durable contract for lanes.
//!
//! A lane is an independent Orchestrator thread opened by a lead. This module is the
//! public surface of the ledger that records what the thread row cannot:
//!
//! - **identity and origin** (immutable): the lane and thread IDs, the thread and run
//!   that opened it, the creation key that makes a retry idempotent;
//! - **management** (mutable, with a revision): the current lead and the policy.
//!   Detaching a lane changes only these; the origin stays. `threads.parent_thread_id`
//!   is never the management relation (a host mirror cannot erase it);
//! - the **brief** and its revision, the **start seed** and the execution
//!   **generation** that let a crash before the root started be repaired;
//! - the **reports**: numbered by the ledger, immutable, with the latest one a pointer.
//!
//! Every mutation is a compare-and-swap on the revision the caller read
//! (`StorageError::StaleRevision`), runs in one transaction and returns the new state.
//! The runtime publishes the matching event (`LaneRecord::created_event` and its
//! siblings) **only after** the call returned `Ok`; the ledger never publishes.
//!
//! This module never starts/stops a run or rewrites conversation history. Durable
//! management transitions, authorization fences and projection repair contracts live
//! in [`crate::lane_management`]; the host owns sidebar projection and family locks.

use event_bus::LedgerEvent;
use serde::{Deserialize, Serialize};

pub use event_bus::{
    LANE_ARTIFACT_REFS_MAX, LANE_LIST_PAGE_MAX, LANE_LIST_PAGE_MAX_BYTES, LANE_QUESTION_REFS_MAX,
    LANE_REPORT_MAX_BYTES, LANE_SUMMARY_MAX_BYTES, LaneBrief, LaneInput, LaneInputKind, LanePolicy,
    LaneReport, LaneReportInput, LaneReportProducer, LaneScope, LaneStatus, LaneValidationError,
    LaneValidationKind, LaneWorkspaceDeclaration,
};

pub use crate::error::{LaneConflictReason, LaneCursorError};

/// Largest start seed, in UTF-8 bytes.
pub const LANE_START_SEED_MAX_BYTES: usize = 64 * 1024;
/// Largest identifier or key (lane, thread, run, creation key, delivery key), in bytes.
pub const LANE_ID_MAX_BYTES: usize = 256;
/// Most reports one [`Database::lane_reports`](crate::Database::lane_reports) call returns.
pub const LANE_REPORTS_PAGE_MAX: usize = 50;
/// Longest objective shown by [`LaneListItem::objective_preview`], in bytes.
pub const LANE_OBJECTIVE_PREVIEW_MAX_BYTES: usize = 256;
/// `lane_list` listings kept per lead before the oldest is evicted.
pub const LANE_LIST_SNAPSHOTS_PER_LEAD: usize = 16;
/// `lane_list` listings kept in all before the oldest is evicted.
pub const LANE_LIST_SNAPSHOTS_MAX: usize = 256;

/// A request to create a lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewLane {
    /// Chosen by the runtime before anything starts, so a retry names the same lane.
    pub lane_id: String,
    /// The thread the lane root lives in.
    pub thread_id: String,
    /// The thread of the lead that opens the lane. Immutable.
    pub origin_thread_id: String,
    /// The lead run that opens the lane. Immutable.
    pub origin_run_id: String,
    /// Idempotency key, unique per origin thread (for example the tool call ID). A
    /// second request with the same key and the same brief returns the first lane; the
    /// same key with another brief is [`LaneConflictReason::CreationKeyReused`].
    pub creation_key: String,
    /// `report_policy` decides the initial policy: attached and summary start under the
    /// origin thread as lead, detached starts without a lead.
    pub brief: LaneBrief,
    /// What the runtime needs to start the root (opaque to the ledger); kept so a crash
    /// before the start loses nothing. Never sent to a model by the ledger.
    pub start_seed: String,
    /// Written to the lane's thread row when it is created, as the runtime would.
    pub project_id: Option<String>,
    pub project_root: Option<String>,
    pub title: Option<String>,
    /// Unix nanoseconds, from the caller's clock.
    pub at_ns: i64,
}

/// A stored lane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneRecord {
    pub lane_id: String,
    pub thread_id: String,
    pub origin_thread_id: String,
    pub origin_run_id: String,
    pub creation_key: String,
    pub scope: LaneScope,
    /// The lead that manages the lane now; `None` once detached.
    pub lead_thread_id: Option<String>,
    pub policy: LanePolicy,
    /// Raised by every change of lead or policy. Starts at 1.
    pub management_revision: u64,
    /// Membership epoch of the current independent family; None while managed.
    pub detached_since_management_revision: Option<u64>,
    /// The brief; its `report_policy` is the current [`Self::policy`].
    pub brief: LaneBrief,
    /// Raised by every changed brief. Starts at 1.
    pub brief_revision: u64,
    pub start_seed: String,
    /// A cancelled or locally failed creation intent. Never recovered automatically.
    pub start_blocked_reason: Option<String>,
    /// The current root run; `None` until the first start.
    pub root_run_id: Option<String>,
    /// 0 until the root started, then raised by every start or restart.
    pub generation: u64,
    /// The sequence of the latest report; 0 when there is none.
    pub latest_report_seq: u64,
    pub created_at_ns: i64,
    pub updated_at_ns: i64,
    /// The latest real activity (creation, a report, an explicit touch). Reads,
    /// monitor ticks and redraws never move it.
    pub last_activity_at_ns: i64,
}

/// The `start_blocked_reason` of an intent the operator stopped before its root started.
/// Unlike a cancellation or a local failure it can be resumed explicitly
/// ([`StorageHandle::resume_lane_start`](crate::StorageHandle::resume_lane_start)); a
/// restart or a crash scan never recovers it.
pub const LANE_START_OPERATOR_STOP: &str = "operator_stop: stopped before root start";

impl LaneRecord {
    /// Whether the creation intent was stopped by the operator and may be resumed.
    #[must_use]
    pub fn is_operator_stopped(&self) -> bool {
        self.start_blocked_reason.as_deref() == Some(LANE_START_OPERATOR_STOP)
    }

    /// Whether the root run was ever started.
    #[must_use]
    pub const fn is_started(&self) -> bool {
        self.generation > 0
    }

    /// The event for a creation that committed. Publish it only after the call that
    /// returned this record.
    #[must_use]
    pub fn created_event(&self) -> LedgerEvent {
        LedgerEvent::LaneCreated {
            lane_id: self.lane_id.clone(),
            thread_id: self.thread_id.clone(),
            origin_run_id: self.origin_run_id.clone(),
            origin_thread_id: self.origin_thread_id.clone(),
            lead_thread_id: self.lead_thread_id.clone(),
            policy: self.policy,
            management_revision: self.management_revision,
        }
    }

    /// The event for a report that committed. `run_id` is the producing run, if any.
    #[must_use]
    pub fn report_appended_event(
        &self,
        report: &LaneReport,
        run_id: Option<String>,
    ) -> LedgerEvent {
        LedgerEvent::LaneReportAppended {
            lane_id: self.lane_id.clone(),
            thread_id: self.thread_id.clone(),
            run_id,
            report_seq: report.report_seq,
            status: report.status,
            needs_human: report.needs_human,
        }
    }

    /// The event for a management change that committed.
    #[must_use]
    pub fn management_changed_event(&self, run_id: Option<String>) -> LedgerEvent {
        LedgerEvent::LaneManagementChanged {
            lane_id: self.lane_id.clone(),
            thread_id: self.thread_id.clone(),
            run_id,
            lead_thread_id: self.lead_thread_id.clone(),
            policy: self.policy,
            management_revision: self.management_revision,
        }
    }

    /// The event for a brief revision that committed.
    #[must_use]
    pub fn brief_revised_event(&self, run_id: Option<String>) -> LedgerEvent {
        LedgerEvent::LaneBriefRevised {
            lane_id: self.lane_id.clone(),
            thread_id: self.thread_id.clone(),
            run_id,
            brief_revision: self.brief_revision,
        }
    }
}

/// The result of [`StorageHandle::create_lane`](crate::StorageHandle::create_lane).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneCreation {
    pub lane: LaneRecord,
    /// `false` when the request repeated an earlier one: the existing lane is returned
    /// and no event is due.
    pub created: bool,
}

/// The result of a revision-checked update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneUpdate {
    pub lane: LaneRecord,
    /// `false` when the request asked for what is already stored: the revision did not
    /// move and no event is due.
    pub changed: bool,
}

/// The result of [`StorageHandle::append_lane_report`](crate::StorageHandle::append_lane_report).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneReportAppended {
    pub lane: LaneRecord,
    pub report: LaneReport,
}

/// Which lanes `lane_list` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneListRequest {
    /// The lead whose managed lanes are listed. A lane that stops being managed by it
    /// (a detach) disappears from later pages; callers re-check read authority on every
    /// page.
    pub lead_thread_id: String,
    /// Only lanes under this policy; `None` for every managed lane.
    pub policy: Option<LanePolicy>,
    /// The `next_cursor` of the previous page; `None` starts a listing.
    pub cursor: Option<String>,
    /// Clamped to `1..=`[`LANE_LIST_PAGE_MAX`].
    pub limit: usize,
}

/// One entry of a `lane_list` page. Bounded: the brief is shown by its objective's
/// first bytes and its workspace declaration (for comparing branches and worktrees).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaneListItem {
    pub lane_id: String,
    pub thread_id: String,
    pub policy: LanePolicy,
    pub management_revision: u64,
    pub brief_revision: u64,
    /// The start of the objective, cut at a character boundary. A preview, not the brief.
    pub objective_preview: String,
    pub workspace: LaneWorkspaceDeclaration,
    pub root_run_id: Option<String>,
    pub generation: u64,
    pub created_at_ns: i64,
    pub last_activity_at_ns: i64,
    pub latest_report: Option<LaneReport>,
}

/// One page of `lane_list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaneListPage {
    /// At most [`LANE_LIST_PAGE_MAX`] entries; the whole page, serialized, is at most
    /// [`LANE_LIST_PAGE_MAX_BYTES`] bytes (cursor and fixed fields counted).
    pub items: Vec<LaneListItem>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
    /// The ledger revision the listing describes. All pages of one listing report the
    /// revision of its first page.
    pub snapshot_revision: u64,
}

impl crate::Database {
    /// One lane, unless its thread was deleted by retention.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn lane(&self, lane_id: &str) -> Result<Option<LaneRecord>, crate::StorageError> {
        crate::repo::lanes::get(&self.conn, lane_id)
    }

    /// The lane whose root lives in `thread_id`, if the thread is a lane.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn lane_by_thread(
        &self,
        thread_id: &str,
    ) -> Result<Option<LaneRecord>, crate::StorageError> {
        crate::repo::lanes::get_by_thread(&self.conn, thread_id)
    }

    /// Reports of a lane after `after_seq`, oldest first, at most
    /// [`LANE_REPORTS_PAGE_MAX`].
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn lane_reports(
        &self,
        lane_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<LaneReport>, crate::StorageError> {
        crate::repo::lanes::reports(&self.conn, lane_id, after_seq, limit)
    }

    /// Recoverable creation intents whose root never started, in lane ID order
    /// (at most 200). Explicitly cancelled or locally failed intents are excluded.
    ///
    /// # Errors
    /// Returns an error if SQLite access or row decoding fails.
    pub fn unstarted_lanes(&self, limit: usize) -> Result<Vec<LaneRecord>, crate::StorageError> {
        crate::repo::lanes::unstarted(&self.conn, limit)
    }
}
