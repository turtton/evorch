//! ADR 0041 順序 5 acceptance: settings, budget eviction order, oversize,
//! retry backoff, physical admission / reclaim, leases and requests, and the
//! background schedule. Time is passed explicitly or observed through
//! channels; nothing sleeps.

use std::sync::mpsc;
use std::time::Duration;

use rusqlite::Connection;
use secret_guard::SecretRedactor;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::capacity::{self, MEASURED_POLICY, Measurement};
use super::work::{self, Env, IndexRuntime};
use super::{Control, ThreadIndexCounters, should_step};
use crate::db::{FileSizes, file_sizes};
use crate::repo::thread_index::{SqliteSnapshotStore, SqliteThreadIndex};
use crate::thread_index::*;
use crate::writer::Command;
use crate::{Database, HardLimits, RunContextRecord, Storage, StorageConfig};

const S: i64 = 1_000_000_000;
const HOUR: i64 = 3_600 * S;
const MIB: u64 = 1 << 20;

fn gui() -> IndexOwner {
    IndexOwner {
        id: "gui-1".into(),
        kind: IndexOwnerKind::Gui,
    }
}

fn cli() -> IndexOwner {
    IndexOwner {
        id: "cli-1".into(),
        kind: IndexOwnerKind::Cli,
    }
}

fn enabled(max_bytes: u64) -> ThreadIndexSettings {
    ThreadIndexSettings {
        enabled: true,
        max_bytes,
    }
}

fn messages(body: &str) -> Value {
    json!([{"role": "user", "content": [{"type": "text", "text": body}]}])
}

fn write(run_id: &str, body: &str, at: i64, status: ProvenanceStatus) -> SnapshotWrite {
    let messages = messages(body);
    SnapshotWrite {
        record: RunContextRecord {
            run_id: run_id.into(),
            role: "worker".into(),
            name: "worker".into(),
            parent_run_id: None,
            config_json: "{}".into(),
            messages_json: messages.to_string(),
            checkpoints_json: "[]".into(),
            terminal_phase: "Done".into(),
            restorable: true,
            updated_at_ns: at,
        },
        expected_snapshot_revision: None,
        binding: None,
        status,
        map: ProvenanceMap {
            version: PROVENANCE_FORMAT_VERSION,
            raw_message_count: 1,
            checkpoint_count: 0,
            messages: vec![MessageProvenance {
                raw_index: 0,
                ordinal: 0,
                blocks: vec![BlockProvenance {
                    block_index: 0,
                    origin: BlockOrigin::Native,
                    excluded_reason: None,
                }],
            }],
        },
    }
}

fn verified(run_id: &str, body: &str, at: i64) -> SnapshotWrite {
    write(
        run_id,
        body,
        at,
        ProvenanceStatus::Verified {
            evidence: ProvenanceEvidence::CompletePositionMap,
        },
    )
}

#[derive(Debug)]
struct Row {
    state: String,
    evicted_reason: Option<String>,
    last_error_code: Option<String>,
    attempts: u32,
    next_attempt_at_ns: Option<i64>,
    charged: u64,
    indexed_at_ns: Option<i64>,
    blocks: u64,
}

fn row(conn: &Connection, run_id: &str) -> Row {
    conn.query_row(
        "SELECT state, evicted_reason, last_error_code, attempt_count, next_attempt_at_ns, \
         charged_bytes, indexed_at_ns, \
         (SELECT COUNT(*) FROM conversation_blocks b WHERE b.run_id = r.run_id) \
         FROM thread_index_runs r WHERE run_id = ?1",
        [run_id],
        |row| {
            Ok(Row {
                state: row.get(0)?,
                evicted_reason: row.get(1)?,
                last_error_code: row.get(2)?,
                attempts: row.get(3)?,
                next_attempt_at_ns: row.get(4)?,
                charged: row.get(5)?,
                indexed_at_ns: row.get(6)?,
                blocks: row.get(7)?,
            })
        },
    )
    .unwrap()
}

struct Fixture {
    _dir: Option<TempDir>,
    db: Database,
    config: StorageConfig,
    rt: IndexRuntime,
    redactor: SecretRedactor,
    source_suspended: bool,
}

impl Fixture {
    fn new(max_bytes: u64) -> Self {
        Self::at(TempDir::new().unwrap(), max_bytes)
    }

    fn at(dir: TempDir, max_bytes: u64) -> Self {
        let config = StorageConfig {
            db_path: dir.path().join("index.db"),
            thread_index: enabled(max_bytes),
            ..StorageConfig::default()
        };
        let db = Database::open(&config).unwrap();
        let mut rt = IndexRuntime::new(config.thread_index);
        work::configure_fts(&db.conn).unwrap();
        let env = Env {
            conn: &db.conn,
            db_path: &config.db_path,
            limits: &config.hard_limits,
            source_suspended: false,
        };
        work::apply_settings(env, &mut rt, config.thread_index).unwrap();
        Self {
            _dir: Some(dir),
            db,
            config,
            rt,
            redactor: SecretRedactor::from_env(),
            source_suspended: false,
        }
    }

    /// Another process on the same database (own connection and runtime).
    fn peer(&self) -> Self {
        let db = Database::open(&self.config).unwrap();
        let mut rt = IndexRuntime::new(self.config.thread_index);
        let env = Env {
            conn: &db.conn,
            db_path: &self.config.db_path,
            limits: &self.config.hard_limits,
            source_suspended: false,
        };
        work::apply_settings(env, &mut rt, self.config.thread_index).unwrap();
        Self {
            _dir: None,
            db,
            config: self.config.clone(),
            rt,
            redactor: SecretRedactor::from_env(),
            source_suspended: false,
        }
    }

    fn conn(&self) -> &Connection {
        &self.db.conn
    }

    fn control(&mut self, now_ns: i64) -> Control<'_> {
        Control {
            env: Env {
                conn: &self.db.conn,
                db_path: &self.config.db_path,
                limits: &self.config.hard_limits,
                source_suspended: self.source_suspended,
            },
            rt: &mut self.rt,
            redactor: &self.redactor,
            now_ns,
        }
    }

    fn save(&self, write: &SnapshotWrite) -> SnapshotReceipt {
        SqliteSnapshotStore::new(self.conn())
            .save_snapshot_bundle(write)
            .unwrap()
    }

    fn lease(&mut self, owner: &IndexOwner, now_ns: i64) -> IndexLease {
        match self
            .control(now_ns)
            .acquire_lease(owner, now_ns, HOUR)
            .unwrap()
        {
            LeaseResult::Acquired(lease) => lease,
            other => panic!("lease not acquired: {other:?}"),
        }
    }

    /// Steps until the job reports anything but progress.
    fn drive(&mut self, lease: &IndexLease, now_ns: i64) -> IndexStepResult {
        for _ in 0..10_000 {
            let result = self.control(now_ns).step(lease, now_ns).unwrap();
            if result != IndexStepResult::Progress {
                return result;
            }
        }
        panic!("index did not settle");
    }

    fn charge(&self, run_id: &str) -> u64 {
        let bundle = SqliteSnapshotStore::new(self.conn())
            .load_snapshot_bundle(run_id)
            .unwrap()
            .unwrap();
        let input = extract_snapshot_bundle(&bundle, &self.redactor).unwrap();
        account_index_input(&input, DEFAULT_ACCOUNTING_POLICY)
            .unwrap()
            .charged_bytes
    }

    fn row(&self, run_id: &str) -> Row {
        row(self.conn(), run_id)
    }

    fn sizes(&self) -> FileSizes {
        file_sizes(&self.config.db_path).unwrap()
    }
}

fn idle() -> IndexStepResult {
    IndexStepResult::Idle {
        next_wake_at_ns: None,
    }
}

#[test]
fn step_scheduling_prefers_idle_but_never_starves() {
    assert!(should_step(true, 0, Duration::ZERO));
    assert!(!should_step(false, 31, Duration::from_millis(99)));
    assert!(should_step(false, 32, Duration::ZERO));
    assert!(should_step(false, 0, Duration::from_millis(100)));
}

#[test]
fn startup_applies_accounting_version_and_indexes_newest_first() {
    let mut fx = Fixture::new(512 * MIB);
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, S), idle());
    let versions: (u32, u32) = fx
        .conn()
        .query_row(
            "SELECT extractor_version, accounting_version FROM thread_index_state",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(versions, (EXTRACTOR_VERSION, ACCOUNTING_VERSION));
    fx.save(&verified("run-old", "older conversation", 10 * S));
    fx.save(&verified("run-new", "newer conversation", 20 * S));
    let lease = fx.lease(&gui(), 100 * S);
    let mut now = 100 * S;
    loop {
        now += 1;
        match fx.control(now).step(&lease, now).unwrap() {
            IndexStepResult::Progress => {}
            result => {
                assert_eq!(result, idle());
                break;
            }
        }
    }
    let (old, new) = (fx.row("run-old"), fx.row("run-new"));
    assert_eq!(
        (old.state.as_str(), new.state.as_str()),
        ("indexed", "indexed")
    );
    assert!(
        new.indexed_at_ns < old.indexed_at_ns,
        "newer run must be indexed first"
    );
    assert_eq!(fx.rt.counters.commits, 2);
}

