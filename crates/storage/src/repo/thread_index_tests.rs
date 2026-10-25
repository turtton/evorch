//! ADR 0041 Phase B storage acceptance: atomic bundles, R2 transitions, diff
//! commits with FTS, guards, retry/backoff, eviction and tombstones. Time is
//! always passed explicitly; nothing sleeps.

use rusqlite::{Connection, params};
use secret_guard::SecretRedactor;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::thread_index::{
    IndexTransition, MAX_ATTEMPTS, SqliteSnapshotStore, SqliteThreadIndex, StagedSnapshot,
    index_transition, retry_delay_ns,
};
use crate::thread_index::*;
use crate::threads::ThreadRunRow;
use crate::{Database, RunContextRecord, StorageConfig};

const S: i64 = 1_000_000_000;

fn open(dir: &TempDir) -> Connection {
    let path = dir.path().join("storage.db");
    drop(
        Database::open(&StorageConfig {
            db_path: path.clone(),
            ..StorageConfig::default()
        })
        .unwrap(),
    );
    Connection::open(path).unwrap()
}

fn text(role: &str, body: &str) -> Value {
    json!({"role": role, "content": [{"type": "text", "text": body}]})
}

fn record(run_id: &str, messages: &Value, checkpoints: usize, at: i64) -> RunContextRecord {
    let checkpoints: Vec<Value> = (0..checkpoints)
        .map(|index| json!({"through": index, "summary": text("user", "summary")}))
        .collect();
    RunContextRecord {
        run_id: run_id.into(),
        role: "worker".into(),
        name: "worker".into(),
        parent_run_id: None,
        config_json: "{}".into(),
        messages_json: messages.to_string(),
        checkpoints_json: Value::Array(checkpoints).to_string(),
        terminal_phase: "Done".into(),
        restorable: true,
        updated_at_ns: at,
    }
}

fn native_map(messages: &Value, checkpoints: usize) -> ProvenanceMap {
    let raw = messages.as_array().unwrap();
    let mapped = raw
        .iter()
        .enumerate()
        .filter(|(_, message)| message["role"] != "system")
        .enumerate()
        .map(|(ordinal, (raw_index, message))| MessageProvenance {
            raw_index,
            ordinal,
            blocks: (0..message["content"].as_array().unwrap().len())
                .map(|block_index| BlockProvenance {
                    block_index,
                    origin: BlockOrigin::Native,
                    excluded_reason: None,
                })
                .collect(),
        })
        .collect();
    ProvenanceMap {
        version: PROVENANCE_FORMAT_VERSION,
        raw_message_count: raw.len(),
        checkpoint_count: checkpoints,
        messages: mapped,
    }
}

fn verified(run_id: &str, messages: &Value, checkpoints: usize, at: i64) -> SnapshotWrite {
    SnapshotWrite {
        record: record(run_id, messages, checkpoints, at),
        expected_snapshot_revision: None,
        binding: None,
        status: ProvenanceStatus::Verified {
            evidence: ProvenanceEvidence::CompletePositionMap,
        },
        map: native_map(messages, checkpoints),
    }
}

fn save(conn: &Connection, write: &SnapshotWrite) -> SnapshotReceipt {
    SqliteSnapshotStore::new(conn)
        .save_snapshot_bundle(write)
        .unwrap()
}

fn bundle(conn: &Connection, run_id: &str) -> SnapshotBundle {
    SqliteSnapshotStore::new(conn)
        .load_snapshot_bundle(run_id)
        .unwrap()
        .unwrap()
}

fn status(conn: &Connection, run_id: &str) -> IndexRunStatus {
    SqliteThreadIndex::new(conn)
        .index_run_status(run_id)
        .unwrap()
        .unwrap()
}

fn lease(conn: &Connection, epoch: u64, expires_at_ns: i64) -> IndexLease {
    conn.execute(
        "UPDATE thread_index_lease SET owner_id = 'gui-1', owner_kind = 'gui', epoch = ?1, \
         expires_at_ns = ?2 WHERE id = 1",
        params![epoch as i64, expires_at_ns],
    )
    .unwrap();
    IndexLease {
        owner: IndexOwner {
            id: "gui-1".into(),
            kind: IndexOwnerKind::Gui,
        },
        epoch,
        expires_at_ns,
    }
}

fn guard(conn: &Connection, run_id: &str, lease: &IndexLease) -> IndexCommitGuard {
    let status = status(conn, run_id);
    IndexCommitGuard {
        lease: lease.clone(),
        target: status.source.target,
        work_generation: status.work_generation,
        expected_state: IndexState::Pending,
        source_present: true,
        request: None,
    }
}

fn extracted(conn: &Connection, run_id: &str) -> ExtractedIndexInput {
    extract_snapshot_bundle(&bundle(conn, run_id), &SecretRedactor::from_env()).unwrap()
}

fn accounting(input: &ExtractedIndexInput) -> IndexAccounting {
    IndexAccounting {
        accounting_version: 0,
        text_bytes: input.text_bytes,
        attribute_bytes: 7,
        block_count: input.blocks.len() as u64,
        charged_bytes: input.text_bytes + 100,
    }
}

