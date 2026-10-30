//! Bounded wait for a WAL-only admission refusal (ADR 0041 R5).
//!
//! When the only reason the index cannot admit a step is the WAL bound and the
//! preceding `wal_checkpoint(TRUNCATE)` did not complete (a reader pins
//! frames), the job neither pauses nor records gaps. It waits for the reader
//! to leave, probing with a non-blocking checkpoint at a backoff that doubles
//! from 100 ms up to 1 s. The instant the wait began is persisted in
//! `thread_index_state.wal_wait_since_ns`; each request expires at
//! `max(since, created_at_ns) + WAL_WAIT_LIMIT_NS`, so a restart or a new owner
//! never extends a deadline. This module holds the pure state, the persisted
//! `since` accessors and the backoff arithmetic; the step wiring uses them.

use rusqlite::{Connection, OptionalExtension};

use super::capacity::CheckpointOutcome;
use crate::StorageError;
use crate::thread_index::IndexCapacityStatus;

/// How long one request waits for the WAL to recover before it is closed with
/// `capacity_wait_expired` (5 minutes).
pub(crate) const WAL_WAIT_LIMIT_NS: i64 = 300_000_000_000;
/// First probe delay (100 ms).
pub(crate) const BACKOFF_MIN_NS: i64 = 100_000_000;
/// Upper bound of the probe delay (1 s).
pub(crate) const BACKOFF_MAX_NS: i64 = 1_000_000_000;
/// The probe delay is multiplied by this after every failed probe.
pub(crate) const BACKOFF_FACTOR: i64 = 2;

/// The delay after `current`, capped at [`BACKOFF_MAX_NS`].
pub(crate) fn next_backoff_ns(current: i64) -> i64 {
    current
        .saturating_mul(BACKOFF_FACTOR)
        .clamp(BACKOFF_MIN_NS, BACKOFF_MAX_NS)
}

/// The next probe instant: `now + backoff`, but never later than the job
/// deadline so an expiry is observed on time.
pub(crate) fn next_retry_at_ns(now_ns: i64, backoff_ns: i64, job_deadline_ns: Option<i64>) -> i64 {
    let at = now_ns.saturating_add(backoff_ns);
    job_deadline_ns.map_or(at, |deadline| at.min(deadline))
}

/// In-memory state of the current wait. Only `since_ns` is persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WalWait {
    /// When this wait began (mirrors `wal_wait_since_ns`).
    pub since_ns: i64,
    /// The WAL-only refusal that started the wait.
    pub refused: IndexCapacityStatus,
    /// When the next probe is due.
    pub next_retry_at_ns: i64,
    /// The delay behind `next_retry_at_ns`.
    pub backoff_ns: i64,
    /// Probes run so far (not counting the checkpoint that started the wait).
    pub probes: u32,
    /// The most recent checkpoint, which was not complete.
    pub last_checkpoint: Option<CheckpointOutcome>,
}

impl WalWait {
    /// A wait that began at `since_ns` and first probes after the minimum
    /// backoff.
    pub(crate) fn begin(
        since_ns: i64,
        refused: IndexCapacityStatus,
        now_ns: i64,
        job_deadline_ns: Option<i64>,
        last_checkpoint: Option<CheckpointOutcome>,
    ) -> Self {
        Self {
            since_ns,
            refused,
            next_retry_at_ns: next_retry_at_ns(now_ns, BACKOFF_MIN_NS, job_deadline_ns),
            backoff_ns: BACKOFF_MIN_NS,
            probes: 0,
            last_checkpoint,
        }
    }

    /// Whether a probe is due at `now_ns`.
    pub(crate) fn probe_due(&self, now_ns: i64) -> bool {
        now_ns >= self.next_retry_at_ns
    }

    /// Records an incomplete probe and schedules the next one.
    pub(crate) fn probed(
        &mut self,
        outcome: CheckpointOutcome,
        now_ns: i64,
        job_deadline_ns: Option<i64>,
    ) {
        self.probes = self.probes.saturating_add(1);
        self.last_checkpoint = Some(outcome);
        self.backoff_ns = next_backoff_ns(self.backoff_ns);
        self.next_retry_at_ns = next_retry_at_ns(now_ns, self.backoff_ns, job_deadline_ns);
    }

    /// A capacity notification (a reader left): probe at `now_ns` instead of
    /// waiting for the backoff. The backoff itself is kept.
    pub(crate) fn advance_to(&mut self, now_ns: i64) {
        self.next_retry_at_ns = self.next_retry_at_ns.min(now_ns);
    }
}

/// The persisted start of the current wait; `None` when not waiting.
pub(crate) fn get_since(conn: &Connection) -> Result<Option<i64>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT wal_wait_since_ns FROM thread_index_state WHERE id = 1",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten())
}