#[test]
fn logical_budget_evicts_only_older_runs_one_per_step() {
    let mut fx = Fixture::new(512 * MIB);
    for (run, at) in [("run-a", 30), ("run-b", 20), ("run-c", 10)] {
        fx.save(&verified(
            run,
            &format!("{run} body {}", "x".repeat(2_000)),
            at * S,
        ));
    }
    let charge = fx.charge("run-a");
    // Room for exactly two runs.
    let budget = enabled(2 * charge + charge / 2);
    fx.control(S).apply_index_settings(budget).unwrap();
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    assert_eq!(fx.row("run-a").state, "indexed");
    assert_eq!(fx.row("run-b").state, "indexed");
    // Nothing older than run-c holds bytes: it evicts itself, never a newer run.
    let c = fx.row("run-c");
    assert_eq!(
        (c.state.as_str(), c.evicted_reason.as_deref()),
        ("evicted", Some("logical_budget"))
    );
    fx.save(&verified(
        "run-d",
        &format!("run-d body {}", "x".repeat(2_000)),
        40 * S,
    ));
    let before = fx.rt.counters.evictions;
    assert_eq!(fx.drive(&lease, 3 * S), idle());
    assert_eq!(
        fx.rt.counters.evictions,
        before + 1,
        "one older run per step"
    );
    assert_eq!(fx.row("run-b").state, "evicted");
    assert_eq!(fx.row("run-a").state, "indexed");
    assert_eq!(fx.row("run-d").state, "indexed");
    let total: u64 = fx
        .conn()
        .query_row(
            "SELECT SUM(charged_bytes) FROM thread_index_runs",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(total <= budget.max_bytes);
}

#[test]
fn raising_the_limit_keeps_evicted_runs_until_include_evicted() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-new", &"n".repeat(3_000), 20 * S));
    fx.save(&verified("run-old", &"o".repeat(3_000), 10 * S));
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    let charge = fx.row("run-new").charged;
    fx.control(3 * S)
        .apply_index_settings(enabled(charge + 10))
        .unwrap();
    assert_eq!(fx.drive(&lease, 3 * S), idle());
    assert_eq!(fx.row("run-old").state, "evicted");
    let receipt = fx
        .control(4 * S)
        .apply_index_settings(enabled(512 * MIB))
        .unwrap();
    assert_eq!(receipt.settings.max_bytes, 512 * MIB);
    assert_eq!(fx.drive(&lease, 5 * S), idle());
    assert_eq!(
        fx.row("run-old").state,
        "evicted",
        "a raised limit revives nothing"
    );
    let request = ReindexRequest {
        canonical_project: None,
        since_ns: None,
        include_evicted: true,
    };
    let submitted = fx
        .control(6 * S)
        .submit_request(&request, &gui(), 6 * S, HOUR)
        .unwrap();
    assert_eq!(fx.drive(&lease, 7 * S), idle());
    assert_eq!(fx.row("run-old").state, "indexed");
    let status = fx
        .control(8 * S)
        .request_status(&submitted.request_id, 8 * S)
        .unwrap();
    assert!(matches!(
        status.progress,
        RequestProgress::Terminal(ReindexResult::Completed { .. })
    ));
}

#[test]
fn settings_shrink_is_deferred_and_evicts_one_oldest_run_per_step() {
    let mut fx = Fixture::new(512 * MIB);
    for run in ["run-c", "run-b", "run-a"] {
        fx.save(&verified(run, &"old index ".repeat(300), 10 * S));
    }
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    let before = fx.rt.counters.evictions;
    let receipt = fx.control(3 * S).apply_index_settings(enabled(0)).unwrap();
    assert_eq!(receipt.settings.max_bytes, 0);
    assert_eq!(
        fx.rt.counters.evictions, before,
        "receipt is not bulk eviction"
    );
    fx.save(&verified("run-new", "no growth during adjustment", 20 * S));
    for (offset, run) in ["run-a", "run-b", "run-c"].into_iter().enumerate() {
        assert_eq!(
            fx.control(4 * S).step(&lease, 4 * S).unwrap(),
            IndexStepResult::Progress
        );
        assert_eq!(fx.rt.counters.evictions, before + offset as u64 + 1);
        assert_eq!(
            fx.row(run).state,
            "evicted",
            "creation time ties use run_id"
        );
        assert_eq!(fx.row("run-new").blocks, 0);
    }
}

#[test]
fn settings_shrink_preserves_old_blocks_when_db_or_pinned_wal_cannot_admit_delete() {
    for wal_limited in [false, true] {
        let mut fx = Fixture::new(512 * MIB);
        fx.save(&verified("run-old", &"retained ".repeat(400), 10 * S));
        let lease = fx.lease(&gui(), S);
        assert_eq!(fx.drive(&lease, 2 * S), idle());
        let reader = Connection::open(&fx.config.db_path).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM conversation_blocks;")
            .unwrap();
        let sizes = fx.sizes();
        if wal_limited {
            fx.config.hard_limits.max_wal_bytes = sizes.wal + 1;
        } else {
            fx.config.hard_limits.max_db_bytes = sizes.total() + MEASURED_POLICY.reserve_bytes();
        }
        fx.control(3 * S).apply_index_settings(enabled(0)).unwrap();
        assert_eq!(
            fx.row("run-old").state,
            "indexed",
            "no unadmitted settings delete"
        );
        assert!(matches!(
            fx.drive(&lease, 4 * S),
            IndexStepResult::Paused(_)
        ));
        assert_eq!(fx.row("run-old").blocks, 1);
        assert_eq!(fx.rt.counters.evictions, 0);
        assert!(
            !capacity::checkpoint_truncate(fx.conn()).unwrap(),
            "reader pins WAL"
        );
        reader.execute_batch("ROLLBACK").unwrap();
        fx.config.hard_limits = HardLimits::default();
        fx.control(5 * S).apply_index_settings(enabled(0)).unwrap();
        assert_eq!(fx.drive(&lease, 6 * S), idle());
        assert_eq!(fx.row("run-old").state, "evicted");
    }
}

#[test]
fn self_eviction_of_previous_revision_requires_delete_admission() {
    for oversized_step in [false, true] {
        let mut fx = Fixture::new(512 * MIB);
        fx.save(&verified("run-old", &"old ".repeat(800), 10 * S));
        let lease = fx.lease(&gui(), S);
        assert_eq!(fx.drive(&lease, 2 * S), idle());
        let old_charge = fx.row("run-old").charged;
        fx.control(3 * S)
            .apply_index_settings(enabled(old_charge + 1))
            .unwrap();
        let body = "new ".repeat(if oversized_step { 140_000 } else { 2_000 });
        fx.save(&verified("run-old", &body, 20 * S));
        let mut work = work::next_work(fx.conn()).unwrap().unwrap();
        work.lease_epoch = lease.epoch;
        let input = work::extract(fx.conn(), &work, &fx.redactor).unwrap();
        fx.rt.ready = Some((work, Ok(input)));
        let reader = Connection::open(&fx.config.db_path).unwrap();
        reader
            .execute_batch("BEGIN; SELECT COUNT(*) FROM conversation_blocks;")
            .unwrap();
        fx.config.hard_limits.max_wal_bytes = fx.sizes().wal + 1;
        assert!(matches!(
            fx.drive(&lease, 4 * S),
            IndexStepResult::Paused(_)
        ));
        let retained = fx.row("run-old");
        assert_eq!(
            (retained.state.as_str(), retained.blocks, retained.charged),
            ("pending", 1, old_charge)
        );
        assert_eq!(fx.rt.counters.evictions, 0);
        reader.execute_batch("ROLLBACK").unwrap();
        fx.config.hard_limits = HardLimits::default();
        fx.control(5 * S)
            .apply_index_settings(enabled(old_charge + 1))
            .unwrap();
        assert_eq!(fx.drive(&lease, 6 * S), idle());
        assert_eq!(fx.row("run-old").state, "evicted");
    }
}

#[test]
fn reaccounting_is_deferred_and_cannot_delete_through_pinned_wal_limit() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-old", &"old ".repeat(800), 10 * S));
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    let old_charge = fx.row("run-old").charged;
    let reader = Connection::open(&fx.config.db_path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT COUNT(*) FROM conversation_blocks;")
        .unwrap();
    fx.config.hard_limits.max_wal_bytes = fx.sizes().wal + 1;
    fx.rt.accounting.version += 1;
    fx.rt.accounting.per_run_bytes += old_charge;
    fx.control(3 * S)
        .apply_index_settings(enabled(old_charge + 1))
        .unwrap();
    assert_eq!(
        fx.row("run-old").charged,
        old_charge,
        "receipt must not reaccount eagerly"
    );
    assert!(matches!(
        fx.drive(&lease, 4 * S),
        IndexStepResult::Paused(_)
    ));
    assert_eq!(fx.row("run-old").blocks, 1);
    reader.execute_batch("ROLLBACK").unwrap();
    fx.config.hard_limits = HardLimits::default();
    fx.control(5 * S)
        .apply_index_settings(enabled(old_charge + 1))
        .unwrap();
    assert_eq!(fx.drive(&lease, 6 * S), idle());
    assert_eq!(fx.row("run-old").state, "evicted");
}

#[test]
fn settings_adjustment_resumes_from_persisted_rows_after_restart() {
    let mut fx = Fixture::new(512 * MIB);
    for (run, at) in [("run-c", 30), ("run-b", 20), ("run-a", 10)] {
        fx.save(&verified(run, &"persistent index ".repeat(200), at * S));
    }
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    fx.config.thread_index = enabled(0);
    let settings = fx.config.thread_index;
    fx.control(3 * S).apply_index_settings(settings).unwrap();
    let zero = PlannedGrowth::Bounded {
        db: 0,
        wal: 0,
        shm: 0,
    };
    assert_eq!(
        fx.control(3 * S).capacity_status(zero).unwrap().state,
        CapacityState::IndexPaused(IndexPauseReason::SettingsAdjustment)
    );
    assert_eq!(
        fx.control(4 * S).step(&lease, 4 * S).unwrap(),
        IndexStepResult::Progress
    );
    assert_eq!(fx.row("run-a").state, "evicted");
    let mut restarted = fx.peer();
    let lease = restarted.lease(&gui(), 5 * S);
    assert_eq!(
        restarted.row("run-b").state,
        "indexed",
        "restart receipt deletes nothing"
    );
    assert_eq!(restarted.drive(&lease, 6 * S), idle());
    assert_eq!(restarted.rt.counters.evictions, 2);
    assert_eq!(restarted.row("run-c").state, "evicted");
    restarted
        .conn()
        .execute(
            "INSERT INTO conversation_fts(conversation_fts, rank) VALUES('integrity-check', 1)",
            [],
        )
        .unwrap();
}

