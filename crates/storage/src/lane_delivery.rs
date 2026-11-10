//! Durable lane routing/delivery contracts (ADR 0044 Phase 1c).
//!
//! All clocks are caller supplied Unix nanoseconds. Runtime authenticates callers,
//! resolves current roots and holds its ownership mutation guard around these APIs.
//! A successful source write is not delivery: snapshot + receipt precedes ACK.

pub use event_bus::lane::{
    ArtifactRevision, LaneDeliveryBinding, LaneNotification, LaneNotificationKind,
    QuestionRequester, QuestionResponder, QuestionRouting,
};
use serde::{Deserialize, Serialize};

pub const LANE_DELIVERY_PAGE_MAX: usize = 20;
pub const LANE_DELIVERY_PAGE_BYTES: usize = 16 * 1024;
pub const LANE_NO_PROGRESS_LIMIT: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuestionCapacity {
    Requester,
    Global,
}
impl QuestionCapacity {
    pub const fn limit(self) -> u32 {
        match self {
            Self::Requester => 32,
            Self::Global => 1024,
        }
    }
}

/// One diagnostic slot per lane, overwritten/aggregated, never an append-only log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionCapacityBlocked {
    /// Exact recipient whose owned + inherited quota rejected creation.
    pub requester_run_id: String,
    pub capacity: QuestionCapacity,
    pub attempts: u64,
    pub first_at_ns: i64,
    pub last_at_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneQuestionCreated {
    Created(Box<event_bus::UserQuestion>),
    CapacityBlocked(QuestionCapacityBlocked),
}

/// Identifies a currently bound destination root; not itself an ownership permit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneDeliveryOwner {
    pub root_run_id: String,
    pub generation: u64,
    pub ownership_generation: u64,
}

/// Durable destination binding, readable after restart or a lost bind reply.
/// This is an observation for CAS, never an ownership permit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneDeliveryDestination {
    pub thread_id: String,
    pub epoch: u64,
    pub stopped: bool,
    pub owner: Option<LaneDeliveryOwner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneDeliveryClaim<N = LaneNotification> {
    pub destination_thread_id: String,
    pub destination_epoch: u64,
    pub claim_epoch: u64,
    pub token: String,
    pub lease_until_ns: i64,
    pub owner: LaneDeliveryOwner,
    /// A completed-root wake, fixed from the claim request. Active deliveries
    /// do not count toward the per-source automatic-wake guard.
    pub completed_root_wake: bool,
    pub notifications: Vec<N>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneDeliveryNack {
    UnknownThread,
    Readonly,
    OwnershipLost,
    Stopped,
    RestoreFailed,
    StorageFailed,
    /// The referenced PR body is absent or cannot fit the immutable input batch.
    PayloadUnavailable,
}

/// Rebinding ownership/root never implicitly resumes a stopped destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneDestinationAction {
    Bind,
    Stop,
    ExplicitUserResume,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneClaimResult<N = LaneNotification> {
    Claimed(LaneDeliveryClaim<N>),
    Empty,
    Busy,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneClaimRequest {
    pub destination_thread_id: String,
    pub expected_destination_epoch: u64,
    pub owner: LaneDeliveryOwner,
    pub token: String,
    pub now_ns: i64,
    pub lease_until_ns: i64,
    /// False for a completed root when only milestones/interventions are pending.
    pub allow_non_waking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LaneWakeGuard {
    pub revision: u64,
    pub no_progress_batches: u32,
    pub artifact_fingerprint: Option<String>,
    pub failure_episode: Option<String>,
    /// Stable causal wake-batch ID, retained across receipt/ACK retries.
    pub last_wake_batch_id: Option<String>,
    pub suppressed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneGuardSignal {
    WakeBatch {
        batch_id: String,
    },
    TerminalArtifact {
        revision: Option<ArtifactRevision>,
        failure_episode: Option<String>,
    },
    RealUserInput,
    ExplicitUserResume,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneQuestionPage {
    pub questions: Vec<event_bus::UserQuestion>,
    pub next_cursor: Option<LaneQuestionCursor>,
    pub has_more: bool,
}

/// Opaque page continuation; management changes make it stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneQuestionCursor {
    pub lead_thread_id: String,
    pub management_fingerprint: String,
    pub snapshot_max_rowid: i64,
    pub after_question_id: String,
}

/// Returned projection must be published as UserQuestionUpdated after commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneQuestionMutation {
    pub lane_id: String,
    pub question_id: String,
    pub lead_thread_id: String,
    pub expected_management_revision: u64,
    pub expected_routing_revision: u64,
    pub operation_id: String,
    pub action: LaneQuestionAction,
    pub at_ns: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LaneQuestionAction {
    RouteToUser,
    Answer {
        text: String,
        origin: event_bus::AnswerOrigin,
    },
}

/// Read-only observations for a host that shows delivery state. Writes, claims and
/// resumes stay with the runtime; these never change a lane's place or its guard.
impl crate::Database {
    /// The lane's durable wake guard (`suppressed` once the no-progress limit is hit).
    ///
    /// # Errors
    /// Returns an error if the lane is unknown or SQLite access or decoding fails.
    pub fn lane_wake_guard(&self, lane_id: &str) -> Result<LaneWakeGuard, crate::StorageError> {
        crate::repo::lane_delivery::guard(&self.conn, lane_id)
    }

    /// The lane's single bounded `question_capacity_blocked` diagnostic, if any.
    ///
    /// # Errors
    /// Returns an error if the lane is unknown or SQLite access or decoding fails.
    pub fn lane_question_capacity_diagnostic(
        &self,
        lane_id: &str,
    ) -> Result<Option<QuestionCapacityBlocked>, crate::StorageError> {
        crate::repo::lane_delivery::capacity_diagnostic(&self.conn, lane_id)
    }

    /// The durable destination binding of `thread_id` (a lead), including Stop.
    ///
    /// # Errors
    /// Returns an error if SQLite access or decoding fails.
    pub fn lane_delivery_destination(
        &self,
        thread_id: &str,
    ) -> Result<Option<LaneDeliveryDestination>, crate::StorageError> {
        crate::repo::lane_delivery::destination(&self.conn, thread_id)
    }
}

/// Maximum number of actual failed restores of one unchanged source.
pub const LANE_RESTORE_FAILURE_LIMIT: u32 = 3;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneRecoveryState {
    Retryable,
    Suppressed,
    Resolved,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneRecoveryEpisode {
    pub episode_id: String,
    pub destination_thread_id: String,
    pub saved_root_run_id: String,
    pub stable_recovery_target: String,
    pub attempts: u32,
    pub last_attempt_at_ns: Option<i64>,
    pub state: LaneRecoveryState,
    pub last_failure_class: Option<String>,
    pub revision: u64,
    pub failure_report_ref: Option<String>,
}
/// Report CAS captured BEFORE an external restore, separate from episode identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneRecoveryReportTarget {
    pub lane_id: String,
    pub management_revision: u64,
    pub generation: u64,
    pub report_seq: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneRecoveryAttempt {
    /// Ownership/claim fencing only, never episode identity.
    pub destination_epoch: u64,
    pub owner: LaneDeliveryOwner,
    pub token: String,
    pub episode: LaneRecoveryEpisode,
    pub report_target: Option<LaneRecoveryReportTarget>,
}
#[derive(Debug, Clone)]
pub struct LaneRecoveryFailure {
    pub episode: LaneRecoveryEpisode,
    pub report: Option<crate::lanes::LaneReportAppended>,
}
