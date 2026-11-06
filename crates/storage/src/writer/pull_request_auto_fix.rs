//! Single-writer PR policy/preparation and typed outbox operations.
use super::StorageHandle;
use crate::repo::{lane_delivery as delivery, pull_request_auto_fix as pr};
use crate::{
    StorageError,
    lane_delivery::*,
    pull_request_auto_fix::*,
    pull_requests::PullRequestKey,
    thread_index::{SnapshotReceipt, SnapshotWrite},
};
impl StorageHandle {
    /// PR-wide policy; absent state is opt-out.
    pub fn pr_auto_fix_state(&self, key: PullRequestKey) -> Result<PrAutoFixState, StorageError> {
        self.lane_job(false, move |c| pr::state(c, &key))
    }
    /// CAS policy/destination; reset preserves SHA history, cooldown, Stop and guard.
    pub fn update_pr_auto_fix_policy(
        &self,
        request: PrPolicyUpdate,
    ) -> Result<PrAutoFixState, StorageError> {
        self.lane_job(true, move |c| pr::update_policy(c, request))
    }
    /// Reserve a PR/SHA budget slot; cancelled retries reuse the stable notice ID.
    pub fn reserve_pr_notice(
        &self,
        request: PrNoticeReservation,
    ) -> Result<PrNotice, StorageError> {
        self.lane_job(true, move |c| pr::reserve(c, request))
    }
    /// Ready and outbox insertion commit atomically; payload is bounded and secret-checked.
    pub fn ready_pr_notice(&self, id: String, payload: String) -> Result<PrNotice, StorageError> {
        self.lane_job(true, move |c| pr::ready(c, &id, &payload))
    }
    /// Cancel before adoption, releasing the reservation.
    pub fn cancel_pr_notice(&self, id: String) -> Result<(), StorageError> {
        self.lane_job(true, move |c| pr::cancel(c, &id))
    }
    /// Ready + outbox insertion only if the complete expected reservation still matches.
    /// Comparison and writes share one immediate transaction; a stale/missing record is a no-op.
    pub fn ready_pr_notice_if_current(
        &self,
        expected: PrNotice,
        payload: String,
    ) -> Result<Option<PrNotice>, StorageError> {
        self.lane_job(true, move |c| pr::ready_if_current(c, &expected, &payload))
    }
    /// Cancel only the observed reservation/ready record, never a replacement with the same ID.
    /// Returns false for a stale/missing record, without changing the outbox or budget.
    pub fn cancel_pr_notice_if_current(&self, expected: PrNotice) -> Result<bool, StorageError> {
        self.lane_job(true, move |c| pr::cancel_if_current(c, &expected))
    }
    /// Read preparation/payload separately from the outbox reference.
    pub fn pr_notice(&self, id: String) -> Result<Option<PrNotice>, StorageError> {
        self.lane_job(false, move |c| pr::notice(c, &id))
    }
    /// PR-only claim; Lane claims remain Lane-only. Both share destination single-flight.
    pub fn claim_pr_delivery(
        &self,
        request: LaneClaimRequest,
    ) -> Result<PrClaimResult, StorageError> {
        self.lane_job(true, move |c| delivery::claim(c, request))
    }
    /// Atomically adopt an append-only candidate snapshot, receipt, budget, cooldown, auto-off event and wake guard. Runtime publishes only after success and passes the current trusted switch.
    pub fn accept_pr_delivery(
        &self,
        claim: PrDeliveryClaim,
        write: SnapshotWrite,
        now_ns: i64,
        auto_fix_enabled: bool,
    ) -> Result<Option<SnapshotReceipt>, StorageError> {
        self.lane_job(true, move |c| {
            delivery::accept_delivery(c, &claim, write, now_ns, auto_fix_enabled)
        })
    }
    /// Idempotent ACK after receipt; claim also repairs receipt-only crash recovery.
    pub fn ack_pr_delivery(&self, claim: PrDeliveryClaim, now_ns: i64) -> Result<(), StorageError> {
        self.lane_job(true, move |c| delivery::ack(c, &claim, now_ns))
    }
    /// Typed NACK releases the shared destination lease.
    pub fn nack_pr_delivery(
        &self,
        claim: PrDeliveryClaim,
        reason: LaneDeliveryNack,
        now_ns: i64,
    ) -> Result<(), StorageError> {
        self.lane_job(true, move |c| delivery::nack(c, &claim, reason, now_ns))
    }
    /// Stable IDs already adopted; never append these again.
    pub fn pr_delivery_receipt_ids(
        &self,
        claim: PrDeliveryClaim,
    ) -> Result<Vec<String>, StorageError> {
        self.lane_job(false, move |c| delivery::receipt_ids(c, &claim))
    }
    /// PR-stable source guard, independent from destination Stop.
    pub fn pr_wake_guard(&self, key: PullRequestKey) -> Result<LaneWakeGuard, StorageError> {
        self.lane_job(false, move |c| pr::guard(c, &key))
    }
    /// Only real user input or explicit Resume resets the guard.
    pub fn update_pr_wake_guard(
        &self,
        key: PullRequestKey,
        expected: u64,
        signal: PrGuardSignal,
    ) -> Result<LaneWakeGuard, StorageError> {
        self.lane_job(true, move |c| pr::update_guard(c, &key, expected, signal))
    }
    /// Bounded recovery listing for preparation/adopted records (stable ID cursor).
    pub fn pr_notices(
        &self,
        key: PullRequestKey,
        after: String,
    ) -> Result<Vec<PrNotice>, StorageError> {
        self.lane_job(false, move |c| pr::list_notices(c, &key, &after))
    }
    /// PR-only startup/ownership recovery scan. Shared destination lease still fences Lane.
    pub fn pending_pr_delivery_destinations(
        &self,
        after: String,
    ) -> Result<Vec<String>, StorageError> {
        self.lane_job(false, move |c| pr::pending_destinations(c, &after))
    }
    /// Bounded durable exhaustion event history, epoch cursor exclusive (None starts at zero).
    pub fn pr_auto_fix_limit_events(
        &self,
        key: PullRequestKey,
        after: Option<u64>,
    ) -> Result<Vec<PrAutoFixLimitEvent>, StorageError> {
        self.lane_job(false, move |c| pr::limit_events(c, &key, after))
    }
}