#[test]
fn admitted_settings_delete_grows_pinned_wal_within_the_planned_bound() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-old", &"deletion WAL ".repeat(400), 10 * S));
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    let page_size = capacity::page_size(fx.conn()).unwrap();
    let growth = MEASURED_POLICY.planned_growth(
        fx.row("run-old").charged,
        page_size,
        capacity::freelist_bytes(fx.conn(), page_size).unwrap(),
        true,
    );
    let PlannedGrowth::Bounded { db, wal, shm } = growth else {
        panic!("bounded deletion");
    };
    let reader = Connection::open(&fx.config.db_path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT COUNT(*) FROM conversation_blocks;")
        .unwrap();
    fx.control(3 * S).apply_index_settings(enabled(0)).unwrap();
    let before = fx.sizes();
    fx.config.hard_limits.max_db_bytes =
        before.total() + db + wal + shm + MEASURED_POLICY.reserve_bytes() + 1;
    fx.config.hard_limits.max_wal_bytes = before.wal + wal + 1;
    assert_eq!(
        fx.control(4 * S).step(&lease, 4 * S).unwrap(),
        IndexStepResult::Progress
    );
    assert_eq!(fx.row("run-old").state, "evicted");
    let after = fx.sizes();
    assert!(
        after.wal > before.wal,
        "logical deletion still appends WAL frames"
    );
    assert!(after.wal <= before.wal + wal);
    assert!(after.total() <= before.total() + db + wal + shm);
    assert!(after.total() + MEASURED_POLICY.reserve_bytes() < fx.config.hard_limits.max_db_bytes);
    assert!(!capacity::checkpoint_truncate(fx.conn()).unwrap());
    reader.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn deletion_outside_the_measured_step_keeps_the_old_index() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified(
        "run-old",
        &"legacy large index ".repeat(200),
        10 * S,
    ));
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    // A future smaller envelope (or an index written by an older policy).
    fx.rt.policy.step_max_text_bytes = 1;
    fx.control(3 * S).apply_index_settings(enabled(0)).unwrap();
    let IndexStepResult::Paused(status) = fx.drive(&lease, 4 * S) else {
        panic!("must pause");
    };
    assert_eq!(
        status.state,
        CapacityState::IndexPaused(IndexPauseReason::GrowthUnbounded)
    );
    assert_eq!(fx.row("run-old").blocks, 1);
    assert_eq!(fx.rt.counters.evictions, 0);
    let env = Env {
        conn: &fx.db.conn,
        db_path: &fx.config.db_path,
        limits: &fx.config.hard_limits,
        source_suspended: false,
    };
    assert!(
        !work::recheck_pause(env, &mut fx.rt).unwrap(),
        "an unbounded delete cannot falsely resume with the insert bound"
    );
}

#[test]
fn replacement_admits_old_delete_and_new_insert_as_one_bounded_step() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-old", &"o".repeat(4_000), 10 * S));
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    fx.save(&verified("run-old", &"n".repeat(4_000), 20 * S));
    // Each half is within the envelope, their combined write is not.
    fx.rt.policy.step_max_text_bytes = 7_999;
    let IndexStepResult::Paused(status) = fx.drive(&lease, 3 * S) else {
        panic!("must pause");
    };
    assert_eq!(
        status.state,
        CapacityState::IndexPaused(IndexPauseReason::GrowthUnbounded)
    );
    assert_eq!(fx.row("run-old").state, "pending");
    assert_eq!(fx.row("run-old").blocks, 1);
    let text: String = fx
        .conn()
        .query_row(
            "SELECT text FROM conversation_blocks WHERE run_id = 'run-old'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(text, "o".repeat(4_000));
}

fn searchable_runs(fx: &Fixture, runs: &[String]) -> Vec<String> {
    fx.db
        .conversation_candidates(&crate::thread_search::ConversationQuery {
            runs,
            kinds: &[IndexBlockKind::Text],
            filter: &crate::thread_search::ConversationFilter::All,
            after: None,
            limit: 100,
        })
        .unwrap()
        .into_iter()
        .map(|candidate| candidate.run_id)
        .collect()
}

#[test]
fn lowering_coefficients_and_budget_keeps_every_run_that_fits_after_reaccounting() {
    for count in [1, 3] {
        let mut fx = Fixture::new(512 * MIB);
        fx.rt.accounting.version += 1;
        fx.rt.accounting.per_run_bytes = 100_000;
        fx.control(S)
            .apply_index_settings(enabled(512 * MIB))
            .unwrap();
        let runs: Vec<String> = (0..count).map(|n| format!("run-{n}")).collect();
        for (n, run) in runs.iter().enumerate() {
            fx.save(&verified(
                run,
                "retained searchable conversation",
                (10 + n as i64) * S,
            ));
        }
        let lease = fx.lease(&gui(), S);
        assert_eq!(fx.drive(&lease, 2 * S), idle());
        let mut policy = DEFAULT_ACCOUNTING_POLICY;
        policy.version = fx.rt.accounting.version + 1;
        let expected: u64 = runs.iter().map(|run| {
            let (text, attrs, blocks) = fx.conn().query_row(
                "SELECT text_bytes, attribute_bytes, block_count FROM thread_index_runs WHERE run_id = ?1",
                [run], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap();
            reaccount_index(text, attrs, blocks, policy).unwrap().charged_bytes
        }).sum();
        assert!(expected < fx.row(&runs[0]).charged);
        fx.rt.accounting = policy;
        fx.control(3 * S)
            .apply_index_settings(enabled(expected + 1))
            .unwrap();
        // The first reaccount must not evict because later runs still have
        // old charges. Search remains available throughout accounting.
        assert_eq!(
            fx.control(4 * S).step(&lease, 4 * S).unwrap(),
            IndexStepResult::Progress
        );
        assert_eq!(
            fx.rt.counters.evictions, 0,
            "mixed old/new total must not cause eviction"
        );
        assert_eq!(searchable_runs(&fx, &runs), runs);
        assert_eq!(fx.drive(&lease, 5 * S), idle());
        assert_eq!(fx.rt.counters.evictions, 0);
        assert_eq!(searchable_runs(&fx, &runs), runs);
        for run in &runs {
            assert_eq!(fx.row(run).state, "indexed");
        }
    }
}

#[test]
fn reaccounting_and_eviction_phases_resume_separately_and_only_remove_the_needed_oldest_run() {
    let mut fx = Fixture::new(512 * MIB);
    fx.rt.accounting.version += 1;
    fx.rt.accounting.per_run_bytes = 100_000;
    fx.control(S)
        .apply_index_settings(enabled(512 * MIB))
        .unwrap();
    let runs = ["run-a", "run-b", "run-c"].map(str::to_owned);
    for run in &runs {
        fx.save(&verified(run, "phase index", 10 * S));
    }
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    let policy = AccountingPolicy {
        version: fx.rt.accounting.version + 1,
        ..DEFAULT_ACCOUNTING_POLICY
    };
    let new_charge = fx.charge("run-a");
    let settings = enabled(2 * new_charge);
    fx.config.thread_index = settings;
    fx.rt.accounting = policy;
    fx.control(3 * S).apply_index_settings(settings).unwrap();
    let zero = PlannedGrowth::Bounded {
        db: 0,
        wal: 0,
        shm: 0,
    };
    assert_eq!(
        fx.control(3 * S).capacity_status(zero).unwrap().adjustment,
        Some(IndexAdjustmentPhase::Reaccounting)
    );
    assert_eq!(
        fx.control(4 * S).step(&lease, 4 * S).unwrap(),
        IndexStepResult::Progress
    );
    assert_eq!(fx.rt.counters.evictions, 0);
    let mut restarted = fx.peer();
    restarted.rt.accounting = policy;
    restarted
        .control(5 * S)
        .apply_index_settings(settings)
        .unwrap();
    let lease = restarted.lease(&gui(), 5 * S);
    for _ in 0..2 {
        assert_eq!(
            restarted.control(6 * S).step(&lease, 6 * S).unwrap(),
            IndexStepResult::Progress
        );
        assert_eq!(restarted.rt.counters.evictions, 0);
        assert_eq!(searchable_runs(&restarted, &runs), runs);
    }
    assert_eq!(
        restarted
            .control(7 * S)
            .capacity_status(zero)
            .unwrap()
            .adjustment,
        Some(IndexAdjustmentPhase::Evicting)
    );
    let mut restarted_again = restarted.peer();
    restarted_again.rt.accounting = policy;
    restarted_again
        .control(8 * S)
        .apply_index_settings(settings)
        .unwrap();
    let lease = restarted_again.lease(&gui(), 8 * S);
    assert_eq!(restarted_again.drive(&lease, 9 * S), idle());
    assert_eq!(restarted_again.rt.counters.evictions, 1);
    assert_eq!(restarted_again.row("run-a").state, "evicted");
    assert_eq!(searchable_runs(&restarted_again, &runs), runs[1..]);
    assert_eq!(
        restarted_again
            .control(10 * S)
            .capacity_status(zero)
            .unwrap()
            .adjustment,
        None
    );
}

#[test]
fn obsolete_extractor_is_hidden_without_owner_during_adjustment_and_capacity_pause() {
    let mut fx = Fixture::new(512 * MIB);
    let runs = vec!["run-a".to_owned(), "run-b".to_owned()];
    for (n, run) in runs.iter().enumerate() {
        fx.save(&verified(
            run,
            "obsolete extractor text",
            (10 + n as i64) * S,
        ));
    }
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    fx.conn()
        .execute_batch(
            "UPDATE thread_index_runs SET extractor_version = 0, indexed_extractor_version = 0;
         UPDATE thread_index_state SET extractor_version = 0;",
        )
        .unwrap();
    let mut restarted = fx.peer(); // settings acceptance, but no owner/step
    let check_hidden = |fx: &Fixture| {
        assert!(
            searchable_runs(fx, &runs).is_empty(),
            "old extractor must not return hits"
        );
        assert!(
            fx.db
                .thread_index_coverage(&runs)
                .unwrap()
                .iter()
                .all(|r| r.coverage == crate::thread_search::RunCoverage::Pending)
        );
    };
    check_hidden(&restarted);
    let lease = restarted.lease(&gui(), 3 * S);
    assert_eq!(
        restarted.control(4 * S).step(&lease, 4 * S).unwrap(),
        IndexStepResult::Progress
    );
    assert_eq!(
        restarted.row("run-b").state,
        "indexed",
        "bounded adjustment leaves later rows untouched"
    );
    check_hidden(&restarted);
    let reader = Connection::open(&restarted.config.db_path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT COUNT(*) FROM conversation_blocks;")
        .unwrap();
    restarted.config.hard_limits.max_wal_bytes = restarted.sizes().wal + 1;
    assert!(matches!(
        restarted.drive(&lease, 5 * S),
        IndexStepResult::Paused(_)
    ));
    check_hidden(&restarted);
    reader.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn stale_unbounded_replacement_resumes_after_a_smaller_revision_without_settings_reload() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-old", &"o".repeat(4_000), 10 * S));
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    fx.save(&verified("run-old", &"n".repeat(4_000), 20 * S));
    fx.rt.policy.step_max_text_bytes = 7_999;
    assert!(matches!(
        fx.drive(&lease, 3 * S),
        IndexStepResult::Paused(_)
    ));
    fx.save(&verified("run-old", "small updated source", 30 * S));
    fx.save(&verified("run-other", "another pending run", 40 * S));
    assert_eq!(
        fx.drive(&lease, 4 * S),
        idle(),
        "stale growth must be discarded without reload/restart"
    );
    assert_eq!(fx.row("run-old").state, "indexed");
    assert_eq!(fx.row("run-other").state, "indexed");
    assert!(fx.rt.ready.is_none());
    assert!(fx.rt.paused_growth.is_none());
}

#[test]
fn parked_background_owner_discards_a_stale_replacement_bound_on_the_next_source_command() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-old", &"o".repeat(4_000), 10 * S));
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    fx.save(&verified("run-old", &"n".repeat(4_000), 20 * S));
    fx.rt.policy.step_max_text_bytes = 7_999;
    assert!(matches!(
        fx.drive(&lease, 3 * S),
        IndexStepResult::Paused(_)
    ));
    let mut job = super::IndexJob::inert();
    std::mem::swap(&mut job.rt, &mut fx.rt);
    // Deliberately no owner: run_due must discard an obsolete bound even
    // before its scheduling gate can authorize another step.
    super::run_due(&mut job, &fx.db.conn, &fx.config, false, false);
    assert!(job.rt.pause.is_some(), "unchanged bound must remain parked");
    fx.save(&verified("run-old", "now smaller", 30 * S));
    super::run_due(&mut job, &fx.db.conn, &fx.config, false, false);
    assert!(job.rt.pause.is_none());
    assert!(job.rt.paused_work.is_none());
    assert!(job.rt.ready.is_none());
    assert!(job.maybe_work);
}

#[test]
fn oversize_reasons_are_distinguished_in_runs_and_request_gaps() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified(
        "run-huge",
        &"h".repeat(MEASURED_POLICY.step_max_text_bytes as usize + 1),
        20 * S,
    ));
    fx.save(&verified("run-small", "small", 10 * S));
    let small = fx.charge("run-small");
    let lease = fx.lease(&gui(), S);
    let request = ReindexRequest {
        canonical_project: None,
        since_ns: None,
        include_evicted: false,
    };
    fx.control(S)
        .apply_index_settings(enabled(small - 1))
        .unwrap();
    let submitted = fx
        .control(S)
        .submit_request(&request, &gui(), S, HOUR)
        .unwrap();
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    let huge = fx.row("run-huge");
    assert_eq!(huge.state, "evicted");
    assert_eq!(huge.evicted_reason.as_deref(), Some("oversized"));
    assert_eq!(huge.last_error_code.as_deref(), Some("oversized_step"));
    assert_eq!(huge.blocks, 0);
    let small_row = fx.row("run-small");
    assert_eq!(
        small_row.last_error_code.as_deref(),
        Some("oversized_budget")
    );
    let status = fx
        .control(3 * S)
        .request_status(&submitted.request_id, 3 * S)
        .unwrap();
    let RequestProgress::Terminal(ReindexResult::CompletedWithGaps { summary, .. }) =
        status.progress
    else {
        panic!("expected gaps: {status:?}");
    };
    let mut reasons: Vec<_> = summary
        .gaps
        .iter()
        .map(|gap| (gap.reason, gap.count))
        .collect();
    reasons.sort_by_key(|(reason, _)| format!("{reason:?}"));
    assert_eq!(
        reasons,
        vec![
            (RequestGapReason::OversizedBudget, 1),
            (RequestGapReason::OversizedStep, 1)
        ]
    );
}

