//! A run saves each snapshot against the revision it last committed, and the
//! receipt of that save comes from the committing transaction, not a read-back.
use super::LoopState;
use super::continuity_tests::stored_fixture_in;
use crate::restore::RestoredState;
use crate::restore::revision::SnapshotRevision;
use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use storage::StorageError;

fn saved_revision(state: &LoopState) -> u64 {
    let runtime = state.runtime().unwrap();
    let store = runtime.shared.run_store.get().unwrap();
    store
        .restore_bundle(state.caller_run_id())
        .unwrap()
        .unwrap()
        .snapshot_revision
}

fn save(
    state: &LoopState,
) -> Result<crate::ownership::SnapshotReference, crate::restore::SnapshotError> {
    crate::restore::persist_prepared_context(state, &state.context)
}

#[tokio::test]
async fn consecutive_saves_chain_revisions_and_return_the_committed_receipt() {
    let (_dir, _storage, runtime, state) = stored_fixture_in("revision-chain.db", true).await;
    assert_eq!(state.context.snapshot_revision.get(), Some(0));
    for expected in 1..=3_u64 {
        let reference = save(&state).unwrap();
        assert_eq!(state.context.snapshot_revision.get(), Some(expected));
        assert_eq!(saved_revision(&state), expected);
        let crate::ownership::SnapshotReference::RunContext { receipt } = reference else {
            panic!("a stored run returns a run-context receipt");
        };
        let record = runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(state.caller_run_id())
            .unwrap()
            .unwrap();
        assert!(
            receipt.matches(&record),
            "receipt describes the stored bytes"
        );
    }
}

#[tokio::test]
async fn a_context_replaced_by_another_save_is_rejected_and_leaves_the_newer_snapshot() {
    let (_dir, _storage, runtime, state) = stored_fixture_in("revision-stale.db", true).await;
    save(&state).unwrap();
    // A second handle read revision 1 as well, and saves first.
    let store = runtime.shared.run_store.get().unwrap();
    let bundle = store
        .restore_bundle(state.caller_run_id())
        .unwrap()
        .unwrap();
    let winner = RestoredState::from_bundle(&bundle).unwrap();
    assert_eq!(winner.snapshot_revision.get(), Some(1));
    let mut record = bundle.record.clone();
    record.terminal_phase = "Done".into();
    let write = winner
        .provenance
        .write(record, winner.snapshot_revision.get().unwrap())
        .unwrap();
    let saved = store.handle.save_snapshot_bundle(&write).unwrap();
    winner.snapshot_revision.commit(saved.snapshot_revision);
    let winning = store
        .restore_bundle(state.caller_run_id())
        .unwrap()
        .unwrap();
    assert_eq!(winning.snapshot_revision, 2);

    let error = save(&state).unwrap_err();
    assert!(
        matches!(
            error,
            crate::restore::SnapshotError::Storage(StorageError::StaleMutation)
        ),
        "{error:?}"
    );
    assert_eq!(state.context.snapshot_revision.get(), Some(1));
    assert_eq!(
        store
            .restore_bundle(state.caller_run_id())
            .unwrap()
            .unwrap(),
        winning
    );
}

#[tokio::test]
async fn a_failed_save_does_not_advance_the_revision() {
    let (dir, _storage, _runtime, state) = stored_fixture_in("revision-failed.db", true).await;
    save(&state).unwrap();
    let injection = rusqlite::Connection::open(dir.path().join("revision-failed.db")).unwrap();
    injection
        .execute_batch(
            "CREATE TRIGGER reject_save BEFORE UPDATE ON run_contexts \
             BEGIN SELECT RAISE(ABORT, 'save rejected'); END;",
        )
        .unwrap();
    assert!(save(&state).is_err());
    assert_eq!(state.context.snapshot_revision.get(), Some(1));
    assert_eq!(saved_revision(&state), 1);
    injection.execute_batch("DROP TRIGGER reject_save").unwrap();
    save(&state).unwrap();
    assert_eq!(state.context.snapshot_revision.get(), Some(2));
    assert_eq!(saved_revision(&state), 2);
}