fn commit(conn: &Connection, guard: &IndexCommitGuard, now: i64) -> IndexCommitResult {
    let input = extracted(conn, &guard.target.run_id);
    SqliteThreadIndex::new(conn)
        .commit_index_input(guard, &input, accounting(&input), now)
        .unwrap()
}

fn commit_current(conn: &Connection, run_id: &str, lease: &IndexLease, now: i64) {
    let guard = guard(conn, run_id, lease);
    assert_eq!(commit(conn, &guard, now), IndexCommitResult::Committed);
}

/// (source_kind, source_key, ordinal, note_seq, text, id).
type BlockRow = (String, String, Option<i64>, Option<i64>, String, i64);

/// Ordered by key.
fn blocks(conn: &Connection, run_id: &str) -> Vec<BlockRow> {
    let mut statement = conn
        .prepare(
            "SELECT source_kind, source_key, ordinal, note_seq, text, id FROM conversation_blocks \
             WHERE run_id = ?1 ORDER BY source_kind, source_key",
        )
        .unwrap();
    statement
        .query_map([run_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn fts_hits(conn: &Connection, needle: &str) -> Vec<i64> {
    let mut statement = conn
        .prepare(
            "SELECT rowid FROM conversation_fts WHERE conversation_fts MATCH ?1 ORDER BY rowid",
        )
        .unwrap();
    statement
        .query_map([format!("\"{needle}\"")], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn fts_integrity(conn: &Connection) {
    conn.execute(
        "INSERT INTO conversation_fts(conversation_fts, rank) VALUES('integrity-check', 1)",
        [],
    )
    .expect("external-content FTS must match conversation_blocks");
}

fn note(conn: &Connection, run_id: &str, body: &str, at: i64) -> NoteReceipt {
    SqliteSnapshotStore::new(conn)
        .append_indexed_note(run_id, body, at)
        .unwrap()
}

#[test]
fn bundled_sqlite_supports_trigram_fts_and_dbstat() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    let version: String = conn
        .query_row("SELECT sqlite_version()", [], |row| row.get(0))
        .unwrap();
    assert!(
        version.as_str() >= "3.34",
        "trigram needs SQLite 3.34+, got {version}"
    );
    let pages: i64 = conn
        .query_row(
            "SELECT count(*) FROM dbstat WHERE name = 'conversation_blocks'",
            [],
            |row| row.get(0),
        )
        .expect("bundled SQLite must provide dbstat for physical measurement");
    assert!(pages >= 1);
    let tokenizer: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'conversation_fts'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(tokenizer.contains("trigram") && tokenizer.contains("columnsize = 0"));
    fts_integrity(&conn);
}

#[test]
fn snapshot_save_commits_revisions_sidecar_binding_and_pending_together() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    let messages = json!([text("system", "sys"), text("user", "hello alpha")]);
    let mut write = verified("run-a", &messages, 0, 10);
    write.binding = Some(ThreadRunRow {
        run_id: "run-a".into(),
        thread_id: "thread-1".into(),
        root_run_id: "run-a".into(),
        depth: 0,
    });
    let first = save(&conn, &write);
    assert_eq!((first.snapshot_revision, first.source_revision), (1, 1));
    let loaded = bundle(&conn, "run-a");
    let provenance = loaded.provenance.clone().unwrap();
    assert_eq!(
        provenance.snapshot_sha256,
        snapshot_sha256(
            &loaded.record.messages_json,
            &loaded.record.checkpoints_json
        )
    );
    assert_eq!(provenance.snapshot_sha256, first.snapshot_sha256);
    assert!(matches!(
        provenance.status,
        ProvenanceStatus::Verified { .. }
    ));
    let thread: String = conn
        .query_row(
            "SELECT thread_id FROM thread_runs WHERE run_id = 'run-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(thread, "thread-1");
    let state = status(&conn, "run-a");
    assert_eq!(state.state, IndexState::Pending);
    assert_eq!(state.source.snapshot_revision, 1);
    assert_eq!(state.source.context_sha256, first.snapshot_sha256);
    assert_eq!(state.source.retention_at_ns, 10);

    write.expected_snapshot_revision = Some(1);
    write.record.updated_at_ns = 20;
    let second = save(&conn, &write);
    assert_eq!((second.snapshot_revision, second.source_revision), (2, 2));
    let state = status(&conn, "run-a");
    assert_eq!(
        (state.work_generation, state.source.retention_at_ns),
        (2, 10)
    );
    assert_eq!(bundle(&conn, "run-a").snapshot_revision, 2);

    // The legacy API saves the same bundle shape but never as verified.
    crate::repo::run_context::upsert(&conn, &record("run-legacy", &messages, 1, 5)).unwrap();
    let legacy = bundle(&conn, "run-legacy");
    let provenance = legacy.provenance.unwrap();
    assert_eq!(
        provenance.status,
        ProvenanceStatus::Unknown {
            reason: ProvenanceUnknownReason::LegacyUnproven
        }
    );
    assert_eq!(
        (
            provenance.map.raw_message_count,
            provenance.map.checkpoint_count
        ),
        (2, 1)
    );
    assert_eq!((legacy.snapshot_revision, legacy.source_revision), (1, 1));
}

#[test]
fn verified_map_that_does_not_match_is_saved_as_unknown() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    let messages = json!([text("user", "one"), text("assistant", "two")]);
    let mut write = verified("run-a", &messages, 0, 10);
    write.map.messages.pop();
    save(&conn, &write);
    assert_eq!(
        bundle(&conn, "run-a").provenance.unwrap().status,
        ProvenanceStatus::Unknown {
            reason: ProvenanceUnknownReason::IncompleteMap
        }
    );
    let error = extract_snapshot_bundle(&bundle(&conn, "run-a"), &SecretRedactor::from_env());
    assert_eq!(
        error,
        Err(ExtractionError::ProvenanceUnknown(
            ProvenanceUnknownReason::IncompleteMap
        ))
    );
}

#[test]
fn revision_conflict_and_invalid_json_change_nothing() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    let messages = json!([text("user", "one")]);
    save(&conn, &verified("run-a", &messages, 0, 10));
    let before = bundle(&conn, "run-a");
    let mut stale = verified("run-a", &json!([text("user", "two")]), 0, 20);
    stale.expected_snapshot_revision = Some(0);
    assert_eq!(
        SqliteSnapshotStore::new(&conn).save_snapshot_bundle(&stale),
        Err(SnapshotApiError::RevisionConflict)
    );
    let mut invalid = verified("run-a", &messages, 0, 30);
    invalid.record.messages_json = "{not json".into();
    assert_eq!(
        SqliteSnapshotStore::new(&conn).save_snapshot_bundle(&invalid),
        Err(SnapshotApiError::InvalidSnapshot)
    );
    assert_eq!(bundle(&conn, "run-a"), before);
    assert!(conn.is_autocommit());
}

#[test]
fn snapshot_and_note_faults_at_every_boundary_leave_the_old_bundle() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    let old_messages = json!([text("user", "old")]);
    let binding = |thread: &str| ThreadRunRow {
        run_id: "run-a".into(),
        thread_id: thread.into(),
        root_run_id: "run-a".into(),
        depth: 0,
    };
    let mut first = verified("run-a", &old_messages, 0, 10);
    first.binding = Some(binding("thread-old"));
    save(&conn, &first);
    note(&conn, "run-a", "old note", 11);
    let before = bundle(&conn, "run-a");
    let before_status = status(&conn, "run-a");
    let new_messages = json!([text("user", "old"), text("assistant", "new")]);
    for table in [
        "run_contexts",
        "run_context_provenance",
        "thread_runs",
        "thread_index_runs",
    ] {
        conn.execute_batch(&format!(
            "CREATE TEMP TRIGGER inject_insert BEFORE INSERT ON main.{table} \
             BEGIN SELECT RAISE(ABORT, 'injected'); END; \
             CREATE TEMP TRIGGER inject_update BEFORE UPDATE ON main.{table} \
             BEGIN SELECT RAISE(ABORT, 'injected'); END;"
        ))
        .unwrap();
        let mut second = verified("run-a", &new_messages, 1, 20);
        second.binding = Some(binding("thread-new"));
        assert!(
            SqliteSnapshotStore::new(&conn)
                .save_snapshot_bundle(&second)
                .is_err(),
            "{table}"
        );
        if matches!(table, "thread_index_runs") {
            assert!(
                SqliteSnapshotStore::new(&conn)
                    .append_indexed_note("run-a", "new", 21)
                    .is_err()
            );
        }
        conn.execute_batch("DROP TRIGGER temp.inject_insert; DROP TRIGGER temp.inject_update;")
            .unwrap();
        assert!(
            conn.is_autocommit(),
            "{table}: no transaction may stay open"
        );
        assert_eq!(
            bundle(&conn, "run-a"),
            before,
            "{table}: old bundle must stay whole"
        );
        assert_eq!(status(&conn, "run-a"), before_status, "{table}");
        let thread: String = conn
            .query_row(
                "SELECT thread_id FROM thread_runs WHERE run_id = 'run-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(thread, "thread-old", "{table}");
    }
    // After the faults the complete new bundle commits as one unit.
    let receipt = save(&conn, &verified("run-a", &new_messages, 1, 30));
    let after = bundle(&conn, "run-a");
    assert_eq!(
        (
            receipt.snapshot_revision,
            after.provenance.unwrap().snapshot_revision
        ),
        (2, 2)
    );
}

#[test]
fn staged_snapshot_participates_in_the_callers_transaction() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    save(
        &conn,
        &verified("run-a", &json!([text("user", "old")]), 0, 10),
    );
    let before = bundle(&conn, "run-a");
    conn.execute_batch("BEGIN").unwrap();
    let receipt = StagedSnapshot(&conn)
        .stage_snapshot_bundle(&verified("run-a", &json!([text("user", "new")]), 0, 20))
        .unwrap();
    assert_eq!(receipt.snapshot_revision, 2);
    // The caller's evidence write fails after staging: everything rolls back.
    conn.execute_batch("ROLLBACK").unwrap();
    assert_eq!(bundle(&conn, "run-a"), before);
}

#[test]
fn commit_applies_block_diffs_and_keeps_fts_consistent() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    let lease = lease(&conn, 1, 100 * S);
    let v1 = json!([
        text("user", "alpha apple"),
        text("assistant", "bravo banana"),
        text("user", "charlie cherry")
    ]);
    save(&conn, &verified("run-a", &v1, 0, 10));
    note(&conn, "run-a", "note durian", 11);
    commit_current(&conn, "run-a", &lease, S);
    let first = blocks(&conn, "run-a");
    assert_eq!(first.len(), 4);
    assert_eq!(fts_hits(&conn, "banana"), vec![first[1].5]);
    let indexed = status(&conn, "run-a");
    assert_eq!(
        (indexed.state, indexed.indexed_revision),
        (IndexState::Indexed, Some(2))
    );
    assert_eq!(
        (indexed.block_count, indexed.charged_bytes),
        (4, indexed.text_bytes + 100)
    );

    // Update ordinal 1, drop ordinal 2, add a tool_use block on ordinal 1.
    let v2 = json!([
        text("user", "alpha apple"),
        {"role": "assistant", "content": [
            {"type": "text", "text": "bravo blueberry"},
            {"type": "tool_use", "id": "t1", "name": "grep", "input": {"q": "echo"}}
        ]}
    ]);
    save(&conn, &verified("run-a", &v2, 0, 20));
    commit_current(&conn, "run-a", &lease, 2 * S);
    let second = blocks(&conn, "run-a");
    let keys: Vec<&str> = second.iter().map(|block| block.1.as_str()).collect();
    assert_eq!(keys, ["0:0", "1:0", "1:1", "working_note"]);
    assert_eq!(second[0].5, first[0].5, "unchanged blocks keep their row");
    assert_eq!(
        second[1].5, first[1].5,
        "updated blocks are upserted in place"
    );
    assert!(fts_hits(&conn, "banana").is_empty() && fts_hits(&conn, "cherry").is_empty());
    assert_eq!(fts_hits(&conn, "blueberry"), vec![second[1].5]);
    fts_integrity(&conn);

    // Compaction-equivalent: checkpoints grow, raw messages stay; nothing moves.
    save(&conn, &verified("run-a", &v2, 2, 30));
    commit_current(&conn, "run-a", &lease, 3 * S);
    assert_eq!(blocks(&conn, "run-a"), second);

    // Same note text under a new seq: the hash matches but the locator updates.
    let renote = note(&conn, "run-a", "note durian", 40);
    commit_current(&conn, "run-a", &lease, 4 * S);
    let third = blocks(&conn, "run-a");
    assert_eq!(third[3].3, Some(renote.note.seq as i64));
    assert_eq!(third[3].5, second[3].5);
    assert_eq!(fts_hits(&conn, "durian"), vec![third[3].5]);
    fts_integrity(&conn);
}