fn ghost(fx: &Fixture, run_id: &str) {
    let mut index = SqliteThreadIndex::new(fx.conn());
    index
        .discover_source(&IndexSourceHeader {
            target: IndexTarget {
                run_id: run_id.into(),
                source_revision: 1,
                extractor_version: EXTRACTOR_VERSION,
                accounting_version: ACCOUNTING_VERSION,
            },
            snapshot_revision: 1,
            context_sha256: String::new(),
            note_seq: None,
            provenance_version: None,
            source_updated_at_ns: S,
            retention_at_ns: S,
            source_present: true,
        })
        .unwrap();
}

#[test]
fn transient_failures_back_off_and_idle_until_the_retry_time() {
    let mut fx = Fixture::new(512 * MIB);
    ghost(&fx, "run-ghost");
    let lease = fx.lease(&gui(), S);
    let mut now = 10 * S;
    let mut delays = Vec::new();
    for attempt in 1..=4 {
        let result = fx.drive(&lease, now);
        let ghost = fx.row("run-ghost");
        assert_eq!(
            (ghost.state.as_str(), ghost.attempts),
            ("retry_wait", attempt)
        );
        let due = ghost.next_attempt_at_ns.unwrap();
        assert_eq!(
            result,
            IndexStepResult::Idle {
                next_wake_at_ns: Some(due)
            }
        );
        delays.push((due - now) / S);
        // Just before the due time nothing is promoted.
        assert_eq!(
            fx.drive(&lease, due - 1),
            IndexStepResult::Idle {
                next_wake_at_ns: Some(due)
            }
        );
        now = due;
    }
    assert_eq!(delays, vec![1, 2, 4, 8]);
    assert_eq!(fx.drive(&lease, now), idle());
    let ghost = fx.row("run-ghost");
    assert_eq!((ghost.state.as_str(), ghost.attempts), ("blocked", 5));
}

#[test]
fn capacity_boundaries_stop_on_equality_and_resume_with_hysteresis() {
    let policy = MEASURED_POLICY;
    let reserve = policy.reserve_bytes();
    assert_eq!(reserve, 64 * MIB);
    let growth = PlannedGrowth::Bounded {
        db: 1_000,
        wal: 2_000,
        shm: 0,
    };
    let limits = |max_db_bytes, max_wal_bytes| HardLimits {
        max_db_bytes,
        max_wal_bytes,
        ..HardLimits::default()
    };
    let measured = |db, wal| Measurement {
        sizes: FileSizes { db, wal, shm: 0 },
        source_suspended: false,
    };
    let state = |limits: &HardLimits, measured, growth, paused| {
        capacity::evaluate(&policy, limits, true, measured, growth, paused).state
    };
    let h = 10 * reserve;
    let p = h - reserve - 3_000;
    let paused = CapacityState::IndexPaused(IndexPauseReason::ReserveInsufficient);
    let wide = limits(h, u64::MAX / 4);
    assert_eq!(
        state(&wide, measured(p - 1, 0), growth, false),
        CapacityState::Normal
    );
    assert_eq!(
        state(&wide, measured(p, 0), growth, false),
        paused,
        "P+G+R = H stops"
    );
    assert_eq!(state(&wide, measured(p + 1, 0), growth, false), paused);
    let j = policy.hysteresis_bytes;
    assert_eq!(
        state(&wide, measured(p - 1, 0), growth, true),
        paused,
        "resume needs +J"
    );
    assert_eq!(
        state(&wide, measured(p - j, 0), growth, true),
        paused,
        "+J equality stays"
    );
    assert_eq!(
        state(&wide, measured(p - j - 1, 0), growth, true),
        CapacityState::Normal
    );
    let wal = limits(h, 10_000);
    assert_eq!(
        state(&wal, measured(0, 7_999), growth, false),
        CapacityState::Normal
    );
    assert_eq!(
        state(&wal, measured(0, 8_000), growth, false),
        CapacityState::IndexPaused(IndexPauseReason::WalLimit)
    );
    assert_eq!(
        state(&wide, measured(0, 0), PlannedGrowth::Unbounded, false),
        CapacityState::IndexPaused(IndexPauseReason::GrowthUnbounded)
    );
    assert_eq!(
        state(
            &limits(reserve, u64::MAX / 4),
            measured(0, 0),
            growth,
            false
        ),
        paused,
        "H that cannot hold the reserve disables indexing"
    );
    let suspended = Measurement {
        source_suspended: true,
        ..measured(0, 0)
    };
    assert_eq!(
        state(&wide, suspended, growth, false),
        CapacityState::SourceSuspended(crate::LimitKind::DbSize)
    );
    assert_eq!(
        capacity::evaluate(&policy, &wide, false, measured(0, 0), growth, false).state,
        CapacityState::IndexPaused(IndexPauseReason::Disabled)
    );
    // The commit bound covers every measured commit (largest charged run).
    let PlannedGrowth::Bounded { db, wal, .. } = policy.planned_growth(3_812_201, 4096, 0, false)
    else {
        panic!("bounded");
    };
    assert!(wal >= 1_310 * 4_120 && db >= 4_161_536);
}

/// `H` such that `P + R + margin = H` at the current file sizes.
fn tight_limits(sizes: FileSizes, margin: u64) -> HardLimits {
    HardLimits {
        max_db_bytes: sizes.total() + MEASURED_POLICY.reserve_bytes() + margin,
        ..HardLimits::default()
    }
}

#[test]
fn logical_room_without_physical_reserve_pauses_without_attempts() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-a", "fits the logical budget easily", 10 * S));
    let lease = fx.lease(&gui(), S);
    fx.config.hard_limits = tight_limits(fx.sizes(), 64 * 1024);
    let result = fx.drive(&lease, 2 * S);
    let IndexStepResult::Paused(status) = result else {
        panic!("expected a pause: {result:?}");
    };
    assert!(matches!(
        status.state,
        CapacityState::IndexPaused(
            IndexPauseReason::ReserveInsufficient | IndexPauseReason::ReclaimExhausted
        )
    ));
    let a = fx.row("run-a");
    assert_eq!((a.state.as_str(), a.attempts, a.blocks), ("pending", 0, 0));
    // The foreground keeps writing inside the reserve while the index pauses.
    fx.save(&verified(
        "run-b",
        "canonical write after the pause",
        20 * S,
    ));
    assert!(fx.sizes().total() < fx.config.hard_limits.max_db_bytes);
}

