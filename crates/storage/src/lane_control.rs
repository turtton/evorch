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