#[test]
fn stale_or_misdirected_work_is_discarded() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    let messages = json!([text("user", "alpha")]);
    save(&conn, &verified("run-a", &messages, 0, 10));
    let current = lease(&conn, 3, 100 * S);
    let base = guard(&conn, "run-a", &current);
    let discarded = |guard: &IndexCommitGuard, now: i64| match commit(&conn, guard, now) {
        IndexCommitResult::Discarded(reason) => reason,
        IndexCommitResult::Committed => panic!("stale work committed"),
    };
    let mut old_epoch = base.clone();
    old_epoch.lease.epoch = 2;
    assert_eq!(discarded(&old_epoch, S), CommitRejection::LeaseLost);
    assert_eq!(
        discarded(&base, 100 * S),
        CommitRejection::LeaseLost,
        "expired lease"
    );
    let mut wrong_version = base.clone();
    wrong_version.target.extractor_version += 1;
    assert_eq!(
        discarded(&wrong_version, S),
        CommitRejection::VersionChanged
    );
    let mut not_pending = base.clone();
    not_pending.expected_state = IndexState::RetryWait;
    assert_eq!(discarded(&not_pending, S), CommitRejection::StateChanged);

    conn.execute_batch(
        "INSERT INTO thread_index_requests(request_id, include_evicted, state, created_at_ns, \
         finished_at_ns) VALUES ('req-1', 0, 'cancelled', 1, 2)",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO thread_index_request_targets VALUES ('req-1', 'run-a', ?1, ?2, ?3, ?4, \
         'pending', NULL)",
        params![
            base.target.source_revision as i64,
            base.target.extractor_version,
            base.target.accounting_version,
            base.work_generation as i64
        ],
    )
    .unwrap();
    let mut cancelled = base.clone();
    cancelled.request = Some(RequestGuard {
        request_id: "req-1".into(),
        target: base.target.clone(),
        work_generation: base.work_generation,
    });
    assert_eq!(discarded(&cancelled, S), CommitRejection::RequestInactive);
    assert!(
        conn.execute(
            "UPDATE thread_index_request_targets SET source_revision = 9",
            []
        )
        .is_err(),
        "request targets are immutable"
    );

    save(
        &conn,
        &verified("run-a", &json!([text("user", "beta")]), 0, 20),
    );
    assert_eq!(discarded(&base, S), CommitRejection::SourceChanged);
    let failure = SqliteThreadIndex::new(&conn)
        .record_index_failure(&base, IndexFailure::Transient, S)
        .unwrap();
    assert_eq!(
        failure,
        IndexCommitResult::Discarded(CommitRejection::SourceChanged)
    );
    assert_eq!(
        status(&conn, "run-a").attempt_count,
        0,
        "old job must not touch new revision"
    );

    let fresh = guard(&conn, "run-a", &current);
    assert_eq!(commit(&conn, &fresh, S), IndexCommitResult::Committed);
    assert_eq!(
        discarded(&fresh, S),
        CommitRejection::StateChanged,
        "double commit"
    );
    SqliteThreadIndex::new(&conn)
        .evict_index("run-a", IndexEvictionReason::LogicalBudget)
        .unwrap();
    assert_eq!(discarded(&fresh, S), CommitRejection::GenerationChanged);
}