#[test]
fn reclaim_gives_up_after_two_cycles_without_full_vacuum_on_auto_vacuum_none() {
    let dir = TempDir::new().unwrap();
    {
        let conn = Connection::open(dir.path().join("index.db")).unwrap();
        conn.execute_batch("PRAGMA auto_vacuum = 0; CREATE TABLE keep_none(x);")
            .unwrap();
    }
    let mut fx = Fixture::at(dir, 512 * MIB);
    assert!(!capacity::incremental_vacuum_enabled(fx.conn()).unwrap());
    for index in 0..6 {
        fx.save(&verified(
            &format!("run-{index}"),
            &"z".repeat(20_000),
            (10 + index) * S,
        ));
    }
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    fx.save(&verified("run-late", "one more run", 100 * S));
    let pages_before: i64 = fx
        .conn()
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    // `R < H < P + R`: no amount of index deletion can make room.
    let sizes = fx.sizes();
    fx.config.hard_limits = HardLimits {
        max_db_bytes: MEASURED_POLICY.reserve_bytes() + sizes.total() / 2,
        ..HardLimits::default()
    };
    let before = fx.rt.counters.reclaim_cycles;
    let IndexStepResult::Paused(status) = fx.drive(&lease, 3 * S) else {
        panic!("expected a pause");
    };
    assert_eq!(
        status.state,
        CapacityState::IndexPaused(IndexPauseReason::ReclaimExhausted)
    );
    assert_eq!(fx.rt.counters.reclaim_cycles - before, 2);
    let pages_after: i64 = fx
        .conn()
        .query_row("PRAGMA page_count", [], |row| row.get(0))
        .unwrap();
    assert!(
        pages_after >= pages_before,
        "no full VACUUM shrinks the file"
    );
    // Sticky: re-judged only after a measured decrease or a settings change.
    let IndexStepResult::Paused(again) = fx.drive(&lease, 4 * S) else {
        panic!("still paused");
    };
    assert_eq!(again.state, status.state);
    fx.config.hard_limits = HardLimits::default();
    fx.control(5 * S)
        .apply_index_settings(enabled(512 * MIB))
        .unwrap();
    assert_eq!(fx.drive(&lease, 6 * S), idle());
    assert_eq!(fx.row("run-late").state, "indexed");
}

#[test]
fn a_reader_pinning_the_wal_stops_reclaim_and_stays_sticky() {
    let mut fx = Fixture::new(512 * MIB);
    for index in 0..4 {
        fx.save(&verified(
            &format!("run-{index}"),
            &"w".repeat(20_000),
            (10 + index) * S,
        ));
    }
    let lease = fx.lease(&gui(), S);
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    let reader = Connection::open(&fx.config.db_path).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM run_contexts", [], |row| row.get(0))
        .unwrap();
    fx.save(&verified(
        "run-late",
        "written after the reader began",
        100 * S,
    ));
    fx.config.hard_limits = tight_limits(fx.sizes(), 64 * 1024);
    let IndexStepResult::Paused(status) = fx.drive(&lease, 3 * S) else {
        panic!("expected a pause");
    };
    assert_eq!(
        status.state,
        CapacityState::IndexPaused(IndexPauseReason::WalReaderPinned)
    );
    let env = Env {
        conn: &fx.db.conn,
        db_path: &fx.config.db_path,
        limits: &fx.config.hard_limits,
        source_suspended: false,
    };
    assert!(
        !work::recheck_pause(env, &mut fx.rt).unwrap(),
        "no short-cycle retry"
    );
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    fx.config.hard_limits = HardLimits::default();
    fx.control(4 * S)
        .apply_index_settings(enabled(512 * MIB))
        .unwrap();
    assert_eq!(fx.drive(&lease, 5 * S), idle());
    assert_eq!(fx.row("run-late").state, "indexed");
}

#[test]
fn an_owner_whose_lease_was_taken_cannot_commit_its_late_result() {
    let mut gui_fx = Fixture::new(512 * MIB);
    let mut cli_fx = gui_fx.peer();
    gui_fx.save(&verified("run-a", "shared work", 10 * S));
    let gui_lease = match gui_fx.control(S).acquire_lease(&gui(), S, 10 * S).unwrap() {
        LeaseResult::Acquired(lease) => lease,
        other => panic!("{other:?}"),
    };
    let held = cli_fx
        .control(2 * S)
        .acquire_lease(&cli(), 2 * S, 10 * S)
        .unwrap();
    assert!(matches!(held, LeaseResult::HeldByOther(ref lease) if lease.epoch == gui_lease.epoch));
    // The GUI extracts (result kept in memory), then stalls past its expiry.
    let mut extracted = false;
    for _ in 0..20 {
        gui_fx.control(3 * S).step(&gui_lease, 3 * S).unwrap();
        if gui_fx
            .rt
            .ready
            .as_ref()
            .is_some_and(|(_, result)| result.is_ok())
        {
            extracted = true;
            break;
        }
    }
    assert!(extracted);
    let LeaseResult::Acquired(cli_lease) = cli_fx
        .control(12 * S)
        .acquire_lease(&cli(), 12 * S, HOUR)
        .unwrap()
    else {
        panic!("expired lease is taken over");
    };
    assert_eq!(cli_lease.epoch, gui_lease.epoch + 1);
    assert_eq!(
        gui_fx.control(12 * S).step(&gui_lease, 12 * S).unwrap(),
        IndexStepResult::LeaseLost
    );
    assert_eq!(gui_fx.row("run-a").state, "pending");
    assert_eq!(gui_fx.row("run-a").blocks, 0);
    assert_eq!(
        gui_fx
            .control(13 * S)
            .heartbeat(&gui_lease, 13 * S, HOUR)
            .unwrap(),
        LeaseResult::Lost
    );
    // The stale owner's guard is rejected by the repository as well.
    let input = {
        let bundle = SqliteSnapshotStore::new(gui_fx.conn())
            .load_snapshot_bundle("run-a")
            .unwrap()
            .unwrap();
        extract_snapshot_bundle(&bundle, &gui_fx.redactor).unwrap()
    };
    let generation: u64 = gui_fx
        .conn()
        .query_row(
            "SELECT work_generation FROM thread_index_runs WHERE run_id = 'run-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let guard = IndexCommitGuard {
        lease: gui_lease.clone(),
        target: IndexTarget {
            run_id: "run-a".into(),
            source_revision: input.source_revision,
            extractor_version: EXTRACTOR_VERSION,
            accounting_version: ACCOUNTING_VERSION,
        },
        work_generation: generation,
        expected_state: IndexState::Pending,
        source_present: true,
        request: None,
    };
    let accounting = account_index_input(&input, DEFAULT_ACCOUNTING_POLICY).unwrap();
    assert_eq!(
        SqliteThreadIndex::new(gui_fx.conn())
            .commit_index_input(&guard, &input, accounting, 14 * S)
            .unwrap(),
        IndexCommitResult::Discarded(CommitRejection::LeaseLost)
    );
    for _ in 0..100 {
        if cli_fx.control(15 * S).step(&cli_lease, 15 * S).unwrap() != IndexStepResult::Progress {
            break;
        }
    }
    assert_eq!(cli_fx.row("run-a").state, "indexed");
    cli_fx.control(16 * S).release_lease(&cli_lease).unwrap();
    assert!(super::lease::current(cli_fx.conn()).unwrap().is_none());
}

