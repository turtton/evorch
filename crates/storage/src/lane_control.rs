//! Atomic Phase 2 inputs. Runtime authorizes callers/observes artifacts;
//! storage compares snapshots and commits receipts with the mutation.
pub use event_bus::lane::*;
#[derive(Debug, Clone)]
pub struct ForwardLaneQuestion {
    pub caller_root: String,
    pub question_id: String,
    pub expected_lane_generation: u64,
    pub expected_management_revision: u64,
    pub operation_id: String,
    pub at_ns: i64,
}
#[derive(Debug, Clone)]
pub struct BeginLaneVerification {
    pub verification_id: VerificationId,
    pub lead_thread_id: String,
    pub target: LaneVerificationTarget,
    pub reviewer_run_id: String,
    pub pr_evidence: Option<LanePrEvidence>,
    /// What could not be observed when the review was started (bounded, informational).
    pub caveats: Vec<String>,
    pub at_ns: i64,
}
/// Who asks for the close. The lead's identity is the runtime's trusted fact, never
/// a tool argument; storage compares it with the lane in the close transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneCloser {
    /// The current lead thread and the root run that is running it right now.
    Lead {
        thread_id: String,
        root_run_id: String,
    },
    /// A human host operation. Only runtime's human context can construct this; the
    /// surface and client are the host's authenticated provenance.
    Human {
        surface: String,
        client_id: Option<String>,
    },
}
/// A close is a judgement of the lead (or a human) about management of the lane.
/// Nothing here observes artifacts, Git, GitHub or the Reviewer's run.
#[derive(Debug, Clone)]
pub struct CloseLane {
    pub closer: LaneCloser,
    pub kind: LaneCloseKind,
    pub lane_id: String,
    /// Required for a lead close. A human may close without a Ready.
    pub ready_seq: Option<u64>,
    pub expected_management_revision: u64,
    pub expected_request_revision: u64,
    /// The closer's reason (non-empty, bounded).
    pub note: String,
    /// Optional Reviewer record attached for reference only.
    pub verification_id: Option<VerificationId>,
    pub at_ns: i64,
}
/// The revisions a close compares, as they are now. Returned so a closer whose
/// observation went stale can look again; it is not an authority to close.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LaneCloseContext {
    pub lane_id: String,
    pub management_revision: u64,
    pub request_revision: u64,
    pub brief_revision: u64,
    /// The latest report's sequence when it is a Ready for the current management.
    pub latest_ready_seq: Option<u64>,
    /// The brief revision that Ready was reported for.
    pub ready_brief_revision: Option<u64>,
}
/// What a person reads before ending a lane: the brief, the Ready the close would be about,
/// reference facts (never conditions) and the revisions the close compares. All of it comes
/// from one snapshot of the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneClosePreview {
    pub lane: crate::lanes::LaneRecord,
    pub context: LaneCloseContext,
    /// The latest report, when it is a Ready for the current management.
    pub ready: Option<LaneReport>,
    /// Review state of that Ready; `None` when there is no Ready.
    pub readiness: Option<LaneReadyState>,
    pub pr_context: LaneClosePrContext,
    /// The close that stands now, if any.
    pub current_close: Option<LaneCloseResult>,
    /// Questions raised by the lane's own runs that nobody has answered, whether or not they
    /// were forwarded to the lead yet. A close leaves them as they are; this only tells the
    /// person they exist.
    pub open_question_count: u64,
}
/// Longest note a close accepts, in bytes.
pub const LANE_CLOSE_NOTE_MAX_BYTES: usize = 2048;
/// Most review caveats a verification or close snapshot keeps, each at most 256 bytes.
pub const LANE_CAVEATS_MAX: usize = 16;
/// B supplies an owned observation. Storage never decides timer eligibility.
#[derive(Debug, Clone)]
pub struct ObserveLaneStall {
    pub lane_id: String,
    pub management_revision: u64,
    pub owner: crate::lane_delivery::LaneDeliveryOwner,
    pub progress_anchor: LaneProgressAnchor,
    pub classification: LaneStallClassification,
    pub at_ns: i64,
}

impl crate::Database {
    /// The lane attention list of one lead, derived from the ledger. Read only: it
    /// marks nothing as seen and appends no input.
    ///
    /// # Errors
    /// `StaleCursor` when `cursor` was cut from another filter or state, and SQLite
    /// or decoding failures.
    pub fn lane_attention(
        &self,
        filter: LaneAttentionFilter,
        cursor: Option<String>,
        limit: usize,
    ) -> Result<LaneAttentionPage, crate::StorageError> {
        crate::repo::lane_control::attention(&self.conn, filter, cursor, limit)
    }

    /// What a person reads before ending a lane, with the revisions the close compares,
    /// from one snapshot. Read only; nothing here is a condition of the close.
    ///
    /// # Errors
    /// `NotAttached` for a detached lane, `LaneNotFound`, and SQLite or decoding failures.
    pub fn lane_close_preview(
        &self,
        lane_id: &str,
    ) -> Result<LaneClosePreview, crate::StorageError> {
        let tx = self.conn.unchecked_transaction()?;
        crate::repo::lane_control::close_preview(&tx, lane_id)
    }

    /// The close that stands for a managed lane now; `None` when there is none or the lane
    /// is detached. Read only.
    ///
    /// # Errors
    /// SQLite or decoding failures.
    pub fn current_lane_close(
        &self,
        lane_id: &str,
    ) -> Result<Option<LaneCloseResult>, crate::StorageError> {
        crate::repo::lane_control::current_close(&self.conn, lane_id)
    }
}
