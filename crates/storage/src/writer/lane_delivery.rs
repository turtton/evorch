//! Public single-writer lane delivery API.
use super::StorageHandle;
use crate::{StorageError, lane_delivery::*};
impl StorageHandle {
    /// Commits question, responder routing and optional outbox. Capacity failure is a durable bounded diagnostic; no question is inserted.
    pub fn create_lane_question(
        &self,
        lane_id: String,
        question: event_bus::UserQuestion,
        requester: QuestionRequester,
        binding: LaneDeliveryBinding,
        at_ns: i64,
    ) -> Result<LaneQuestionCreated, StorageError> {
        self.lane_job(true, move |conn| {
            crate::repo::lane_delivery::create_question(
                conn, &lane_id, question, requester, binding, at_ns,
            )
        })
    }
    /// CAS answer/route mutation; publish the returned projection as UserQuestionUpdated after commit. Runtime authenticates lead and holds its ownership mutation guard.
    pub fn mutate_lane_question(
        &self,
        request: LaneQuestionMutation,
    ) -> Result<event_bus::UserQuestion, StorageError> {
        self.lane_job(true, move |conn| {
            crate::repo::lane_delivery::mutate_question(conn, request)
        })
    }
    /// Canonical responder routing, separate from continuation links. None means legacy routing.
    pub fn lane_question_routing(
        &self,
        question_id: String,
    ) -> Result<Option<QuestionRouting>, StorageError> {
        self.lane_job(false, move |conn| {
            crate::repo::lane_delivery::routing(conn, &question_id)
        })
    }
    /// Bounded current-lead inbox, not the lead run's 32-question inheritance set.
    pub fn lane_questions(
        &self,
        lead_thread_id: String,
        cursor: Option<LaneQuestionCursor>,
    ) -> Result<LaneQuestionPage, StorageError> {
        self.lane_job(false, move |conn| {
            crate::repo::lane_delivery::questions(conn, &lead_thread_id, cursor)
        })
    }
    /// Atomically appends a new report and its semantic notification. Never backfills historical reports.
    pub fn append_lane_report_with_delivery(
        &self,
        lane_id: String,
        expected_management_revision: u64,
        report: crate::lanes::LaneReportInput,
        at_ns: i64,
    ) -> Result<crate::lanes::LaneReportAppended, StorageError> {
        self.lane_job(true, move |conn| {
            crate::repo::lane_delivery::append_report(
                conn,
                &lane_id,
                expected_management_revision,
                report,
                at_ns,
            )
        })
    }
    /// Atomically records actual user input acceptance and a reference-only intervention notice.
    pub fn accept_lane_user_input(
        &self,
        lane_id: String,
        receipt: crate::HostInputReceipt,
    ) -> Result<(), StorageError> {
        self.lane_job(true, move |conn| {
            crate::repo::lane_delivery::accept_input(conn, &lane_id, receipt)
        })
    }
    /// Bounded durable inbox for either lead or lane destination; IDs remain stable across claims.
    pub fn lane_inbox(
        &self,
        destination: String,
        after: String,
    ) -> Result<Vec<LaneNotification>, StorageError> {
        self.lane_job(false, move |conn| {
            crate::repo::lane_delivery::inbox(conn, &destination, &after)
        })
    }
    /// Reads the durable epoch/Stop/owner without creating a binding. Use its epoch
    /// for rebind/claim after restart, including when the previous bind reply was lost.
    pub fn lane_delivery_destination(
        &self,
        thread_id: String,
    ) -> Result<Option<LaneDeliveryDestination>, StorageError> {
        self.lane_job(false, move |conn| {
            crate::repo::lane_delivery::destination(conn, &thread_id)
        })
    }
    /// CAS root/owner/Stop binding. The caller authenticates explicit resume; progress alone never clears Stop.
    pub fn bind_lane_delivery_destination(
        &self,
        thread_id: String,
        expected_epoch: u64,
        owner: LaneDeliveryOwner,
        action: LaneDestinationAction,
    ) -> Result<u64, StorageError> {
        self.lane_job(true, move |conn| {
            crate::repo::lane_delivery::bind_destination(
                conn,
                &thread_id,
                expected_epoch,
                owner,
                action,
            )
        })
    }
    /// Claims one bounded batch, single-flight per destination. Lease expiry uses the injected clock.
    /// After fencing an expired/no lease, repairs already-received notices before
    /// wake filtering (even for suppressed lanes). Empty may commit receipt-only ACKs;
    /// it never requires a snapshot rewrite or a runtime wake.
    pub fn claim_lane_deliveries(
        &self,
        request: LaneClaimRequest,
    ) -> Result<LaneClaimResult, StorageError> {
        self.lane_job(true, move |conn| {
            crate::repo::lane_delivery::claim(conn, request)
        })
    }
    /// Releases a current claim without losing pending notices. Stale epochs/tokens cannot release newer work.
    pub fn nack_lane_delivery(
        &self,
        claim: LaneDeliveryClaim,
        reason: LaneDeliveryNack,
        now_ns: i64,
    ) -> Result<(), StorageError> {
        self.lane_job(true, move |conn| {
            crate::repo::lane_delivery::nack(conn, &claim, reason, now_ns)
        })
    }
    /// Snapshot + non-user receipts + completed-root wake guards in one transaction.
    /// Fully received retries return None without rewriting history or counting again.
    pub fn accept_lane_delivery(
        &self,
        claim: LaneDeliveryClaim,
        write: crate::thread_index::SnapshotWrite,
        now_ns: i64,
    ) -> Result<Option<crate::thread_index::SnapshotReceipt>, StorageError> {
        self.lane_job(true, move |conn| {
            crate::repo::lane_delivery::accept_delivery(conn, &claim, write, now_ns)
        })
    }
    /// Requires durable receipts; retry repairs ACK without re-appending input.
    pub fn ack_lane_delivery(
        &self,
        claim: LaneDeliveryClaim,
        now_ns: i64,
    ) -> Result<(), StorageError> {
        self.lane_job(true, move |conn| {
            crate::repo::lane_delivery::ack(conn, &claim, now_ns)
        })
    }
    /// Receipt IDs already committed, including across compaction; exclude them before constructing a delivery snapshot.
    pub fn lane_delivery_received_ids(
        &self,
        claim: LaneDeliveryClaim,
    ) -> Result<Vec<String>, StorageError> {
        self.lane_job(false, move |conn| {
            crate::repo::lane_delivery::receipt_ids(conn, &claim)
        })
    }
    /// Bounded recovery scan for startup, ownership acquisition or user-input redrive.
    pub fn pending_lane_delivery_destinations(
        &self,
        after: String,
    ) -> Result<Vec<String>, StorageError> {
        self.lane_job(false, move |conn| {
            crate::repo::lane_delivery::pending_destinations(conn, &after)
        })
    }
    /// Durable per-source guard; root generation and ownership changes do not reset it.
    pub fn lane_wake_guard(&self, lane_id: String) -> Result<LaneWakeGuard, StorageError> {
        self.lane_job(false, move |conn| {
            crate::repo::lane_delivery::guard(conn, &lane_id)
        })
    }
    /// CAS guard update. Only actual user input/resume or a known changed immutable artifact resets it.
    pub fn update_lane_wake_guard(
        &self,
        lane_id: String,
        expected_revision: u64,
        signal: LaneGuardSignal,
    ) -> Result<LaneWakeGuard, StorageError> {
        self.lane_job(true, move |conn| {
            crate::repo::lane_delivery::update_guard(conn, &lane_id, expected_revision, signal)
        })
    }
    /// The lane's single bounded durable question_capacity_blocked diagnostic.
    pub fn lane_question_capacity_diagnostic(
        &self,
        lane_id: String,
    ) -> Result<Option<QuestionCapacityBlocked>, StorageError> {
        self.lane_job(false, move |conn| {
            crate::repo::lane_delivery::capacity_diagnostic(conn, &lane_id)
        })
    }
}