#[test]
fn transient_failures_back_off_then_block_and_survive_restart() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    save(
        &conn,
        &verified("run-a", &json!([text("user", "alpha")]), 0, 10),
    );
    let lease = lease(&conn, 1, i64::MAX);
    let mut now = 1_000 * S;
    for attempt in 1..MAX_ATTEMPTS {
        let guard = guard(&conn, "run-a", &lease);
        let result = SqliteThreadIndex::new(&conn)
            .record_index_failure(&guard, IndexFailure::Transient, now)
            .unwrap();
        assert_eq!(result, IndexCommitResult::Committed);
        let waiting = status(&conn, "run-a");
        let due = now + retry_delay_ns(attempt);
        assert_eq!(
            (waiting.state, waiting.attempt_count),
            (IndexState::RetryWait, attempt)
        );
        assert_eq!(waiting.next_attempt_at_ns, Some(due));
        let mut index = SqliteThreadIndex::new(&conn);
        assert_eq!(index.next_retry_at_ns().unwrap(), Some(due));
        assert_eq!(index.promote_due_retries(due - 1).unwrap(), 0);
        assert_eq!(index.promote_due_retries(due).unwrap(), 1);
        let pending = status(&conn, "run-a");
        assert_eq!(
            (pending.state, pending.attempt_count),
            (IndexState::Pending, attempt)
        );
        now = due;
    }
    assert_eq!(
        [1, 2, 3, 4, 9, 10].map(retry_delay_ns),
        [S, 2 * S, 4 * S, 8 * S, 256 * S, 300 * S]
    );
    let guard = guard(&conn, "run-a", &lease);
    SqliteThreadIndex::new(&conn)
        .record_index_failure(&guard, IndexFailure::Transient, now)
        .unwrap();
    let blocked = status(&conn, "run-a");
    assert_eq!(
        (blocked.state, blocked.attempt_count),
        (IndexState::Blocked, MAX_ATTEMPTS)
    );
    assert_eq!(
        blocked.blocked_reason,
        Some(IndexBlockedReason::AttemptsExhausted)
    );

    drop(conn);
    let conn = open(&dir);
    let mut index = SqliteThreadIndex::new(&conn);
    while !index.discover_batch(10, now).unwrap().complete {}
    index.discover_source(&blocked.source).unwrap();
    assert_eq!(
        status(&conn, "run-a"),
        blocked,
        "restart/rediscovery must not reset attempts"
    );
}