#[tokio::test]
async fn a_context_without_a_known_revision_is_never_saved() {
    let (_dir, _storage, _runtime, mut state) =
        stored_fixture_in("revision-unknown.db", true).await;
    state.context.snapshot_revision = SnapshotRevision::unknown();
    assert!(matches!(
        save(&state),
        Err(crate::restore::SnapshotError::RevisionUnknown)
    ));
    let runtime = state.runtime().unwrap();
    let store = runtime.shared.run_store.get().unwrap();
    assert!(
        store
            .restore_bundle(state.caller_run_id())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn restoring_keeps_the_read_revision_even_when_provenance_is_unknown() {
    let (_dir, storage, runtime, state) = stored_fixture_in("revision-restore.db", true).await;
    let run = state.caller_run_id();
    save(&state).unwrap();
    save(&state).unwrap();
    // A legacy save has no provenance proof; it is revision 3 all the same.
    let store = runtime.shared.run_store.get().unwrap();
    let record = store.restore_record(run).unwrap().unwrap();
    storage.handle().upsert_run_context(&record).unwrap();
    let bundle = store.restore_bundle(run).unwrap().unwrap();
    assert_eq!(bundle.snapshot_revision, 3);
    let restored = RestoredState::from_bundle(&bundle).unwrap();
    assert_eq!(restored.snapshot_revision.get(), Some(3));
    assert_eq!(
        restored.provenance,
        crate::restore::provenance::ConversationProvenance::unknown()
    );
}

#[tokio::test]
async fn only_a_bundle_read_gives_a_savable_revision_and_a_fork_starts_at_zero() {
    let (_dir, _storage, runtime, state) = stored_fixture_in("revision-fork.db", true).await;
    save(&state).unwrap();
    let store = runtime.shared.run_store.get().unwrap();
    let bundle = store
        .restore_bundle(state.caller_run_id())
        .unwrap()
        .unwrap();
    assert_eq!(
        RestoredState::from_record(&bundle.record)
            .unwrap()
            .snapshot_revision
            .get(),
        None,
        "a record alone does not say which revision it was"
    );
    let boundary = state
        .context
        .messages
        .iter()
        .take(2)
        .filter(|message| message.role != providers::Role::System)
        .count() as u64;
    let fork = RestoredState::for_fork_seed(&bundle.record, boundary).unwrap();
    assert_eq!(
        fork.snapshot_revision.get(),
        Some(0),
        "a fork runs under a new run ID"
    );
}

static DB_NAME: OnceLock<Mutex<String>> = OnceLock::new();
static BODY_SELECTS: AtomicU64 = AtomicU64::new(0);

// SQLite calls this while the statement is valid; it only reads the statement
// text and database file name and bumps an atomic, never re-entering SQLite.
unsafe extern "C" fn trace(
    mask: c_uint,
    _: *mut c_void,
    statement: *mut c_void,
    sql: *mut c_void,
) -> c_int {
    if mask != rusqlite::ffi::SQLITE_TRACE_STMT as c_uint || sql.is_null() {
        return 0;
    }
    // SAFETY: STMT supplies a live sqlite3_stmt and a NUL-terminated SQL string.
    let (file, sql) = unsafe {
        let db = rusqlite::ffi::sqlite3_db_handle(statement.cast());
        let file = rusqlite::ffi::sqlite3_db_filename(db, c"main".as_ptr());
        (
            (!file.is_null()).then(|| CStr::from_ptr(file).to_string_lossy().into_owned()),
            CStr::from_ptr(sql.cast::<c_char>())
                .to_string_lossy()
                .into_owned(),
        )
    };
    let watched = DB_NAME
        .get()
        .map(|name| {
            name.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        })
        .unwrap_or_default();
    if file.is_some_and(|file| file.ends_with(&watched))
        && sql.trim_start().starts_with("SELECT")
        && sql.contains("messages_json")
    {
        BODY_SELECTS.fetch_add(1, Ordering::Relaxed);
    }
    0
}

unsafe extern "C" fn instrument(
    connection: *mut rusqlite::ffi::sqlite3,
    _: *mut *mut c_char,
    _: *const rusqlite::ffi::sqlite3_api_routines,
) -> c_int {
    // SAFETY: SQLite owns this live connection; registration takes no ownership.
    unsafe {
        rusqlite::ffi::sqlite3_trace_v2(
            connection,
            rusqlite::ffi::SQLITE_TRACE_STMT as c_uint,
            Some(trace),
            std::ptr::null_mut(),
        )
    }
}

struct Instrumentation;

impl Instrumentation {
    fn install(db_name: &str) -> Self {
        *DB_NAME
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = db_name.into();
        // SAFETY: instrument only registers a callback; it opens/closes no DB.
        unsafe { rusqlite::auto_extension::register_auto_extension(instrument) }.unwrap();
        Self
    }
}

impl Drop for Instrumentation {
    fn drop(&mut self) {
        rusqlite::auto_extension::cancel_auto_extension(instrument);
    }
}

/// The IO contract: a successful save reads no snapshot body. Every save used
/// to be followed by a full read-back whose only product was the receipt.
#[tokio::test]
async fn a_successful_save_reads_no_snapshot_body() {
    const NAME: &str = "revision-io-contract.db";
    let _trace = Instrumentation::install(NAME);
    let (_dir, _storage, runtime, state) = stored_fixture_in(NAME, false).await;
    let before = BODY_SELECTS.load(Ordering::Relaxed);
    for _ in 0..3 {
        save(&state).unwrap();
    }
    assert_eq!(
        BODY_SELECTS.load(Ordering::Relaxed),
        before,
        "saving must not select run_contexts bodies"
    );
    assert_eq!(state.context.snapshot_revision.get(), Some(3));
    // Positive control: the trace does see a body read.
    runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_bundle(state.caller_run_id())
        .unwrap();
    assert!(BODY_SELECTS.load(Ordering::Relaxed) > before);
}

#[tokio::test]
async fn an_escalation_seed_is_a_new_runs_first_snapshot_and_is_not_saved_twice() {
    let (_dir, _storage, runtime, state) = stored_fixture_in("revision-seed.db", true).await;
    let source = state.caller_run_id();
    let target = runtime.reserve_run_id();
    let seed = |runtime: &crate::AgentRuntime| {
        crate::restore::persist_escalation_seed(
            runtime,
            target,
            source,
            "memo",
            &crate::RunConfig::default(),
            false,
            &Default::default(),
            None,
        )
    };
    assert_eq!(seed(&runtime).unwrap(), 1);
    assert!(matches!(
        seed(&runtime),
        Err(crate::restore::SnapshotError::Storage(
            StorageError::StaleMutation
        ))
    ));
    let store = runtime.shared.run_store.get().unwrap();
    let bundle = store.restore_bundle(target).unwrap().unwrap();
    assert_eq!(bundle.snapshot_revision, 1);
    assert_eq!(
        RestoredState::from_bundle(&bundle)
            .unwrap()
            .snapshot_revision
            .get(),
        Some(1)
    );
}
