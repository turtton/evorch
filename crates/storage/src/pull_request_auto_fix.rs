//! PR-wide policy and preparation. Delivery uses the lane outbox's single-flight
//! engine; no producer or runtime input path is installed here.
//!
//! Callers authenticate user operations, supply Unix nanoseconds, and hold the
//! destination ownership guard during adoption. Eligibility (checks/publishing)
//! belongs to the coordinator. Storage fences link, policy, budget and lease.
use crate::{lane_delivery::*, pull_requests::PullRequestKey};
use serde::{Deserialize, Serialize};
pub const PR_AUTO_FIX_LIMIT: u32 = 3;
pub const PR_AUTO_FIX_COOLDOWN_NS: i64 = 600_000_000_000;
pub const PR_NOTICE_PAYLOAD_MAX: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrAutoFixState {
    pub key: PullRequestKey,
    pub enabled: bool,
    pub destination_thread_id: Option<String>,
    pub link_revision: Option<u64>,
    pub policy_revision: u64,
    pub budget_epoch: u64,
    pub used: u32,
    pub reserved: u32,
    pub cooldown_until_ns: Option<i64>,
    pub stop_reason: Option<String>,
    pub limit_reached_epoch: Option<u64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrPolicyAction {
    SetEnabled(bool),
    ResetBudget,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrPolicyUpdate {
    pub key: PullRequestKey,
    pub expected_policy_revision: u64,
    /// Exact previous destination observed/confirmed by the user (including None).
    pub expected_destination_thread_id: Option<String>,
    pub destination_thread_id: String,
    pub link_revision: u64,
    pub action: PrPolicyAction,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrNoticeMode {
    Manual,
    Auto,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrNoticeReservation {
    pub key: PullRequestKey,
    pub head_sha: String,
    pub mode: PrNoticeMode,
    pub destination_thread_id: String,
    pub link_revision: u64,
    pub expected_policy_revision: u64,
    pub success_seq: u64,
    /// Effective trusted global switch; runtime must pass its current value again
    /// at adoption. Manual notices are independent of this switch.
    pub auto_fix_enabled: bool,
    pub now_ns: i64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrNoticePhase {
    Reserved,
    Ready,
    Adopted,
    Cancelled,
}
/// PR-side preparation record. Payload is already bounded and secret-checked;
/// outbox envelopes contain only the stable reference below.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrNotice {
    pub notice_id: String,
    /// Increments whenever a cancelled stable SHA ID is reserved again.
    pub reservation_revision: u64,
    pub key: PullRequestKey,
    pub head_sha: String,
    pub mode: PrNoticeMode,
    pub destination_thread_id: Option<String>,
    pub link_revision: u64,
    pub policy_revision: u64,
    pub budget_epoch: u64,
    pub success_seq: u64,
    pub phase: PrNoticePhase,
    pub payload: Option<String>,
}
/// Wire-neutral outbox reference, never a CI log or human instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrNoticeRef {
    pub notification_id: String,
    pub destination_thread_id: String,
}
pub type PrDeliveryClaim = LaneDeliveryClaim<PrNoticeRef>;
pub type PrClaimResult = LaneClaimResult<PrNoticeRef>;
/// PR has no terminal-artifact signal. Head/check/ACK/poll changes cannot reset it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrGuardSignal {
    WakeBatch { batch_id: String },
    RealUserInput,
    ExplicitUserResume,
}

/// Typed refusal for coordinator/UI decisions (never contains payload text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrAutoFixRejection {
    ArchivedDestination,
    NotLinked,
    StaleLink,
    DestinationConfirmationRequired,
    BudgetResetRequired,
    AutoFixDisabled,
    WrongDestination,
    BudgetExhausted,
    Cooldown,
    UnknownNotice,
    InvalidHeadSha,
    AlreadyAdopted,
    ReservationConflict,
    BudgetReserved,
    PayloadTooLarge,
    NotReserved,
    NoDestination,
    DestinationMismatch,
    NotReady,
    WakeSuppressed,
    StaleBudget,
}
impl std::fmt::Display for PrAutoFixRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ArchivedDestination => "pr_notice_archived_destination",
            Self::NotLinked => "pr_notice_not_linked",
            Self::StaleLink => "pr_notice_stale_link",
            Self::DestinationConfirmationRequired => "pr_notice_destination_confirmation_required",
            Self::BudgetResetRequired => "pr_notice_budget_reset_required",
            Self::AutoFixDisabled => "pr_notice_auto_fix_disabled",
            Self::WrongDestination => "pr_notice_wrong_destination",
            Self::BudgetExhausted => "pr_notice_budget_exhausted",
            Self::Cooldown => "pr_notice_cooldown",
            Self::UnknownNotice => "pr_notice_unknown_notice",
            Self::InvalidHeadSha => "pr_notice_invalid_head_sha",
            Self::AlreadyAdopted => "pr_notice_already_adopted",
            Self::ReservationConflict => "pr_notice_reservation_conflict",
            Self::BudgetReserved => "pr_notice_budget_reserved",
            Self::PayloadTooLarge => "pr_notice_payload_too_large",
            Self::NotReserved => "pr_notice_not_reserved",
            Self::NoDestination => "pr_notice_no_destination",
            Self::DestinationMismatch => "pr_notice_destination_mismatch",
            Self::NotReady => "pr_notice_not_ready",
            Self::WakeSuppressed => "pr_notice_wake_suppressed",
            Self::StaleBudget => "pr_notice_stale_budget",
        })
    }
}

/// Durable exhaustion fact, recorded once in the third adoption transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrAutoFixLimitEvent {
    pub key: PullRequestKey,
    pub budget_epoch: u64,
    pub notice_id: String,
    pub at_ns: i64,
}