#[test]
fn evicted_run_stays_evicted_after_limit_raise_updates_and_restart() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    let lease = lease(&conn, 1, i64::MAX);
    save(
        &conn,
        &verified("run-a", &json!([text("user", "alpha apple")]), 0, 10),
    );
    commit_current(&conn, "run-a", &lease, S);
    SqliteThreadIndex::new(&conn)
        .evict_index("run-a", IndexEvictionReason::LogicalBudget)
        .unwrap();
    assert!(blocks(&conn, "run-a").is_empty() && fts_hits(&conn, "apple").is_empty());
    assert_eq!(
        index_transition(IndexState::Evicted, IndexEvent::SettingsApplied),
        IndexTransition::Keep
    );
    let receipt = save(
        &conn,
        &verified("run-a", &json!([text("user", "alpha apple")]), 1, 20),
    );
    let note_receipt = note(&conn, "run-a", "later", 21);
    drop(conn);
    let conn = open(&dir);
    let mut index = SqliteThreadIndex::new(&conn);
    while !index.discover_batch(10, 30).unwrap().complete {}
    let evicted = status(&conn, "run-a");
    assert_eq!(evicted.state, IndexState::Evicted);
    assert_eq!(
        evicted.source.target.source_revision,
        note_receipt.source_revision
    );
    assert_eq!(evicted.source.snapshot_revision, receipt.snapshot_revision);
    assert_eq!(
        (
            evicted.text_bytes,
            evicted.charged_bytes,
            evicted.indexed_revision
        ),
        (0, 0, None)
    );
    let reason: String = conn
        .query_row(
            "SELECT evicted_reason FROM thread_index_runs WHERE run_id = 'run-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reason, "logical_budget");
    fts_integrity(&conn);
}

