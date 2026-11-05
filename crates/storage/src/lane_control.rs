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
    pub at_ns: i64,
}
#[derive(Debug, Clone)]
pub struct CloseLane {
    pub lead_thread_id: String,
    pub target: LaneVerificationTarget,
    pub verification_id: VerificationId,
    pub pr_evidence: Option<LanePrEvidence>,
    pub at_ns: i64,
}
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
}