#[test]
fn concurrent_elections_have_exactly_one_winner() {
    let fx = Fixture::new(512 * MIB);
    let config = fx.config.clone();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = [gui(), cli()]
        .into_iter()
        .map(|owner| {
            let config = config.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let db = Database::open(&config).unwrap();
                barrier.wait();
                super::lease::acquire(&db.conn, &owner, S, HOUR).unwrap()
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let winners = results
        .iter()
        .filter(|result| matches!(result, LeaseResult::Acquired(_)))
        .count();
    assert_eq!(winners, 1, "{results:?}");
    drop(fx);
}

fn all() -> ReindexRequest {
    ReindexRequest {
        canonical_project: None,
        since_ns: None,
        include_evicted: false,
    }
}

fn terminal(fx: &mut Fixture, request_id: &str, now: i64) -> ReindexResult {
    match fx
        .control(now)
        .request_status(request_id, now)
        .unwrap()
        .progress
    {
        RequestProgress::Terminal(result) => result,
        other => panic!("not terminal: {other:?}"),
    }
}

#[test]
fn accepted_request_survives_owner_crash_and_completes_after_takeover() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-a", "alpha", 10 * S));
    fx.save(&verified("run-b", "beta", 20 * S));
    let gui_lease = fx.lease(&gui(), S);
    let submitted = fx
        .control(2 * S)
        .submit_request(&all(), &cli(), 2 * S, 10 * S)
        .unwrap();
    // A live GUI owner: the CLI's answer would be `accepted`.
    assert!(matches!(submitted.lease, LeaseResult::HeldByOther(ref lease) if *lease == gui_lease));
    assert!(
        matches!(submitted.progress, RequestProgress::Pending(ref summary) if summary.target_count == 2)
    );
    // The GUI dies without releasing; its lease expires at 1 h + 1 s.
    let status = fx
        .control(2 * S)
        .request_status(&submitted.request_id, HOUR + 2 * S)
        .unwrap();
    assert_eq!(status.owner, None, "owner absence is diagnosable");
    let LeaseResult::Acquired(cli_lease) = fx
        .control(HOUR + 2 * S)
        .acquire_lease(&cli(), HOUR + 2 * S, HOUR)
        .unwrap()
    else {
        panic!("takeover");
    };
    assert!(cli_lease.epoch > gui_lease.epoch);
    assert_eq!(fx.drive(&cli_lease, HOUR + 3 * S), idle());
    let ReindexResult::Completed { summary, .. } =
        terminal(&mut fx, &submitted.request_id, HOUR + 4 * S)
    else {
        panic!("completed");
    };
    assert_eq!((summary.target_count, summary.succeeded), (2, 2));
}

#[test]
fn request_results_cover_zero_targets_gaps_cancel_and_late_changes() {
    let mut fx = Fixture::new(512 * MIB);
    let lease = fx.lease(&cli(), S);
    let none = ReindexRequest {
        canonical_project: Some("/no/such/project".into()),
        ..all()
    };
    let empty = fx
        .control(S)
        .submit_request(&none, &cli(), S, HOUR)
        .unwrap();
    assert!(matches!(
        empty.progress,
        RequestProgress::Terminal(ReindexResult::Completed { ref summary, .. }) if summary.target_count == 0
    ));
    assert_eq!(
        fx.control(S).submit_request(
            &ReindexRequest {
                since_ns: Some(-1),
                ..all()
            },
            &cli(),
            S,
            HOUR
        ),
        Err(RequestFailure::InvalidArguments)
    );
    fx.save(&write(
        "run-unknown",
        "provenance unknown",
        10 * S,
        ProvenanceStatus::Unknown {
            reason: ProvenanceUnknownReason::LegacyUnproven,
        },
    ));
    let blocked = fx
        .control(2 * S)
        .submit_request(&all(), &cli(), 2 * S, HOUR)
        .unwrap();
    assert_eq!(fx.drive(&lease, 3 * S), idle());
    let ReindexResult::CompletedWithGaps { summary, .. } =
        terminal(&mut fx, &blocked.request_id, 3 * S)
    else {
        panic!("all-gap is completed_with_gaps, never failed");
    };
    assert_eq!(summary.succeeded, 0);
    assert!(matches!(
        summary.gaps[0].reason,
        RequestGapReason::Blocked(_)
    ));
    // Late source update and deletion after submission.
    fx.save(&verified("run-x", "x one", 20 * S));
    fx.save(&verified("run-y", "y one", 21 * S));
    fx.save(&verified("run-z", "z one", 22 * S));
    let late = fx
        .control(4 * S)
        .submit_request(&all(), &cli(), 4 * S, HOUR)
        .unwrap();
    let shared = fx
        .control(4 * S)
        .submit_request(&all(), &cli(), 4 * S, HOUR)
        .unwrap();
    fx.control(4 * S)
        .cancel_request(&shared.request_id)
        .unwrap();
    fx.save(&verified("run-x", "x two", 30 * S));
    SqliteThreadIndex::new(fx.conn())
        .delete_index_source("run-y")
        .unwrap();
    assert_eq!(fx.drive(&lease, 5 * S), idle());
    let ReindexResult::CompletedWithGaps { summary, .. } =
        terminal(&mut fx, &late.request_id, 5 * S)
    else {
        panic!("late changes are gaps");
    };
    let reasons: Vec<_> = summary.gaps.iter().map(|gap| gap.reason).collect();
    assert!(
        reasons.contains(&RequestGapReason::Superseded),
        "{reasons:?}"
    );
    assert!(reasons.contains(&RequestGapReason::Deleted), "{reasons:?}");
    assert!(summary.succeeded >= 1);
    let cancelled = fx
        .control(6 * S)
        .request_status(&shared.request_id, 6 * S)
        .unwrap();
    assert_eq!(cancelled.progress, RequestProgress::Cancelled);
    assert_eq!(
        fx.row("run-z").state,
        "indexed",
        "a cancel does not cancel shared work"
    );
    // Disabled index: pending targets become index_disabled gaps.
    fx.save(&verified("run-w", "w", 40 * S));
    let disabled = fx
        .control(7 * S)
        .submit_request(&all(), &cli(), 7 * S, HOUR)
        .unwrap();
    fx.control(7 * S)
        .apply_index_settings(ThreadIndexSettings {
            enabled: false,
            max_bytes: 512 * MIB,
        })
        .unwrap();
    assert!(matches!(
        fx.drive(&lease, 8 * S),
        IndexStepResult::Paused(_)
    ));
    let ReindexResult::CompletedWithGaps { summary, .. } =
        terminal(&mut fx, &disabled.request_id, 8 * S)
    else {
        panic!("disabled gaps");
    };
    assert!(
        summary
            .gaps
            .iter()
            .any(|gap| gap.reason == RequestGapReason::IndexDisabled)
    );
}

fn storage(dir: &TempDir, limits: HardLimits) -> Storage {
    Storage::open(StorageConfig {
        db_path: dir.path().join("writer.db"),
        hard_limits: limits,
        ..StorageConfig::default()
    })
    .unwrap()
}

fn wait_for(
    observer: &mpsc::Receiver<ThreadIndexCounters>,
    done: impl Fn(&ThreadIndexCounters) -> bool,
) -> ThreadIndexCounters {
    loop {
        let counters = observer
            .recv_timeout(Duration::from_secs(30))
            .expect("background index step");
        if done(&counters) {
            return counters;
        }
    }
}

#[test]
fn background_job_steps_while_commands_keep_arriving_and_then_sleeps() {
    let dir = TempDir::new().unwrap();
    let storage = storage(&dir, HardLimits::default());
    let handle = storage.handle();
    for (run, at) in [("run-a", 10), ("run-b", 20), ("run-c", 30)] {
        handle
            .save_snapshot_bundle(&verified(run, &format!("{run} text"), at * S))
            .unwrap();
    }
    let (observe, observer) = mpsc::channel();
    handle
        .0
        .send(Command::Index(super::IndexCommand::Observe(observe)))
        .unwrap();
    let (release, blocked) = mpsc::channel();
    handle.block_writer(blocked).unwrap();
    // While the writer is blocked, queue the owner start and then more than 32
    // ordinary commands: they run back to back, so the step taken among them
    // is a forced (non-idle) one.
    let (reply, started) = mpsc::channel();
    handle
        .0
        .send(Command::Index(super::IndexCommand::StartOwner(
            gui(),
            handle.0.clone(),
            reply,
        )))
        .unwrap();
    for _ in 0..40 {
        handle.0.try_send(Command::Usage(Vec::new())).unwrap();
    }
    release.send(()).unwrap();
    assert!(matches!(
        started.recv().unwrap(),
        Ok(LeaseResult::Acquired(_))
    ));
    wait_for(&observer, |counters| counters.commits >= 3);
    let status = handle.thread_index_job_status().unwrap();
    assert!(status.forced_steps >= 1, "{status:?}");
    assert!(status.owner_lease.is_some());
    // Drained: nothing schedules a wake-up until a new command arrives.
    let mut status = handle.thread_index_job_status().unwrap();
    for _ in 0..100_000 {
        if !status.wake_scheduled {
            break;
        }
        status = handle.thread_index_job_status().unwrap();
    }
    assert!(!status.wake_scheduled, "{status:?}");
    assert_eq!(status.retry_wake_in, None);
    assert_eq!(status.paused, None);
    handle.stop_thread_index_owner().unwrap();
}

#[test]
fn paused_and_disabled_jobs_do_not_wake_and_canonical_writes_continue() {
    let dir = TempDir::new().unwrap();
    {
        drop(storage(&dir, HardLimits::default()));
    }
    let sizes = file_sizes(&dir.path().join("writer.db")).unwrap();
    let storage = storage(&dir, tight_limits(sizes, MIB));
    let handle = storage.handle();
    let (observe, observer) = mpsc::channel();
    handle
        .0
        .send(Command::Index(super::IndexCommand::Observe(observe)))
        .unwrap();
    handle
        .save_snapshot_bundle(&verified("run-a", &"p".repeat(4_000), 10 * S))
        .unwrap();
    assert!(matches!(
        handle.start_thread_index_owner(gui()),
        Ok(LeaseResult::Acquired(_))
    ));
    wait_for(&observer, |counters| counters.pauses >= 1);
    let status = handle.thread_index_job_status().unwrap();
    assert!(status.paused.is_some());
    assert!(
        !status.wake_scheduled,
        "a paused index never wakes the writer"
    );
    // Canonical snapshots and notes still fit inside the reserve.
    handle
        .save_snapshot_bundle(&verified("run-b", "after pause", 20 * S))
        .unwrap();
    handle
        .append_indexed_note("run-b", "note after pause")
        .unwrap();
    let capacity = handle.thread_index_capacity().unwrap();
    assert!(matches!(capacity.state, CapacityState::IndexPaused(_)));
    handle
        .apply_thread_index_settings(ThreadIndexSettings {
            enabled: false,
            max_bytes: 512 * MIB,
        })
        .unwrap();
    let status = handle.thread_index_job_status().unwrap();
    assert!(
        !status.wake_scheduled,
        "a disabled index never wakes the writer"
    );
}

#[test]
fn legacy_proof_supersedes_a_frozen_request_and_a_new_request_completes() {
    let mut fx = Fixture::new(512 * MIB);
    fx.conn().execute(
        "INSERT INTO run_contexts VALUES ('legacy','worker','worker',NULL, \
         '{\"thread_id\":\"legacy\",\"parent_run_id\":null,\"continuity\":{\"inherited_note\":null,\"sources\":[]}}', \
         '[{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"legacy native\"}]}]', \
         '[]','Done',1,1,0)", [],
    ).unwrap();
    fx.conn().execute("INSERT INTO threads(thread_id,root_run_id,created_at_ns,updated_at_ns,source) VALUES('legacy','legacy',1,1,'runtime')",[]).unwrap();
    fx.conn()
        .execute(
            "INSERT INTO thread_runs VALUES('legacy','legacy','legacy',0)",
            [],
        )
        .unwrap();
    let first = fx
        .control(S)
        .submit_request(&all(), &cli(), S, HOUR)
        .unwrap();
    let LeaseResult::Acquired(lease) = first.lease else {
        panic!("owner")
    };
    let IndexStepResult::Idle { .. } = fx.drive(&lease, S) else {
        panic!("drained")
    };
    let ReindexResult::CompletedWithGaps { summary, .. } = terminal(&mut fx, &first.request_id, S)
    else {
        panic!("frozen target must not be rebound")
    };
    assert_eq!(
        summary.gaps,
        vec![RequestGapCount {
            reason: RequestGapReason::Superseded,
            count: 1
        }]
    );
    let second = fx
        .control(2 * S)
        .submit_request(&all(), &cli(), 2 * S, HOUR)
        .unwrap();
    fx.drive(&lease, 2 * S);
    assert!(
        matches!(terminal(&mut fx,&second.request_id,2*S), ReindexResult::Completed { summary, .. } if summary.succeeded == 1)
    );
    assert!(matches!(
        terminal(&mut fx, &first.request_id, 2 * S),
        ReindexResult::CompletedWithGaps { .. }
    ));
}

#[test]
fn disabled_submission_finishes_even_when_another_owner_is_parked() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-a", "disabled source", 1));
    fx.lease(&gui(), S);
    fx.control(S)
        .apply_index_settings(ThreadIndexSettings {
            enabled: false,
            max_bytes: 512 * MIB,
        })
        .unwrap();
    let submitted = fx
        .control(S)
        .submit_request(&all(), &cli(), S, HOUR)
        .unwrap();
    assert!(matches!(submitted.lease, LeaseResult::HeldByOther(_)));
    assert!(
        matches!(submitted.progress, RequestProgress::Terminal(ReindexResult::CompletedWithGaps { summary, .. })
        if summary.gaps == vec![RequestGapCount { reason:RequestGapReason::IndexDisabled,count:1 }])
    );
}

#[test]
fn capacity_paused_submission_finishes_without_waiting_for_the_owner() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-a", "reserve unavailable", 1));
    fx.lease(&gui(), S);
    fx.config.hard_limits.max_db_bytes = MEASURED_POLICY.reserve_bytes();
    let submitted = fx
        .control(S)
        .submit_request(&all(), &cli(), S, HOUR)
        .unwrap();
    assert!(matches!(submitted.progress,
        RequestProgress::Terminal(ReindexResult::CompletedWithGaps { summary, .. })
        if summary.gaps == vec![RequestGapCount { reason:RequestGapReason::CapacityPaused,count:1 }]));
}