/// Starts the wait at `now_ns` unless one is already recorded, and returns the
/// effective start. An existing value is never overwritten.
pub(crate) fn set_since(conn: &Connection, now_ns: i64) -> Result<i64, StorageError> {
    conn.execute(
        "UPDATE thread_index_state SET wal_wait_since_ns = ?1 \
         WHERE id = 1 AND wal_wait_since_ns IS NULL",
        [now_ns],
    )?;
    Ok(get_since(conn)?.unwrap_or(now_ns))
}

/// Ends the wait; `true` when a start was recorded.
pub(crate) fn clear_since(conn: &Connection) -> Result<bool, StorageError> {
    Ok(conn.execute(
        "UPDATE thread_index_state SET wal_wait_since_ns = NULL \
         WHERE id = 1 AND wal_wait_since_ns IS NOT NULL",
        [],
    )? == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::apply_migrations;
    use crate::thread_index::{CapacityState, IndexPauseReason, PlannedGrowth, RequestGapReason};
    use crate::writer::index::requests;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply_migrations(&conn).unwrap();
        conn
    }

    fn insert_request(conn: &Connection, id: &str, state: &str, created_at_ns: i64) {
        let finished = (state != "pending").then_some(created_at_ns);
        let failure = (state == "failed").then_some("x");
        conn.execute(
            "INSERT INTO thread_index_requests(request_id, include_evicted, state, \
             failure_reason, created_at_ns, finished_at_ns) VALUES (?1, 0, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, state, failure, created_at_ns, finished],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO thread_index_request_targets(request_id, run_id, source_revision, \
             extractor_version, accounting_version, work_generation, state) \
             VALUES (?1, 'run-1', 1, 1, 1, 1, 'pending')",
            [id],
        )
        .unwrap();
    }

    fn target_state(conn: &Connection, id: &str) -> (String, Option<String>) {
        conn.query_row(
            "SELECT state, gap_reason FROM thread_index_request_targets WHERE request_id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    const LIMIT: i64 = WAL_WAIT_LIMIT_NS;

    #[test]
    fn gap_expired_uses_the_later_of_wait_start_and_submission_inclusively() {
        let conn = conn();
        // Submitted before the wait began: the deadline counts from `since`.
        insert_request(&conn, "before", "pending", 100);
        // Submitted during the wait: the deadline counts from its own creation.
        insert_request(&conn, "during", "pending", 1_000);
        let since = 500;
        let reason = RequestGapReason::CapacityWaitExpired;

        // One nanosecond before `since + limit` nothing expires.
        assert_eq!(
            requests::gap_expired(&conn, since, LIMIT, since + LIMIT - 1, reason).unwrap(),
            0
        );
        // The boundary is inclusive and only the earlier request expires.
        assert_eq!(
            requests::gap_expired(&conn, since, LIMIT, since + LIMIT, reason).unwrap(),
            1
        );
        assert_eq!(target_state(&conn, "before").0, "gap");
        assert_eq!(
            serde_json::from_str::<RequestGapReason>(&target_state(&conn, "before").1.unwrap())
                .unwrap(),
            reason
        );
        assert_eq!(target_state(&conn, "during").0, "pending");

        assert_eq!(
            requests::gap_expired(&conn, since, LIMIT, 1_000 + LIMIT, reason).unwrap(),
            1
        );
        assert_eq!(target_state(&conn, "during").0, "gap");
        // Nothing pending is left to expire.
        assert_eq!(
            requests::gap_expired(&conn, since, LIMIT, i64::MAX, reason).unwrap(),
            0
        );
    }

    #[test]
    fn cancelled_and_finished_requests_are_outside_the_wait() {
        let conn = conn();
        insert_request(&conn, "cancelled", "cancelled", 100);
        insert_request(&conn, "failed", "failed", 100);

        assert_eq!(
            requests::gap_expired(
                &conn,
                0,
                LIMIT,
                i64::MAX,
                RequestGapReason::CapacityWaitExpired
            )
            .unwrap(),
            0
        );
        assert_eq!(target_state(&conn, "cancelled").0, "pending");
        assert_eq!(
            requests::earliest_wait_deadline(&conn, 0, LIMIT).unwrap(),
            None
        );
        assert_eq!(requests::latest_pending_created_at(&conn).unwrap(), None);

        insert_request(&conn, "live", "pending", 700);
        assert_eq!(
            requests::earliest_wait_deadline(&conn, 0, LIMIT).unwrap(),
            Some(700 + LIMIT)
        );
        assert_eq!(
            requests::latest_pending_created_at(&conn).unwrap(),
            Some(700)
        );
    }

    #[test]
    fn earliest_deadline_and_latest_creation_follow_the_live_requests() {
        let conn = conn();
        assert_eq!(
            requests::earliest_wait_deadline(&conn, 0, LIMIT).unwrap(),
            None
        );
        insert_request(&conn, "a", "pending", 100);
        insert_request(&conn, "b", "pending", 900);
        // `a` waits from `since`, `b` from its own submission.
        assert_eq!(
            requests::earliest_wait_deadline(&conn, 500, LIMIT).unwrap(),
            Some(500 + LIMIT)
        );
        assert_eq!(
            requests::latest_pending_created_at(&conn).unwrap(),
            Some(900)
        );
        // A request without a pending target no longer waits.
        conn.execute(
            "UPDATE thread_index_request_targets SET state = 'gap', gap_reason = '{}' \
             WHERE request_id = 'a'",
            [],
        )
        .unwrap();
        assert_eq!(
            requests::earliest_wait_deadline(&conn, 500, LIMIT).unwrap(),
            Some(900 + LIMIT)
        );
    }

    #[test]
    fn wait_start_is_set_once_and_cleared_only_when_present() {
        let conn = conn();
        assert_eq!(get_since(&conn).unwrap(), None);
        assert!(!clear_since(&conn).unwrap());

        assert_eq!(set_since(&conn, 10).unwrap(), 10);
        // A later start (restart, another owner) does not extend the wait.
        assert_eq!(set_since(&conn, 99).unwrap(), 10);
        assert_eq!(get_since(&conn).unwrap(), Some(10));

        assert!(clear_since(&conn).unwrap());
        assert!(!clear_since(&conn).unwrap());
        assert_eq!(get_since(&conn).unwrap(), None);
        assert_eq!(set_since(&conn, 99).unwrap(), 99);
    }

    #[test]
    fn backoff_doubles_from_100ms_and_stops_at_1s() {
        let mut delays = vec![BACKOFF_MIN_NS];
        while *delays.last().unwrap() < BACKOFF_MAX_NS {
            delays.push(next_backoff_ns(*delays.last().unwrap()));
        }
        delays.push(next_backoff_ns(BACKOFF_MAX_NS));
        let millis: Vec<i64> = delays.iter().map(|ns| ns / 1_000_000).collect();
        assert_eq!(millis, [100, 200, 400, 800, 1000, 1000]);
        assert_eq!(next_backoff_ns(i64::MAX), BACKOFF_MAX_NS);
    }

    #[test]
    fn next_retry_is_capped_by_the_job_deadline() {
        assert_eq!(next_retry_at_ns(1_000, 200, None), 1_200);
        assert_eq!(next_retry_at_ns(1_000, 200, Some(5_000)), 1_200);
        assert_eq!(next_retry_at_ns(1_000, 200, Some(1_100)), 1_100);
        assert_eq!(next_retry_at_ns(i64::MAX, 200, None), i64::MAX);
    }

    fn refused() -> IndexCapacityStatus {
        IndexCapacityStatus {
            state: CapacityState::IndexPaused(IndexPauseReason::WalLimit),
            adjustment: None,
            db_bytes: 0,
            wal_bytes: 0,
            shm_bytes: 0,
            reserve_bytes: 0,
            resume_hysteresis_bytes: 0,
            operation: None,
            growth: PlannedGrowth::Bounded {
                db: 0,
                wal: 0,
                shm: 0,
            },
            max_db_bytes: 0,
            max_wal_bytes: 0,
        }
    }

    fn pinned() -> CheckpointOutcome {
        CheckpointOutcome {
            busy: true,
            log_frames: 10,
            checkpointed_frames: 4,
            locked: false,
        }
    }

    #[test]
    fn probes_back_off_and_a_notification_probes_immediately() {
        let mut wait = WalWait::begin(0, refused(), 0, None, Some(pinned()));
        assert_eq!(wait.next_retry_at_ns, BACKOFF_MIN_NS);
        assert!(!wait.probe_due(BACKOFF_MIN_NS - 1));
        assert!(wait.probe_due(BACKOFF_MIN_NS));

        let mut now = BACKOFF_MIN_NS;
        let mut delays = Vec::new();
        for _ in 0..6 {
            wait.probed(pinned(), now, None);
            delays.push((wait.next_retry_at_ns - now) / 1_000_000);
            now = wait.next_retry_at_ns;
        }
        assert_eq!(delays, [200, 400, 800, 1000, 1000, 1000]);
        assert_eq!(wait.probes, 6);

        wait.advance_to(now - 10);
        assert!(wait.probe_due(now - 10));
        assert_eq!(wait.backoff_ns, BACKOFF_MAX_NS);
    }

    #[test]
    fn the_job_deadline_caps_the_scheduled_probe() {
        let mut wait = WalWait::begin(0, refused(), 0, Some(50_000_000), None);
        assert_eq!(wait.next_retry_at_ns, 50_000_000);
        wait.probed(pinned(), 50_000_000, Some(120_000_000));
        assert_eq!(wait.next_retry_at_ns, 120_000_000);
    }

    #[test]
    fn checkpoint_outcome_is_complete_only_when_nothing_is_left() {
        let ok = CheckpointOutcome {
            busy: false,
            log_frames: 7,
            checkpointed_frames: 7,
            locked: false,
        };
        assert!(ok.complete());
        assert!(!CheckpointOutcome { busy: true, ..ok }.complete());
        assert!(
            !CheckpointOutcome {
                checkpointed_frames: 6,
                ..ok
            }
            .complete()
        );
        assert!(!CheckpointOutcome { locked: true, ..ok }.complete());
    }
}
