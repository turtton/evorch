//! Phase 2 domain values; no DB rows, leases, permits or wire adapters.
use super::QuestionResponder;
use serde::{Deserialize, Serialize};
pub const LANE_ATTENTION_MAX: usize = 20;
pub const LANE_ATTENTION_BYTES: usize = 16 * 1024;

/// Fixed-size identity suitable for Copy run purposes, allocated by the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VerificationId(pub [u8; 16]);
impl std::fmt::Display for VerificationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneOperationError {
    NotCurrentLead,
    NotAttached,
    StaleManagement,
    StaleReady,
    StaleVerification,
    ArtifactChanged,
    VerificationMissing,
    VerificationRejected,
    ReviewerIncomplete,
    AlreadySubmitted,
    EvidenceUnavailable,
    OwnershipLost,
    AlreadyAnswered,
    StaleRouting,
    StaleCursor,
    NotResponder,
    InvalidInput,
    StaleEpisode,
    OperationReused,
}
impl LaneOperationError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotCurrentLead => "not_current_lead",
            Self::NotAttached => "not_attached",
            Self::StaleManagement => "stale_management",
            Self::StaleReady => "stale_ready",
            Self::StaleVerification => "stale_verification",
            Self::ArtifactChanged => "artifact_changed",
            Self::VerificationMissing => "verification_missing",
            Self::VerificationRejected => "verification_rejected",
            Self::ReviewerIncomplete => "reviewer_incomplete",
            Self::AlreadySubmitted => "already_submitted",
            Self::EvidenceUnavailable => "evidence_unavailable",
            Self::OwnershipLost => "ownership_lost",
            Self::AlreadyAnswered => "already_answered",
            Self::StaleRouting => "stale_routing",
            Self::StaleCursor => "stale_cursor",
            Self::NotResponder => "not_responder",
            Self::InvalidInput => "invalid_input",
            Self::StaleEpisode => "stale_episode",
            Self::OperationReused => "operation_reused",
        }
    }
}
/// Genuine status/artifact/input progress epoch, never report text/seq/generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneProgressAnchor {
    pub epoch: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneStallClassification {
    NoReport,
    WorkStalled,
    WaitingQuestion,
    SilentDone,
    RestoreUnavailable,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneNudgeState {
    NotRequested,
    Pending,
    Accepted,
    Suppressed,
    Obsolete,
    Failed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneStallEpisode {
    pub episode_id: String,
    pub lane_id: String,
    pub management_revision: u64,
    pub revision: u64,
    pub classification: LaneStallClassification,
    pub progress_anchor: LaneProgressAnchor,
    pub first_detected_at_ns: i64,
    pub resolved_at_ns: Option<i64>,
    pub nudge_state: LaneNudgeState,
    pub notification_id: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneVerificationState {
    Pending,
    Submitted,
    Completed,
    Incomplete,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneVerificationTarget {
    pub lane_id: String,
    pub management_revision: u64,
    pub brief_revision: u64,
    pub ready_seq: u64,
    pub request_revision: u64,
    pub artifact_fingerprint: Option<String>,
}
/// Immutable version binding from the ADR 0043 accessor. Runtime owns freshness
/// and external artifact observation; storage rechecks the active link/facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanePrEvidence {
    pub repository: String,
    pub number: u64,
    pub link_id: u64,
    pub link_revision: u64,
    pub head_sha: String,
    pub success_seq: u64,
    pub last_attempt_seq: u64,
    pub fetched_at_ns: i64,
    pub facts_digest: String,
    pub local_head_sha: Option<String>,
    pub local_head_observed_at_ns: Option<i64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneVerificationVerdict {
    Approved,
    Rejected,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneCriterionCheck {
    pub criterion_index: u32,
    pub passed: bool,
    pub evidence_refs: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneVerificationSubmission {
    pub verdict: LaneVerificationVerdict,
    pub checks: Vec<LaneCriterionCheck>,
    pub result_ref: String,
    pub result_digest: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneVerification {
    pub verification_id: VerificationId,
    pub target: LaneVerificationTarget,
    pub reviewer_run_id: String,
    pub state: LaneVerificationState,
    pub pr_evidence: Option<LanePrEvidence>,
    pub submission: Option<LaneVerificationSubmission>,
    pub created_at_ns: i64,
    pub submitted_at_ns: Option<i64>,
    pub completed_at_ns: Option<i64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneCloseOutcome {
    Closed,
    AlreadyClosed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneCloseResult {
    pub lane_id: String,
    pub close_id: String,
    pub verification_ref: VerificationId,
    pub ready_seq: u64,
    pub closed_revision: u64,
    pub outcome: LaneCloseOutcome,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaneAttentionKind {
    QuestionPending {
        question_id: String,
        responder: QuestionResponder,
        routing_revision: u64,
    },
    Blocked {
        report_seq: u64,
    },
    Failed {
        report_seq: u64,
        episode_id: Option<String>,
    },
    RestoreFailed {
        destination_thread_id: String,
        failure_ref: String,
        attempts: u32,
        suppressed: bool,
    },
    ReadyUnverified {
        ready_seq: u64,
        verification_state: Option<LaneVerificationState>,
    },
    QuestionCapacityBlocked {
        capacity: LaneQuestionCapacity,
        limit: u32,
    },
    WakeSuppressed {
        guard_revision: u64,
    },
    StopPending {
        destination_thread_id: String,
        pending_count: u64,
    },
    Stall {
        episode_id: String,
        classification: LaneStallClassification,
        nudge_state: LaneNudgeState,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneQuestionCapacity {
    Requester,
    Global,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneAttentionAction {
    Answer,
    RouteToUser,
    Continue,
    Inspect,
    Verify,
    Close,
    Resume,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneAttentionItem {
    pub id: String,
    pub lane_id: String,
    pub lead_thread_id: String,
    pub management_revision: u64,
    pub revision: u64,
    pub kind: LaneAttentionKind,
    pub since_at_ns: i64,
    pub summary: String,
    pub evidence_refs: Vec<String>,
    pub available_actions: Vec<LaneAttentionAction>,
}
/// Runtime authorizes this explicit scope and fills agent lead from its root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneAttentionFilter {
    pub lead_thread_id: String,
    pub project_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneAttentionPage {
    pub items: Vec<LaneAttentionItem>,
    pub next_cursor: Option<String>,
    pub snapshot_revision: String,
}