#[test]
fn deleted_source_is_a_permanent_tombstone() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    let lease = lease(&conn, 1, i64::MAX);
    let messages = json!([text("user", "alpha apple")]);
    save(&conn, &verified("run-a", &messages, 0, 10));
    commit_current(&conn, "run-a", &lease, S);
    save(&conn, &verified("run-a", &messages, 1, 20));
    let late = guard(&conn, "run-a", &lease);
    conn.execute_batch(
        "INSERT INTO thread_index_requests(request_id, include_evicted, state, created_at_ns) \
         VALUES ('req-1', 0, 'pending', 1)",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO thread_index_request_targets VALUES ('req-1', 'run-a', 2, 1, 0, ?1, 'pending', NULL)",
        [late.work_generation as i64],
    )
    .unwrap();
    let mut index = SqliteThreadIndex::new(&conn);
    index.delete_index_source("run-a").unwrap();
    index.delete_index_source("run-a").unwrap();
    assert!(blocks(&conn, "run-a").is_empty() && fts_hits(&conn, "apple").is_empty());
    let gap: String = conn
        .query_row(
            "SELECT gap_reason FROM thread_index_request_targets",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<RequestGapReason>(&gap).unwrap(),
        RequestGapReason::Deleted
    );
    let tombstone = status(&conn, "run-a");
    assert_eq!(
        (tombstone.state, tombstone.source.source_present),
        (IndexState::Deleted, false)
    );

    assert_eq!(
        commit(&conn, &late, S),
        IndexCommitResult::Discarded(CommitRejection::SourceDeleted)
    );
    let mut header = tombstone.source.clone();
    header.source_present = true;
    header.target.source_revision += 5;
    SqliteThreadIndex::new(&conn)
        .discover_source(&header)
        .unwrap();
    save(&conn, &verified("run-a", &messages, 2, 30));
    while !SqliteThreadIndex::new(&conn)
        .discover_batch(10, 40)
        .unwrap()
        .complete
    {}
    let after = status(&conn, "run-a");
    assert_eq!(
        (after.state, after.source.source_present),
        (IndexState::Deleted, false)
    );
    assert_eq!(
        after.source.target.source_revision,
        tombstone.source.target.source_revision
    );
    let flagged: bool = conn
        .query_row(
            "SELECT source_after_tombstone FROM thread_index_runs WHERE run_id = 'run-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(flagged);
    let reference = GuardedReference {
        version: GUARDED_REFERENCE_VERSION,
        run_id: "run-a".into(),
        source_revision: 1,
        source: GuardedSource::Message {
            snapshot_revision: 3,
            ordinal: 0,
            block_index: 0,
        },
    };
    assert_eq!(
        SqliteSnapshotStore::new(&conn).resolve_guarded_reference(&reference),
        Ok(GuardedResolution::SourceDeleted)
    );
    // Tombstones for never-indexed runs exist too.
    SqliteThreadIndex::new(&conn)
        .delete_index_source("run-unknown")
        .unwrap();
    assert_eq!(status(&conn, "run-unknown").state, IndexState::Deleted);
    fts_integrity(&conn);
}