#[test]
fn only_the_live_epoch_can_persist_control_failures() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-a", "control failure", 1));
    let submitted = fx.control(S).submit_request(&all(), &gui(), S, S).unwrap();
    let LeaseResult::Acquired(old) = submitted.lease else {
        panic!("owner")
    };
    let new = fx.lease(&cli(), 3 * S);
    super::requests::fail_pending(fx.conn(), &old, 3 * S).unwrap();
    assert!(matches!(
        fx.control(3 * S)
            .request_status(&submitted.request_id, 3 * S)
            .unwrap()
            .progress,
        RequestProgress::Pending(_)
    ));
    super::requests::fail_pending(fx.conn(), &new, 3 * S).unwrap();
    assert!(matches!(
        terminal(&mut fx, &submitted.request_id, 3 * S),
        ReindexResult::Failed {
            reason: RequestFailure::UnrecoverableControlFailure,
            ..
        }
    ));
}

#[test]
fn a_new_epoch_in_the_same_process_discards_the_previous_extraction() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-a", "old epoch", 1));
    let LeaseResult::Acquired(old) = fx.control(S).acquire_lease(&gui(), S, S).unwrap() else {
        panic!("owner")
    };
    for _ in 0..10 {
        fx.control(S).step(&old, S).unwrap();
        if fx.rt.ready.is_some() {
            break;
        }
    }
    assert!(fx.rt.ready.is_some());
    let new = fx.lease(&gui(), 3 * S);
    assert!(new.epoch > old.epoch);
    fx.control(3 * S).step(&new, 3 * S).unwrap();
    assert_eq!(fx.row("run-a").state, "pending");
    assert_eq!(fx.row("run-a").blocks, 0);
    assert_eq!(fx.rt.counters.discarded, 1);
    fx.drive(&new, 3 * S);
    assert_eq!(fx.row("run-a").state, "indexed");
}

#[test]
fn a_changed_target_cannot_evict_older_indexes_before_its_commit_is_rejected() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("old", &"o".repeat(4096), 1));
    let lease = fx.lease(&gui(), S);
    fx.drive(&lease, S);
    fx.rt.settings.max_bytes = fx.charge("old") + 10;
    fx.save(&verified("new", &"n".repeat(4096), 2));
    for _ in 0..10 {
        fx.control(S).step(&lease, S).unwrap();
        if fx.rt.ready.is_some() {
            break;
        }
    }
    assert!(fx.rt.ready.is_some());
    let evictions = fx.rt.counters.evictions;
    fx.save(&verified("new", "replacement", 3));
    fx.control(S).step(&lease, S).unwrap();
    assert_eq!(fx.row("old").state, "indexed");
    assert_eq!(fx.rt.counters.evictions, evictions);
    assert_eq!(fx.rt.counters.discarded, 1);
}

#[test]
fn public_cli_steps_use_one_extractor_without_starting_a_gui_owner() {
    let dir = TempDir::new().unwrap();
    let storage = storage(&dir, HardLimits::default());
    let handle = storage.handle();
    handle
        .save_snapshot_bundle(&verified("run-a", "foreground outside writer", 1))
        .unwrap();
    let (sender, receiver) = mpsc::channel();
    handle
        .0
        .send(Command::Index(super::IndexCommand::Observe(sender)))
        .unwrap();
    let submitted = handle
        .submit_thread_index_request(&all(), &cli(), S, HOUR)
        .unwrap();
    let LeaseResult::Acquired(lease) = submitted.lease else {
        panic!("owner")
    };
    // Initial merge is a separate no-op step, then extraction is dispatched.
    handle.step_thread_index(&lease, S).unwrap();
    handle.step_thread_index(&lease, S).unwrap();
    receiver.recv_timeout(Duration::from_secs(30)).unwrap();
    // The extractor notification is a barrier; no real-clock sleep advances it.
    assert!(
        handle
            .thread_index_job_status()
            .unwrap()
            .owner_lease
            .is_none()
    );
    handle
        .append_indexed_note("other", "canonical writer remains available")
        .unwrap();
    for _ in 0..100 {
        handle.step_thread_index(&lease, S).unwrap();
        if matches!(
            handle
                .thread_index_request_status(&submitted.request_id, S)
                .unwrap()
                .progress,
            RequestProgress::Terminal(ReindexResult::Completed { .. })
        ) {
            return;
        }
    }
    panic!("request did not finish");
}

#[test]
fn stopping_an_owner_never_joins_a_worker_blocked_on_a_full_writer_queue() {
    let (sender, receiver) = mpsc::sync_channel(1);
    sender.send(Command::Usage(Vec::new())).unwrap();
    let (ready, started) = mpsc::channel();
    let (jobs, _work) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        ready.send(()).unwrap();
        let _ = sender.send(Command::Usage(Vec::new()));
    });
    started.recv_timeout(Duration::from_secs(30)).unwrap();
    let conn = Connection::open_in_memory().unwrap();
    let mut job = super::IndexJob::inert();
    job.extractor = Some(super::Extractor {
        jobs,
        _handle: worker,
    });
    job.in_flight = true;
    super::stop_owner(&mut job, &conn).unwrap();
    assert!(
        job.in_flight,
        "restart must retain the single outstanding worker"
    );
    let extractor = job.extractor.take().unwrap();
    drop(receiver);
    extractor._handle.join().unwrap();
}

// Keep a real WAL high-water, not a mocked file size. Autocheckpoint must not
// hide the distinction between checkpointable high-water and a pinned reader.
fn wal_high_water(fx: &Fixture) {
    fx.conn()
        .pragma_update(None, "wal_autocheckpoint", 0)
        .unwrap();
    fx.conn().execute_batch("CREATE TABLE wal_pressure(payload BLOB); INSERT INTO wal_pressure VALUES(zeroblob(1048576));").unwrap();
}

#[test]
fn wal_admission_recovers_high_water_before_terminalizing_a_request() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-a", "private body never diagnostic", 1));
    let submitted = fx
        .control(S)
        .submit_request(&all(), &gui(), S, HOUR)
        .unwrap();
    let LeaseResult::Acquired(lease) = submitted.lease else {
        panic!("lease")
    };
    wal_high_water(&fx);
    // Enough for the fixed merge bound only after truncation.
    fx.config.hard_limits.max_wal_bytes = 8_437_792 + fx.sizes().wal;
    assert_eq!(fx.drive(&lease, 2 * S), idle());
    assert_eq!(fx.rt.counters.pauses, 0);
    assert!(matches!(terminal(&mut fx, &submitted.request_id, 2*S),
        ReindexResult::Completed { summary, .. } if summary.succeeded == 1));
}

#[test]
fn wal_admission_pinned_reader_pauses_without_index_writes_and_gap_stays_terminal() {
    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-a", "private body never diagnostic", 1));
    let submitted = fx
        .control(S)
        .submit_request(&all(), &gui(), S, HOUR)
        .unwrap();
    let LeaseResult::Acquired(lease) = submitted.lease else {
        panic!("lease")
    };
    let reader = Connection::open(&fx.config.db_path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM run_contexts;")
        .unwrap();
    wal_high_water(&fx);
    let before = fx.sizes();
    fx.config.hard_limits.max_wal_bytes = 8_437_792 + before.wal;
    let IndexStepResult::Paused(status) = fx.drive(&lease, 2 * S) else {
        panic!("pause")
    };
    assert_eq!(
        status.state,
        CapacityState::IndexPaused(IndexPauseReason::WalLimit)
    );
    assert_eq!(fx.rt.counters.merges, 0);
    assert_eq!(fx.rt.counters.commits, 0);
    assert_eq!(fx.row("run-a").attempts, 0);
    let terminal_before = terminal(&mut fx, &submitted.request_id, 2 * S);
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    // At equality with +J even a truncated WAL must remain paused.
    fx.config.hard_limits.max_wal_bytes = 8_437_792 + MEASURED_POLICY.hysteresis_bytes;
    let env = Env {
        conn: &fx.db.conn,
        db_path: &fx.config.db_path,
        limits: &fx.config.hard_limits,
        source_suspended: false,
    };
    assert!(!work::recheck_pause(env, &mut fx.rt).unwrap());
    fx.config.hard_limits.max_wal_bytes += 1;
    // Total (+R+J) is independently required.
    let PlannedGrowth::Bounded { db, wal, shm } = fx.rt.paused_growth.unwrap() else {
        panic!("bounded")
    };
    fx.config.hard_limits.max_db_bytes = fx.sizes().total()
        + db
        + wal
        + shm
        + MEASURED_POLICY.reserve_bytes()
        + MEASURED_POLICY.hysteresis_bytes;
    let env = Env {
        conn: &fx.db.conn,
        db_path: &fx.config.db_path,
        limits: &fx.config.hard_limits,
        source_suspended: false,
    };
    assert!(!work::recheck_pause(env, &mut fx.rt).unwrap());
    fx.config.hard_limits.max_db_bytes += 1;
    let env = Env {
        conn: &fx.db.conn,
        db_path: &fx.config.db_path,
        limits: &fx.config.hard_limits,
        source_suspended: false,
    };
    assert!(work::recheck_pause(env, &mut fx.rt).unwrap());
    assert_eq!(fx.drive(&lease, 3 * S), idle());
    assert_eq!(fx.row("run-a").state, "indexed");
    assert_eq!(
        terminal(&mut fx, &submitted.request_id, 3 * S),
        terminal_before
    );
}

#[test]
fn wal_merge_exact_stop_and_resume_boundaries() {
    let growth = PlannedGrowth::Bounded {
        db: 8_388_608,
        wal: 8_437_792,
        shm: 32_768,
    };
    let limits = HardLimits::default();
    assert_eq!(limits.max_wal_bytes, 64 * MIB);
    for (wal, paused, normal) in [
        (58_671_071, false, true),
        (58_671_072, false, false),
        (41_893_856, true, false),
        (41_893_855, true, true),
    ] {
        let status = capacity::evaluate(
            &MEASURED_POLICY,
            &limits,
            true,
            Measurement {
                sizes: FileSizes { db: 0, wal, shm: 0 },
                source_suspended: false,
            },
            growth,
            paused,
        );
        assert_eq!(
            status.state == CapacityState::Normal,
            normal,
            "wal={wal} paused={paused}"
        );
    }
}

#[test]
fn wal_checkpoint_attempt_is_single_and_outside_immediate_even_when_busy() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for pinned in [false, true] {
        let mut fx = Fixture::new(512 * MIB);
        fx.save(&verified("run-a", "private body", 1));
        let lease = fx.lease(&gui(), S);
        let reader = Connection::open(&fx.config.db_path).unwrap();
        if pinned {
            reader
                .execute_batch("BEGIN; SELECT count(*) FROM run_contexts;")
                .unwrap();
        }
        wal_high_water(&fx);
        fx.config.hard_limits.max_wal_bytes = 8_437_792 + fx.sizes().wal;
        let checkpoints = Arc::new(AtomicUsize::new(0));
        let observed = checkpoints.clone();
        fx.conn()
            .authorizer(Some(move |ctx: AuthContext<'_>| {
                if let AuthAction::Pragma {
                    pragma_name: "wal_checkpoint",
                    pragma_value,
                } = ctx.action
                {
                    assert_eq!(pragma_value, Some("TRUNCATE"));
                    observed.fetch_add(1, Ordering::SeqCst);
                    // checkpoint_truncate asserts is_autocommit immediately before
                    // this PRAGMA, so moving it into IMMEDIATE fails this test too.
                }
                Authorization::Allow
            }))
            .unwrap();
        let outcome = fx.control(2 * S).step(&lease, 2 * S).unwrap();
        assert_eq!(checkpoints.load(Ordering::SeqCst), 1);
        assert_eq!(matches!(outcome, IndexStepResult::Paused(_)), pinned);
        if pinned {
            reader.execute_batch("COMMIT").unwrap();
        }
    }
}

