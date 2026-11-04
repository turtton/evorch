//! The lane ledger's writes and reads through the single writer.

use std::sync::mpsc;

use rusqlite::Connection;

use super::{Command, StorageHandle};
use crate::StorageError;
use crate::lanes::{
    LaneBrief, LaneCreation, LaneListPage, LaneListRequest, LaneManagementChange, LaneRecord,
    LaneReport, LaneReportAppended, LaneReportInput, LaneUpdate, NewLane,
};

/// A job for the writer thread: the connection and whether writes are suspended.
pub(super) type LaneJob = Box<dyn FnOnce(&Connection, bool) + Send>;

impl StorageHandle {
    fn lane_write<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, StorageError> + Send + 'static,
    ) -> Result<T, StorageError> {
        self.lane_job(true, work)
    }

    fn lane_read<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, StorageError> + Send + 'static,
    ) -> Result<T, StorageError> {
        self.lane_job(false, work)
    }

    fn lane_job<T: Send + 'static>(
        &self,
        writes: bool,
        work: impl FnOnce(&Connection) -> Result<T, StorageError> + Send + 'static,
    ) -> Result<T, StorageError> {
        let (reply, result) = mpsc::channel();
        let job: LaneJob = Box::new(move |conn, suspended| {
            let outcome = if writes && suspended {
                Err(StorageError::Serialization(
                    "lane writes suspended by storage limit".into(),
                ))
            } else {
                work(conn)
            };
            let _ = reply.send(outcome);
        });
        self.0
            .send(Command::Lane(job))
            .map_err(|_| StorageError::WriterClosed)?;
        result.recv().map_err(|_| StorageError::WriterClosed)?
    }

    /// Creates a lane, or returns the lane an earlier identical request created
    /// (`created == false`, no event due). Writes the lane's thread row (lineage kind
    /// `lane`) in the same transaction, so a crash before the root starts loses
    /// nothing: [`Self::unstarted_lanes`] finds the intent again.
    ///
    /// # Errors
    /// Oversized or empty input, a secret-bearing text, a reused creation key with a
    /// different brief, an ID owned by another lane, a tombstoned thread or run,
    /// suspended writes or a closed writer.
    pub fn create_lane(&self, new: NewLane) -> Result<LaneCreation, StorageError> {
        self.lane_write(move |conn| crate::repo::lanes::create(conn, &new))
    }

    /// Records a root start or restart: the generation moves from
    /// `expected_generation` (0 for the first start) to the next one and the lane's
    /// root run becomes `root_run_id`.
    ///
    /// # Errors
    /// `StaleRevision { kind: "generation" }` when another start won, `LaneNotFound`,
    /// a blocked creation intent (StartBlocked), a tombstoned run, suspended writes
    /// or a closed writer.
    pub fn start_lane_run(
        &self,
        lane_id: &str,
        expected_generation: u64,
        root_run_id: &str,
        at_ns: i64,
    ) -> Result<LaneRecord, StorageError> {
        let (lane_id, root_run_id) = (lane_id.to_owned(), root_run_id.to_owned());
        self.lane_write(move |conn| {
            crate::repo::lanes::start_run(conn, &lane_id, expected_generation, &root_run_id, at_ns)
        })
    }

    /// Permanently blocks a cancelled or locally failed creation intent. A started
    /// lane and an already blocked intent are unchanged; the first reason wins.
    ///
    /// # Errors
    /// Invalid or secret-bearing reason (maximum 1 KiB), missing lane, suspended
    /// writes or a closed writer. Success is the durable cancellation boundary.
    pub fn block_lane_start(
        &self,
        lane_id: &str,
        reason: &str,
        at_ns: i64,
    ) -> Result<LaneUpdate, StorageError> {
        let (lane_id, reason) = (lane_id.to_owned(), reason.to_owned());
        self.lane_write(move |conn| crate::repo::lanes::block_start(conn, &lane_id, &reason, at_ns))
    }

    /// Lets an intent that [`block_lane_start`](Self::block_lane_start) stopped for the
    /// operator ([`LANE_START_OPERATOR_STOP`](crate::lanes::LANE_START_OPERATOR_STOP))
    /// be recovered and started again. A cancelled or locally failed intent stays
    /// blocked (`StartBlocked`); a started or unblocked lane is unchanged.
    ///
    /// # Errors
    /// `StartBlocked`, a missing lane, suspended writes or a closed writer.
    pub fn resume_lane_start(&self, lane_id: &str, at_ns: i64) -> Result<LaneUpdate, StorageError> {
        let lane_id = lane_id.to_owned();
        self.lane_write(move |conn| crate::repo::lanes::resume_start(conn, &lane_id, at_ns))
    }

    /// Appends a report. The ledger numbers it (1, 2, 3, …) and never changes it
    /// afterwards; the producer cannot choose the sequence.
    ///
    /// # Errors
    /// An empty or oversized report (summary over 1 KiB, over 8 artifact references,
    /// over 4 KiB in all), a secret-bearing text, `LaneNotFound`, suspended writes or
    /// a closed writer.
    pub fn append_lane_report(
        &self,
        lane_id: &str,
        report: LaneReportInput,
        at_ns: i64,
    ) -> Result<LaneReportAppended, StorageError> {
        let lane_id = lane_id.to_owned();
        self.lane_write(move |conn| {
            crate::repo::lanes::append_report(conn, &lane_id, &report, at_ns)
        })
    }

    /// Moves the lead and policy of a lane from `expected_revision`, the management
    /// revision the caller read. The origin never changes. This is the data step of a
    /// policy change, a detach or a re-attach; whether a caller may take it is decided
    /// above the ledger.
    ///
    /// # Errors
    /// `StaleRevision { kind: "management" }` when the lane moved on, an inconsistent
    /// lead and policy, `LaneNotFound`, suspended writes or a closed writer.
    pub fn change_lane_management(
        &self,
        lane_id: &str,
        expected_revision: u64,
        change: LaneManagementChange,
        at_ns: i64,
    ) -> Result<LaneUpdate, StorageError> {
        let lane_id = lane_id.to_owned();
        self.lane_write(move |conn| {
            crate::repo::lanes::change_management(conn, &lane_id, expected_revision, &change, at_ns)
        })
    }

    /// Replaces the brief from `expected_revision`, the brief revision the caller read.
    ///
    /// # Errors
    /// `StaleRevision { kind: "brief" }`, an invalid brief or one whose policy differs
    /// from the lane's, `LaneNotFound`, suspended writes or a closed writer.
    pub fn revise_lane_brief(
        &self,
        lane_id: &str,
        expected_revision: u64,
        brief: LaneBrief,
        at_ns: i64,
    ) -> Result<LaneUpdate, StorageError> {
        let lane_id = lane_id.to_owned();
        self.lane_write(move |conn| {
            crate::repo::lanes::revise_brief(conn, &lane_id, expected_revision, &brief, at_ns)
        })
    }

    /// Records real activity (for example a turn of the lane) at `at_ns`; the stored
    /// time only moves forward. Reads and monitor ticks must not call this.
    ///
    /// # Errors
    /// `LaneNotFound`, suspended writes or a closed writer.
    pub fn touch_lane_activity(
        &self,
        lane_id: &str,
        at_ns: i64,
    ) -> Result<LaneRecord, StorageError> {
        let lane_id = lane_id.to_owned();
        self.lane_write(move |conn| crate::repo::lanes::touch_activity(conn, &lane_id, at_ns))
    }

    /// Registers the semantic key of a notification (lane, management revision, kind and
    /// the question / artifact revision / failure episode it is about). `true` when the
    /// key is new, `false` for a repeat. The vessel for the delivery contract of Phase 1c.
    ///
    /// # Errors
    /// `StaleRevision { kind: "management" }`, `LaneNotFound`, suspended writes or a
    /// closed writer.
    pub fn register_lane_delivery_key(
        &self,
        lane_id: &str,
        management_revision: u64,
        event_kind: &str,
        dedupe_key: &str,
        at_ns: i64,
    ) -> Result<bool, StorageError> {
        let (lane_id, event_kind, dedupe_key) = (
            lane_id.to_owned(),
            event_kind.to_owned(),
            dedupe_key.to_owned(),
        );
        self.lane_write(move |conn| {
            crate::repo::lanes::register_delivery_key(
                conn,
                &lane_id,
                management_revision,
                &event_kind,
                &dedupe_key,
                at_ns,
            )
        })
    }

    /// One page of the lanes a lead manages: newest real activity first, ties by lane
    /// ID, at most 20 entries and 16 KiB. A listing that needs more than one page is
    /// frozen on its first page, so the pages of one listing neither repeat nor skip a
    /// lane; a cursor whose listing was evicted is `LaneCursor(Stale)`.
    ///
    /// # Errors
    /// `LaneCursor(..)` for a stale, foreign or malformed cursor, an empty lead,
    /// suspended writes (a first page may store its listing) or a closed writer.
    pub fn lane_list(&self, request: LaneListRequest) -> Result<LaneListPage, StorageError> {
        self.lane_write(move |conn| crate::repo::lanes::list(conn, &request))
    }

    /// One lane, read through the writer so it is current with the caller's last write.
    ///
    /// # Errors
    /// Returns an error for SQLite failure or a closed writer.
    pub fn lane(&self, lane_id: &str) -> Result<Option<LaneRecord>, StorageError> {
        let lane_id = lane_id.to_owned();
        self.lane_read(move |conn| crate::repo::lanes::get(conn, &lane_id))
    }

    /// The lane whose root lives in `thread_id`, if the thread is a lane.
    ///
    /// # Errors
    /// Returns an error for SQLite failure or a closed writer.
    pub fn lane_by_thread(&self, thread_id: &str) -> Result<Option<LaneRecord>, StorageError> {
        let thread_id = thread_id.to_owned();
        self.lane_read(move |conn| crate::repo::lanes::get_by_thread(conn, &thread_id))
    }

    /// Reports of a lane after `after_seq`, oldest first, at most 50.
    ///
    /// # Errors
    /// Returns an error for SQLite failure or a closed writer.
    pub fn lane_reports(
        &self,
        lane_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<LaneReport>, StorageError> {
        let lane_id = lane_id.to_owned();
        self.lane_read(move |conn| crate::repo::lanes::reports(conn, &lane_id, after_seq, limit))
    }

    /// Recoverable creation intents a crash left, in lane ID order, at most
    /// `limit` (200 at most). Cancelled or locally failed intents are excluded.
    ///
    /// # Errors
    /// Returns an error for SQLite failure or a closed writer.
    pub fn unstarted_lanes(&self, limit: usize) -> Result<Vec<LaneRecord>, StorageError> {
        self.lane_read(move |conn| crate::repo::lanes::unstarted(conn, limit))
    }
}
