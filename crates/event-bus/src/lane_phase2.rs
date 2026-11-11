//! Phase 2 domain values; no DB rows, leases, permits or wire adapters.
use super::{ArtifactRevision, QuestionResponder};
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
    /// The latest Ready was reported for an older brief revision.
    StaleBrief,
    /// New requests or reports arrived after the revision the closer observed.
    StaleRequest,
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
            Self::StaleBrief => "stale_brief",
            Self::StaleRequest => "stale_request",
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
    /// Number of acceptance criteria of the brief the Reviewer was started with. A
    /// later brief revision does not change what this review is checked against.
    #[serde(default)]
    pub criteria_count: Option<u32>,
    /// Observations that could not be made (or did not match) when the review was
    /// started: informational, never a reason to refuse the review or a close.
    #[serde(default)]
    pub caveats: Vec<String>,
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
/// Who ended the lane's management, and on whose judgement. Every kind only ends the
/// ledger entry: none of them stops a run or deletes a thread or worktree, and none
/// is a quality guarantee made by the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneCloseKind {
    /// The current lead judged the lane done.
    LeadClosed,
    /// A human judged the lane done (host operation, never an agent tool).
    HumanClosed,
    /// A human ended the lane without claiming it is done.
    Discarded,
}
impl LaneCloseKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LeadClosed => "lead_closed",
            Self::HumanClosed => "human_closed",
            Self::Discarded => "discarded",
        }
    }
}
/// Who decided a close, from the runtime's trusted facts at the time of the close; never
/// from tool arguments or message text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaneCloseActor {
    /// The lead thread and the root run that was running it when it closed.
    Lead {
        thread_id: String,
        root_run_id: String,
    },
    /// A human, through the host's authenticated surface.
    Human {
        surface: String,
        client_id: Option<String>,
    },
}
/// What the ledger holds about the lane's PR when the close was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanePrContextStatus {
    NotLinked,
    /// Linked, with no stored facts yet.
    NoSnapshot,
    /// The latest fetch failed; any evidence is from an earlier success.
    LastFetchFailed,
    Available,
}
/// Stored PR facts as they were when the close was decided: read from the ledger,
/// never fetched, and never a condition of the close.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneClosePrContext {
    pub status: LanePrContextStatus,
    pub evidence: Option<LanePrEvidence>,
}
/// Reviewer record attached to a close for reference, as it was when the close
/// was decided. It is not an observation made at close time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneCloseReview {
    pub verification_id: VerificationId,
    pub reviewer_run_id: String,
    pub state: LaneVerificationState,
    pub verdict: Option<LaneVerificationVerdict>,
    pub result_ref: Option<String>,
    pub result_digest: Option<String>,
    /// The review was started for the Ready and brief that this close targets.
    pub targets_this_ready: bool,
    pub pr_evidence: Option<LanePrEvidence>,
    /// What was (not) observed when the review started.
    pub caveats: Vec<String>,
    /// Artifacts are never re-observed by a close.
    pub artifact_rechecked_at_close: bool,
}
/// The close record as decided. Artifact fields are the Ready report's own and are
/// not re-verified by the close.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneCloseResult {
    pub lane_id: String,
    pub close_id: String,
    pub kind: LaneCloseKind,
    /// The Ready this close is about; `None` for a human close without one.
    #[serde(default)]
    pub ready_seq: Option<u64>,
    /// The request revision observed by the closer.
    pub closed_revision: u64,
    #[serde(default)]
    pub management_revision: u64,
    #[serde(default)]
    pub brief_revision: u64,
    /// The brief revision the Ready was reported for.
    #[serde(default)]
    pub report_brief_revision: Option<u64>,
    #[serde(default)]
    pub artifact_refs: Vec<String>,
    #[serde(default)]
    pub artifact_revision: Option<ArtifactRevision>,
    /// The judgement of the lead or human, as written.
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub review: Option<LaneCloseReview>,
    /// Stored PR facts as the ledger held them; present for every close.
    #[serde(default)]
    pub pr_context: Option<LaneClosePrContext>,
    /// Who decided it. Absent only for a close migrated from before it was recorded.
    #[serde(default)]
    pub closed_by: Option<LaneCloseActor>,
    #[serde(default)]
    pub at_ns: i64,
    pub outcome: LaneCloseOutcome,
}
/// Why a Ready is still waiting for the lead's decision. Informative only: none of
/// these blocks a close, and none of them is a stall of the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneReadyState {
    /// The report carries no immutable artifact revision and nothing was reviewed.
    NoEvidence,
    /// A fixed artifact is named but no independent review exists.
    Unreviewed,
    /// A Reviewer was started and has not finished.
    ReviewInProgress,
    /// The finished review rejected the Ready.
    ReviewRejected,
    /// The Reviewer ended without a complete submission (failed, cancelled, stopped or
    /// finished silently). Not a rejection: the lead may start another review.
    ReviewIncomplete,
    /// The review approved; the lead has not closed.
    AwaitingClose,
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
    /// The lane reported Ready and the lead has not closed it. Not a stall.
    ReadyUnclosed {
        ready_seq: u64,
        readiness: LaneReadyState,
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