#[test]
fn note_change_and_clear_advance_source_and_stale_guarded_references() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    let lease = lease(&conn, 1, i64::MAX);
    save(
        &conn,
        &verified("run-a", &json!([text("user", "alpha")]), 0, 10),
    );
    let first = note(&conn, "run-a", "plan one", 11);
    assert_eq!(first.source_revision, 2);
    assert_eq!(
        bundle(&conn, "run-a").snapshot_revision,
        1,
        "notes keep snapshot_revision"
    );
    let resolve = |seq: u64| {
        SqliteSnapshotStore::new(&conn)
            .resolve_guarded_reference(&GuardedReference {
                version: GUARDED_REFERENCE_VERSION,
                run_id: "run-a".into(),
                source_revision: 2,
                source: GuardedSource::WorkingNote { note_seq: seq },
            })
            .unwrap()
    };
    assert!(matches!(
        resolve(first.note.seq),
        GuardedResolution::WorkingNote(_)
    ));
    commit_current(&conn, "run-a", &lease, S);
    let second = note(&conn, "run-a", "plan two", 12);
    assert_eq!(status(&conn, "run-a").state, IndexState::Pending);
    assert_eq!(resolve(first.note.seq), GuardedResolution::StaleReference);
    assert!(matches!(
        resolve(second.note.seq),
        GuardedResolution::WorkingNote(_)
    ));
    let message_ref = GuardedReference {
        version: GUARDED_REFERENCE_VERSION,
        run_id: "run-a".into(),
        source_revision: 2,
        source: GuardedSource::Message {
            snapshot_revision: 1,
            ordinal: 0,
            block_index: 0,
        },
    };
    assert!(matches!(
        SqliteSnapshotStore::new(&conn).resolve_guarded_reference(&message_ref),
        Ok(GuardedResolution::Message {
            ordinal: 0,
            block_index: 0,
            ..
        })
    ));

    let cleared = note(&conn, "run-a", "", 13);
    assert_eq!(cleared.source_revision, second.source_revision + 1);
    assert_eq!(resolve(second.note.seq), GuardedResolution::StaleReference);
    assert_eq!(
        resolve(cleared.note.seq),
        GuardedResolution::StaleReference,
        "empty note"
    );
    commit_current(&conn, "run-a", &lease, 2 * S);
    let keys: Vec<String> = blocks(&conn, "run-a")
        .into_iter()
        .map(|block| block.1)
        .collect();
    assert_eq!(keys, ["0:0"], "a cleared note leaves no block");
    fts_integrity(&conn);
}

#[test]
fn discovery_scans_headers_newest_first_and_resumes_after_restart() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    for (run_id, at) in [("run-old", 10), ("run-mid", 20)] {
        conn.execute(
            "INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, \
             messages_json, checkpoints_json, terminal_phase, restorable, updated_at_ns) \
             VALUES (?1, 'worker', 'worker', NULL, '{}', '[]', '[]', 'Done', 1, ?2)",
            params![run_id, at],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO run_ledger(run_id, body, created_at_ns) VALUES ('run-note', 'only a note', 30)",
        [],
    )
    .unwrap();
    let batch = SqliteThreadIndex::new(&conn)
        .discover_batch(1, 100)
        .unwrap();
    assert_eq!((batch.discovered, batch.complete), (1, false));
    assert_eq!(status(&conn, "run-note").source.note_seq, Some(1));
    drop(conn);
    let conn = open(&dir);
    let batch = SqliteThreadIndex::new(&conn)
        .discover_batch(1, 100)
        .unwrap();
    assert_eq!((batch.discovered, batch.complete), (1, false));
    assert!(
        SqliteThreadIndex::new(&conn)
            .index_run_status("run-mid")
            .unwrap()
            .is_some()
    );
    assert!(
        SqliteThreadIndex::new(&conn)
            .index_run_status("run-old")
            .unwrap()
            .is_none()
    );
    let batch = SqliteThreadIndex::new(&conn)
        .discover_batch(5, 100)
        .unwrap();
    assert_eq!((batch.discovered, batch.complete), (1, true));
    let legacy = status(&conn, "run-old");
    assert_eq!(legacy.state, IndexState::Pending);
    assert_eq!(
        (
            legacy.source.snapshot_revision,
            legacy.source.context_sha256.as_str()
        ),
        (0, "")
    );
    assert_eq!(
        (
            legacy.source.retention_at_ns,
            legacy.source.provenance_version
        ),
        (10, None)
    );

    // A legacy snapshot has no sidecar and blocks with a reason, never native.
    let error = extract_snapshot_bundle(&bundle(&conn, "run-old"), &SecretRedactor::from_env())
        .unwrap_err();
    assert_eq!(
        error,
        ExtractionError::ProvenanceUnknown(ProvenanceUnknownReason::MissingSidecar)
    );
    let lease = lease(&conn, 1, i64::MAX);
    let guard = guard(&conn, "run-old", &lease);
    let blocked = IndexBlockedReason::Extraction(error);
    SqliteThreadIndex::new(&conn)
        .record_index_failure(&guard, IndexFailure::Blocked(blocked), S)
        .unwrap();
    let state = status(&conn, "run-old");
    assert_eq!(
        (state.state, state.blocked_reason),
        (IndexState::Blocked, Some(blocked))
    );
    assert_eq!(state.last_error_code.as_deref(), Some("provenance_unknown"));
    let pages = SqliteThreadIndex::new(&conn)
        .discover_batch(5, 200)
        .unwrap();
    assert_eq!((pages.discovered, pages.complete), (0, true));
}

#[test]
fn eviction_candidates_are_oldest_first_by_creation_then_run_id() {
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    let lease = lease(&conn, 1, i64::MAX);
    for (run_id, at) in [("run-b", 10), ("run-a", 10), ("run-c", 5), ("run-d", 1)] {
        save(
            &conn,
            &verified(run_id, &json!([text("user", "alpha")]), 0, at),
        );
    }
    for run_id in ["run-a", "run-b", "run-c"] {
        commit_current(&conn, run_id, &lease, S);
    }
    let index = SqliteThreadIndex::new(&conn);
    let order: Vec<String> = index
        .eviction_candidates(10)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(order, ["run-c", "run-a", "run-b"]);
    let total = index.charged_bytes_total().unwrap();
    assert_eq!(total, 3 * (5 + 100));
}