#[test]
fn wal_retry_rechecks_frozen_revision_generation_and_lease_before_commit() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    for mutation in [
        "UPDATE thread_index_runs SET source_revision = source_revision + 1 WHERE run_id='run-a'",
        "UPDATE thread_index_runs SET work_generation = work_generation + 1 WHERE run_id='run-a'",
        "UPDATE thread_index_lease SET epoch = epoch + 1 WHERE id=1",
    ] {
        let mut fx = Fixture::new(512 * MIB);
        fx.save(&verified("run-a", "private body", 1));
        let lease = fx.lease(&gui(), S);
        for _ in 0..10 {
            if fx.rt.ready.is_some() {
                break;
            }
            assert_eq!(
                fx.control(2 * S).step(&lease, 2 * S).unwrap(),
                IndexStepResult::Progress
            );
        }
        assert!(fx.rt.ready.is_some());
        wal_high_water(&fx);
        // A normal commit bound is smaller than the merge bound.
        fx.config.hard_limits.max_wal_bytes = fx.sizes().wal + 1;
        let peer = Connection::open(&fx.config.db_path).unwrap();
        fx.conn()
            .authorizer(Some(move |ctx: AuthContext<'_>| {
                if let AuthAction::Pragma {
                    pragma_name: "wal_checkpoint",
                    ..
                } = ctx.action
                {
                    peer.execute_batch(mutation).unwrap();
                }
                Authorization::Allow
            }))
            .unwrap();
        let result = fx.control(3 * S).step(&lease, 3 * S).unwrap();
        assert!(matches!(
            result,
            IndexStepResult::Progress | IndexStepResult::LeaseLost
        ));
        assert_eq!(fx.rt.counters.commits, 0);
        assert_eq!(fx.row("run-a").blocks, 0);
    }
}

#[test]
fn wal_retry_rejects_ready_commit_when_only_time_crosses_lease_expiry() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };

    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-a", "expiry fence searchable phrase", 1));
    let submission = fx
        .control(S)
        .submit_request(&all(), &gui(), S, HOUR)
        .unwrap();
    let LeaseResult::Acquired(lease) = submission.lease else {
        panic!("lease")
    };
    for _ in 0..10 {
        if fx.rt.ready.is_some() {
            break;
        }
        assert_eq!(
            fx.control(2 * S).step(&lease, 2 * S).unwrap(),
            IndexStepResult::Progress
        );
    }
    assert!(fx.rt.ready.is_some());
    wal_high_water(&fx);
    fx.config.hard_limits.max_wal_bytes = fx.sizes().wal + 1;
    let clock = Arc::new(AtomicI64::new(3 * S));
    fx.rt.admission_clock = Some(clock.clone());
    fx.conn()
        .authorizer(Some(move |ctx: AuthContext<'_>| {
            if let AuthAction::Pragma {
                pragma_name: "wal_checkpoint",
                ..
            } = ctx.action
            {
                // Neither owner, epoch nor expiry changes: only current time does.
                clock.store(lease.expires_at_ns, Ordering::SeqCst);
            }
            Authorization::Allow
        }))
        .unwrap();
    let before = fx
        .control(3 * S)
        .request_status(&submission.request_id, 3 * S)
        .unwrap();
    let outcome = fx.control(3 * S).step(&lease, 3 * S).unwrap();
    assert_eq!(outcome, IndexStepResult::LeaseLost);
    assert_eq!(super::lease::current(fx.conn()).unwrap(), Some(lease));
    assert_eq!(fx.rt.counters.commits, 0);
    assert_eq!(fx.row("run-a").state, "pending");
    assert_eq!(fx.row("run-a").blocks, 0);
    assert_eq!(fx.row("run-a").charged, 0);
    assert_eq!(fx.row("run-a").indexed_at_ns, None);
    let fts_rows: u64 = fx
        .conn()
        .query_row(
            "SELECT count(*) FROM conversation_fts WHERE conversation_fts MATCH 'expiry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fts_rows, 0);
    assert_eq!(
        fx.control(3 * S)
            .request_status(&submission.request_id, 3 * S)
            .unwrap(),
        before
    );
}

#[test]
fn wal_retry_refreshes_commit_and_request_completion_timestamps() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };

    let mut fx = Fixture::new(512 * MIB);
    fx.save(&verified("run-a", "unexpired retry", 1));
    let submission = fx
        .control(S)
        .submit_request(&all(), &gui(), S, HOUR)
        .unwrap();
    let LeaseResult::Acquired(lease) = submission.lease else {
        panic!("lease")
    };
    for _ in 0..10 {
        if fx.rt.ready.is_some() {
            break;
        }
        assert_eq!(
            fx.control(2 * S).step(&lease, 2 * S).unwrap(),
            IndexStepResult::Progress
        );
    }
    assert!(fx.rt.ready.is_some());
    wal_high_water(&fx);
    fx.config.hard_limits.max_wal_bytes = fx.sizes().wal + 1;
    let clock = Arc::new(AtomicI64::new(3 * S));
    fx.rt.admission_clock = Some(clock.clone());
    fx.conn()
        .authorizer(Some(move |ctx: AuthContext<'_>| {
            if let AuthAction::Pragma {
                pragma_name: "wal_checkpoint",
                ..
            } = ctx.action
            {
                clock.store(4 * S, Ordering::SeqCst);
            }
            Authorization::Allow
        }))
        .unwrap();
    assert_eq!(
        fx.control(3 * S).step(&lease, 3 * S).unwrap(),
        IndexStepResult::Progress
    );
    assert_eq!(fx.row("run-a").indexed_at_ns, Some(4 * S));
    let completed_at: i64 = fx
        .conn()
        .query_row(
            "SELECT finished_at_ns FROM thread_index_requests WHERE request_id=?1",
            [&submission.request_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(completed_at, 4 * S);
    assert!(matches!(terminal(&mut fx, &submission.request_id, 4*S),
        ReindexResult::Completed { summary, .. } if summary.succeeded == 1));
}

#[test]
fn wal_retry_expiration_stops_merge_adjustment_and_self_eviction() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    };

    for operation in ["merge", "adjustment", "self_eviction", "budget_eviction"] {
        let mut fx = Fixture::new(512 * MIB);
        if operation == "budget_eviction" {
            fx.save(&verified("run-old", "older index", 0));
            let lease = fx.lease(&gui(), S);
            assert_eq!(fx.drive(&lease, 2 * S), idle());
        }
        let oversized = "x".repeat(MEASURED_POLICY.step_max_text_bytes as usize + 1);
        let body = if operation == "self_eviction" {
            &oversized
        } else {
            "bounded source"
        };
        fx.save(&verified("run-a", body, 1));
        let lease = fx.lease(&gui(), S);
        if operation == "adjustment" {
            assert_eq!(fx.drive(&lease, 2 * S), idle());
            fx.control(2 * S).apply_index_settings(enabled(0)).unwrap();
        } else if matches!(operation, "self_eviction" | "budget_eviction") {
            for _ in 0..10 {
                if fx.rt.ready.is_some() {
                    break;
                }
                assert_eq!(
                    fx.control(2 * S).step(&lease, 2 * S).unwrap(),
                    IndexStepResult::Progress
                );
            }
            assert!(fx.rt.ready.is_some());
            if operation == "budget_eviction" {
                let max_bytes = fx.charge("run-a");
                assert!(fx.row("run-old").charged <= max_bytes);
                fx.control(2 * S)
                    .apply_index_settings(enabled(max_bytes))
                    .unwrap();
            }
        }
        let before = fx.row("run-a");
        let counters = fx.rt.counters;
        wal_high_water(&fx);
        fx.config.hard_limits.max_wal_bytes =
            fx.sizes().wal + if operation == "merge" { 8_437_792 } else { 1 };
        let clock = Arc::new(AtomicI64::new(3 * S));
        fx.rt.admission_clock = Some(clock.clone());
        fx.conn()
            .authorizer(Some(move |ctx: AuthContext<'_>| {
                if let AuthAction::Pragma {
                    pragma_name: "wal_checkpoint",
                    ..
                } = ctx.action
                {
                    clock.store(lease.expires_at_ns + 1, Ordering::SeqCst);
                }
                Authorization::Allow
            }))
            .unwrap();
        assert_eq!(
            fx.control(3 * S).step(&lease, 3 * S).unwrap(),
            IndexStepResult::LeaseLost,
            "{operation}"
        );
        assert_eq!(super::lease::current(fx.conn()).unwrap(), Some(lease));
        let after = fx.row("run-a");
        assert_eq!(
            (after.state, after.blocks, after.charged),
            (before.state, before.blocks, before.charged),
            "{operation}"
        );
        assert_eq!(fx.rt.counters.commits, counters.commits);
        assert_eq!(fx.rt.counters.merges, counters.merges);
        assert_eq!(fx.rt.counters.evictions, counters.evictions);
        if operation == "budget_eviction" {
            assert_eq!(fx.row("run-old").state, "indexed");
            assert_eq!(fx.row("run-old").blocks, 1);
        }
    }
}