#[test]
fn retention_uses_the_run_id_creation_time_when_it_is_older() {
    // 01m5sgnyy9 = 1,792,931,462,089 ms.
    let run_id = "run-01m5sgnyy9wd51198q2xjae64k";
    let created = run_id_created_at_ns(run_id).unwrap();
    assert_eq!(created, 1_792_931_462_089_000_000);
    assert_eq!(run_id_created_at_ns("run-42"), None);
    let dir = TempDir::new().unwrap();
    let conn = open(&dir);
    save(
        &conn,
        &verified(run_id, &json!([text("user", "alpha")]), 0, created + S),
    );
    assert_eq!(status(&conn, run_id).source.retention_at_ns, created);
}

#[test]
fn transition_matrix_matches_adr_r2() {
    use IndexState::*;
    use IndexTransition as T;
    let states = [Pending, Indexed, RetryWait, Blocked, Evicted, Deleted];
    let reset = T::Pending {
        reset_attempts: true,
    };
    let rows: [(IndexEvent, [T; 6]); 14] = [
        (IndexEvent::DiscoverSameRevision, [T::Keep; 6]),
        (IndexEvent::SettingsApplied, [T::Keep; 6]),
        (
            IndexEvent::SourceUpdated,
            [
                reset,
                reset,
                reset,
                reset,
                T::SourceInfoOnly,
                T::MarkSourceAfterTombstone,
            ],
        ),
        (
            IndexEvent::Reindex {
                include_evicted: false,
            },
            [reset, reset, reset, reset, T::Keep, T::Keep],
        ),
        (
            IndexEvent::Reindex {
                include_evicted: true,
            },
            [reset, reset, reset, reset, reset, T::Keep],
        ),
        (
            IndexEvent::ExtractorChanged,
            [reset, reset, reset, reset, T::Keep, T::Keep],
        ),
        (
            IndexEvent::AccountingChanged,
            [
                T::UpdateAccountingVersion,
                T::Reaccount,
                T::UpdateAccountingVersion,
                T::UpdateAccountingVersion,
                T::UpdateAccountingVersion,
                T::Keep,
            ],
        ),
        (
            IndexEvent::RetryDue,
            [
                T::Keep,
                T::Keep,
                T::Pending {
                    reset_attempts: false,
                },
                T::Keep,
                T::Keep,
                T::Keep,
            ],
        ),
        (
            IndexEvent::Commit,
            [
                T::Indexed,
                T::Discard,
                T::Discard,
                T::Discard,
                T::Discard,
                T::Discard,
            ],
        ),
        (
            IndexEvent::TransientFailure,
            [
                T::CountFailure,
                T::Discard,
                T::Discard,
                T::Discard,
                T::Discard,
                T::Discard,
            ],
        ),
        (
            IndexEvent::Blocked,
            [
                T::Block,
                T::Discard,
                T::Discard,
                T::Discard,
                T::Discard,
                T::Discard,
            ],
        ),
        (IndexEvent::CapacityPaused, [T::Keep; 6]),
        (
            IndexEvent::Evict,
            [T::Evict, T::Evict, T::Evict, T::Evict, T::Keep, T::Keep],
        ),
        (
            IndexEvent::DeleteSource,
            [
                T::Delete,
                T::Delete,
                T::Delete,
                T::Delete,
                T::Delete,
                T::Keep,
            ],
        ),
    ];
    for (event, expected) in rows {
        for (state, expected) in states.into_iter().zip(expected) {
            assert_eq!(
                index_transition(state, event),
                expected,
                "{event:?} × {state:?}"
            );
        }
    }
}

#[test]
fn writer_commands_save_bundles_and_notes_before_replying() {
    let dir = TempDir::new().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("storage.db"),
        ..StorageConfig::default()
    };
    let storage = crate::Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let messages = json!([text("user", "alpha")]);
    let receipt = handle
        .save_snapshot_bundle(&verified("run-a", &messages, 0, 10))
        .unwrap();
    let note = handle.append_indexed_note("run-a", "plan").unwrap();
    assert_eq!(
        handle.append_run_ledger("run-a", "plan 2").unwrap(),
        note.note.seq + 1
    );
    let legacy = handle
        .upsert_run_context_receipt(&record("run-a", &messages, 1, 20))
        .unwrap();
    assert_eq!(legacy.messages_end, 1);
    let reader = Database::open(&config).unwrap();
    let saved = reader.snapshot_bundle("run-a").unwrap().unwrap();
    assert_eq!((receipt.snapshot_revision, saved.snapshot_revision), (1, 2));
    assert_eq!(saved.source_revision, 4);
    assert_eq!(saved.latest_note.unwrap().body, "plan 2");
    let state = reader.thread_index_run_status("run-a").unwrap().unwrap();
    assert_eq!(
        (state.state, state.source.note_seq),
        (IndexState::Pending, Some(note.note.seq + 1))
    );
}
