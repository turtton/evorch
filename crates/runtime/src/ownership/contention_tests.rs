//! Contention, retry and observability tests for ownership registry writes.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing_subscriber::layer::SubscriberExt;

use super::retry::virtual_time::VirtualClock;
use super::*;

const RUN: &str = "run-1";
const BODY_MARKER: &str = "BODY-MARKER-must-not-be-logged";

struct Fixture {
    _directory: tempfile::TempDir,
    path: PathBuf,
    permit: OwnerPermit,
}

/// An owner whose lease never expires, with `RUN` in an open turn.
fn fixture() -> Fixture {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let mut owner = ThreadOwner::new(
        "thread-1".into(),
        Lease {
            owner_id: "a".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    owner.settings = config::OwnershipConfig::default();
    Registry::open(&path)
        .expect("registry")
        .start(&owner)
        .expect("start");
    let permit = OwnerPermit {
        registry_path: path.clone(),
        thread_id: owner.thread_id,
        lease: owner.lease,
        run_id: Some(RUN.into()),
    };
    permit.begin_turn().expect("begin");
    Fixture {
        _directory: directory,
        path,
        permit,
    }
}

fn messages() -> Vec<providers::Message> {
    vec![
        providers::Message {
            role: providers::Role::User,
            content: vec![providers::ContentBlock::Text {
                text: format!("{BODY_MARKER} \"quoted\" \\ 改行\n"),
            }],
        },
        providers::Message {
            role: providers::Role::Assistant,
            content: vec![providers::ContentBlock::Text {
                text: "x".repeat(64 * 1024),
            }],
        },
    ]
}

fn owner_of(path: &Path) -> ThreadOwner {
    Registry::open_readonly(path)
        .expect("reader")
        .attach("thread-1")
        .expect("owner")
}

fn stored_payload(path: &Path) -> Option<String> {
    use rusqlite::OptionalExtension;
    rusqlite::Connection::open(path)
        .expect("connection")
        .query_row(
            "SELECT payload FROM owner_checkpoints WHERE thread_id = 'thread-1'",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("checkpoint query")
}

fn exclusive_lock(path: &Path) -> rusqlite::Connection {
    let connection = rusqlite::Connection::open(path).expect("lock connection");
    connection
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("exclusive lock");
    connection
}

#[derive(Clone, Default)]
struct Records(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

struct Fields<'a>(&'a mut BTreeMap<String, String>);

impl tracing::field::Visit for Fields<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Records {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut fields = BTreeMap::new();
        fields.insert("target".into(), event.metadata().target().into());
        fields.insert("level".into(), event.metadata().level().to_string());
        event.record(&mut Fields(&mut fields));
        self.0.lock().unwrap().push(fields);
    }
}

impl Records {
    fn dispatch(&self) -> tracing::Dispatch {
        tracing::Dispatch::new(tracing_subscriber::registry().with(self.clone()))
    }

    fn find(
        &self,
        predicate: impl Fn(&BTreeMap<String, String>) -> bool,
    ) -> Vec<BTreeMap<String, String>> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|record| predicate(record))
            .cloned()
            .collect()
    }

    fn assert_no_body(&self) {
        for record in self.0.lock().unwrap().iter() {
            for value in record.values() {
                assert!(!value.contains(BODY_MARKER), "body leaked into {record:?}");
                assert!(
                    !value.contains("xxxxxxxx"),
                    "payload leaked into {record:?}"
                );
            }
        }
    }
}

fn field<'a>(record: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    record.get(name).map(String::as_str)
}

// ---- observability ----

#[test]
fn a_busy_registry_commit_records_operation_phase_and_size_but_no_body() {
    let fixture = fixture();
    let before = owner_of(&fixture.path);
    let records = Records::default();
    let dispatch = records.dispatch();
    // Held until the commit has failed, so its single busy wait always expires.
    let reader = fixture.permit.mutation_guard().expect("reader guard");
    let messages = messages();
    // The registry API itself never retries.
    let result = tracing::dispatcher::with_default(&dispatch, || {
        Registry::open_existing(&fixture.path)
            .unwrap()
            .checkpoint_permit(&fixture.permit, &messages)
    });
    drop(reader);
    let error = result.expect_err("the reader outlasts one busy wait");
    assert_eq!(
        error.to_string(),
        "ownership checkpoint failed at commit (attempt 1): database is locked"
    );
    assert_eq!(owner_of(&fixture.path), before);
    assert_eq!(stored_payload(&fixture.path), None);
    let failures = records.find(|record| {
        field(record, "target") == Some(DB_TRACE_TARGET)
            && field(record, "message") == Some("ownership registry operation failed")
    });
    assert_eq!(failures.len(), 1, "{failures:?}");
    let record = &failures[0];
    assert_eq!(field(record, "operation"), Some("checkpoint"));
    assert_eq!(field(record, "phase"), Some("commit"));
    assert_eq!(field(record, "attempt"), Some("1"));
    assert_eq!(field(record, "sqlite_code"), Some("Some(DatabaseBusy)"));
    assert_eq!(field(record, "extended_code"), Some("5"));
    assert_eq!(field(record, "thread_id"), Some("thread-1"));
    assert!(
        field(record, "payload_bytes")
            .unwrap()
            .parse::<usize>()
            .unwrap()
            < 1024
    );
    assert!(
        field(record, "elapsed_ms")
            .and_then(|elapsed| elapsed.parse::<u64>().ok())
            .is_some()
    );
    records.assert_no_body();
}

#[cfg(unix)]
#[test]
fn a_contended_claim_is_recorded_as_a_claim() {
    let fixture = fixture();
    let before = owner_of(&fixture.path);
    let records = Records::default();
    let lock = exclusive_lock(&fixture.path);
    let directory = tempfile::tempdir().expect("socket directory");
    let result = tracing::dispatcher::with_default(&records.dispatch(), || {
        ipc::claim(
            &mut Registry::open_existing(&fixture.path).unwrap(),
            ipc::ClaimRequest {
                thread_id: "thread-1",
                expected: &before.lease,
                previous_socket: &directory.path().join("absent.sock"),
                owner_id: "b",
                now_ms: u64::MAX,
                grace_ms: 0,
            },
        )
    });
    lock.execute_batch("ROLLBACK").unwrap();
    let error = result.expect_err("an exclusive lock blocks the claim");
    assert!(
        matches!(
            error,
            RegistryError::Database {
                operation: DbOperation::Claim,
                phase: DbPhase::Begin,
                attempt: 1,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "ownership claim failed at begin (attempt 1): database is locked"
    );
    let failures = records
        .find(|record| field(record, "message") == Some("ownership registry operation failed"));
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(field(&failures[0], "operation"), Some("claim"));
    assert_eq!(owner_of(&fixture.path), before);
}

#[test]
fn a_locked_emit_guard_records_its_phase_without_recursion_or_body() {
    let fixture = fixture();
    let records = Records::default();
    let bus = event_bus::EventBus::new(8);
    let _events = bus.subscribe();
    let permit = fixture.permit.clone();
    assert!(bus.register_mutation_guard(
        RUN.into(),
        Arc::new(move || {
            permit
                .mutation_guard()
                .ok()
                .map(|guard| Box::new(guard) as Box<dyn event_bus::MutationGuard>)
        })
    ));
    let lock = exclusive_lock(&fixture.path);
    let delivered = tracing::dispatcher::with_default(&records.dispatch(), || {
        bus.emit(event_bus::Event::new(
            event_bus::MessageEvent::MessageCompleted {
                run_id: RUN.into(),
                text: BODY_MARKER.into(),
            },
        ))
    });
    lock.execute_batch("ROLLBACK").unwrap();
    assert_eq!(delivered, 0);
    let guard = records.find(|record| {
        field(record, "operation") == Some("guard")
            && field(record, "message") == Some("ownership registry operation failed")
    });
    assert_eq!(guard.len(), 1, "one failure, no recursive guard: {guard:?}");
    assert_eq!(field(&guard[0], "phase"), Some("read"));
    assert_eq!(field(&guard[0], "sqlite_code"), Some("Some(DatabaseBusy)"));
    assert_eq!(field(&guard[0], "level"), Some("WARN"));
    let dropped = records.find(|record| field(record, "target") == Some("evorch::fencing"));
    assert_eq!(dropped.len(), 1, "{dropped:?}");
    assert_eq!(field(&dropped[0], "family"), Some("message"));
    records.assert_no_body();
}

/// The committed state of a claim by owner `b` at the next generation.
fn claimed(owner: &ThreadOwner) -> ThreadOwner {
    let mut next = owner.clone();
    next.lease = Lease {
        owner_id: "b".into(),
        generation: owner.lease.generation + 1,
        expires_at: u64::MAX,
    };
    next.active_turn = false;
    next.active_runs.clear();
    next
}

// ---- (a) serialization outside the write transaction ----

#[test]
fn stored_checkpoint_has_metadata_not_message_history() {
    let fixture = fixture();
    let messages = messages();
    fixture.permit.checkpoint(&messages).expect("checkpoint");
    let payload = stored_payload(&fixture.path).unwrap();
    assert!(super::OwnerCheckpoint::decode(&payload).is_some());
    assert!(!payload.contains(BODY_MARKER));
    let owner = owner_of(&fixture.path);
    assert!(owner.active_runs.is_empty());
    assert!(!owner.active_turn);
}

struct FailingPayload(Arc<AtomicUsize>);

impl serde::Serialize for FailingPayload {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(serde::ser::Error::custom(BODY_MARKER))
    }
}

#[test]
fn a_serialization_failure_leaves_the_owner_untouched_and_is_not_retried() {
    let fixture = fixture();
    let before = owner_of(&fixture.path);
    let calls = Arc::new(AtomicUsize::new(0));
    let records = Records::default();
    let result = tracing::dispatcher::with_default(&records.dispatch(), || {
        fixture.permit.checkpoint_payload(
            &FailingPayload(Arc::clone(&calls)),
            &RetryPolicy::default(),
            &retry::system_clock(),
            &|| false,
        )
    });
    assert!(matches!(result, Err(RegistryError::Json(_))), "{result:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(owner_of(&fixture.path), before);
    assert_eq!(stored_payload(&fixture.path), None);
    let serialize = records.find(|record| field(record, "phase") == Some("serialize"));
    assert_eq!(serialize.len(), 1, "{serialize:?}");
    assert_eq!(field(&serialize[0], "operation"), Some("checkpoint"));
    records.assert_no_body();
}

/// Commits a claim while the checkpoint is being serialized: after the
/// permit's early read, before its write transaction.
struct ClaimingPayload {
    path: PathBuf,
}

impl serde::Serialize for ClaimingPayload {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // With serialization inside the write transaction this claim would
        // wait for that transaction and fail with SQLITE_BUSY.
        let mut registry =
            Registry::open_existing(&self.path).map_err(serde::ser::Error::custom)?;
        registry
            .update("thread-1", |owner| {
                *owner = claimed(owner);
                Ok(())
            })
            .map_err(serde::ser::Error::custom)?;
        serializer.serialize_unit()
    }
}

#[test]
fn a_claim_committed_after_serialization_fences_the_old_generation_commit() {
    let fixture = fixture();
    let result = fixture.permit.checkpoint_payload(
        &ClaimingPayload {
            path: fixture.path.clone(),
        },
        &RetryPolicy::default(),
        &retry::system_clock(),
        &|| false,
    );
    assert!(
        matches!(
            result,
            Err(RegistryError::Ownership(OwnershipError::Fenced))
        ),
        "{result:?}"
    );
    let owner = owner_of(&fixture.path);
    assert_eq!(owner.lease.owner_id, "b");
    assert_eq!(owner.lease.generation, 2);
    // Only the claim's own generation-change marker exists, not a payload.
    assert_eq!(stored_payload(&fixture.path).as_deref(), Some("[]"));
}

// ---- (c) bounded retries ----
//
// Lock waits run on a virtual clock: SQLite's busy handler sleeps on it, so a
// 2 s attempt takes no wall time and hooks release locks at exact budget
// points instead of racing real sleeps.

fn is_busy(error: &RegistryError) -> bool {
    error.sqlite_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
}

fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

fn checkpoint_on(
    permit: &OwnerPermit,
    messages: &[providers::Message],
    policy: &RetryPolicy,
    clock: &Arc<VirtualClock>,
    interrupted: &dyn Fn() -> bool,
) -> Result<(), RegistryError> {
    let _ = messages;
    permit.checkpoint_payload(
        &super::checkpoint::CheckpointInput::default(),
        policy,
        &clock.shared(),
        interrupted,
    )
}

/// Run `action` once, from the sleep that reaches `at` on the virtual clock.
fn at(clock: &VirtualClock, at: Duration, action: impl FnOnce(Duration) + Send + 'static) {
    let mut action = Some(action);
    clock.on_sleep(move |elapsed| {
        if elapsed >= at
            && let Some(action) = action.take()
        {
            action(elapsed);
        }
    });
}

fn attempts_recorded(records: &Records, message: &str) -> Vec<String> {
    records
        .find(|record| field(record, "message") == Some(message))
        .iter()
        .filter_map(|record| field(record, "attempts").map(str::to_owned))
        .collect()
}

#[test]
fn a_begin_wait_and_a_commit_wait_share_one_attempt_budget() {
    let fixture = fixture();
    let before = owner_of(&fixture.path);
    // A writer blocks BEGIN IMMEDIATE; a reader then blocks COMMIT.
    let writer = rusqlite::Connection::open(&fixture.path).expect("writer");
    writer
        .execute_batch("BEGIN IMMEDIATE")
        .expect("writer lock");
    let reader = fixture.permit.mutation_guard().expect("reader guard");
    let clock = VirtualClock::new();
    let released = Arc::new(Mutex::new(None));
    let release = Arc::clone(&released);
    at(&clock, ms(1500), move |elapsed| {
        writer
            .execute_batch("ROLLBACK")
            .expect("release the writer");
        *release.lock().unwrap() = Some(elapsed);
    });
    let records = Records::default();
    let policy = RetryPolicy {
        deadline: Some(ms(2000)),
        ..RetryPolicy::default()
    };
    let result = tracing::dispatcher::with_default(&records.dispatch(), || {
        checkpoint_on(&fixture.permit, &messages(), &policy, &clock, &|| false)
    });
    drop(reader);
    let begin_wait = released
        .lock()
        .unwrap()
        .expect("BEGIN waited for the writer");
    assert!(begin_wait >= ms(1500), "{begin_wait:?}");
    let error = result.expect_err("the reader outlasts the attempt");
    assert!(
        matches!(
            error,
            RegistryError::Database {
                operation: DbOperation::Checkpoint,
                phase: DbPhase::Commit,
                attempt: 1,
                ..
            }
        ),
        "{error:?}"
    );
    // COMMIT only had what BEGIN left of the 2 s attempt, not another 2 s.
    assert_eq!(clock.elapsed(), ms(2000));
    let failures = records
        .find(|record| field(record, "message") == Some("ownership registry operation failed"));
    let phases: Vec<_> = failures
        .iter()
        .filter_map(|record| field(record, "phase"))
        .collect();
    assert_eq!(phases, ["commit"], "{failures:?}");
    assert_eq!(owner_of(&fixture.path), before);
    assert_eq!(stored_payload(&fixture.path), None);
}

#[test]
fn checkpoint_succeeds_when_a_reader_releases_during_retries() {
    let fixture = fixture();
    let records = Records::default();
    let dispatch = records.dispatch();
    let reader = tracing::dispatcher::with_default(&dispatch, || {
        fixture.permit.mutation_guard().expect("reader guard")
    });
    let clock = VirtualClock::new();
    // Past the first attempt's 2 s: the second attempt commits.
    let mut reader = Some(reader);
    at(&clock, ms(2100), move |_| drop(reader.take()));
    let messages = messages();
    let result = tracing::dispatcher::with_default(&dispatch, || {
        checkpoint_on(
            &fixture.permit,
            &messages,
            &RetryPolicy::default(),
            &clock,
            &|| false,
        )
    });
    result.expect("contention that clears within the deadline succeeds");
    assert!(clock.elapsed() >= ms(2100), "{:?}", clock.elapsed());
    let payload = stored_payload(&fixture.path).unwrap();
    assert!(super::OwnerCheckpoint::decode(&payload).is_some());
    assert!(!payload.contains(BODY_MARKER));
    assert!(owner_of(&fixture.path).active_runs.is_empty());

    let failures = records.find(|record| {
        field(record, "target") == Some(DB_TRACE_TARGET)
            && field(record, "message") == Some("ownership registry operation failed")
    });
    assert_eq!(failures.len(), 1, "{failures:?}");
    let commit = &failures[0];
    assert_eq!(field(commit, "phase"), Some("commit"));
    assert_eq!(field(commit, "operation"), Some("checkpoint"));
    assert_eq!(field(commit, "attempt"), Some("1"));
    assert_eq!(field(commit, "sqlite_code"), Some("Some(DatabaseBusy)"));
    assert!(
        field(commit, "payload_bytes")
            .unwrap()
            .parse::<usize>()
            .unwrap()
            < 1024
    );
    assert!(field(commit, "db").is_some_and(|db| db.ends_with("owners.db")));
    assert!(field(commit, "elapsed_ms").is_some());
    assert_eq!(
        attempts_recorded(
            &records,
            "ownership registry operation succeeded after contention"
        ),
        ["2"]
    );
    records.assert_no_body();
}

#[test]
fn a_long_guard_hold_is_recorded() {
    let records = Records::default();
    tracing::dispatcher::with_default(&records.dispatch(), || {
        observe::GuardHold {
            thread_id: "thread-1".into(),
            generation: 1,
            since: Instant::now()
                .checked_sub(observe::SLOW_GUARD_HOLD)
                .expect("monotonic clock past the hold threshold"),
        }
        .finish("owners.db");
    });
    let held = records.find(|record| {
        field(record, "message")
            == Some("ownership guard held long enough to delay registry writers")
    });
    assert_eq!(held.len(), 1, "{held:?}");
    assert!(
        field(&held[0], "held_ms")
            .and_then(|held| held.parse::<u64>().ok())
            .is_some_and(|held| held >= 500)
    );
}

#[test]
fn contention_held_past_the_deadline_fails_explicitly_within_it() {
    let fixture = fixture();
    let before = owner_of(&fixture.path);
    let lock = exclusive_lock(&fixture.path);
    let clock = VirtualClock::new();
    let result = checkpoint_on(
        &fixture.permit,
        &messages(),
        &RetryPolicy::default(),
        &clock,
        &|| false,
    );
    lock.execute_batch("ROLLBACK").unwrap();
    let error = result.expect_err("a lock held past the deadline must fail");
    assert!(is_busy(&error), "{error:?}");
    let RegistryError::Database {
        operation, attempt, ..
    } = &error
    else {
        panic!("contextual database error: {error:?}");
    };
    assert_eq!(*operation, DbOperation::Checkpoint);
    // At most 2 s per attempt over 10 s, plus backoff between them.
    assert!(*attempt >= 5, "{error:?}");
    let reason = error.to_string();
    assert!(
        reason.starts_with("ownership checkpoint failed at read"),
        "{reason}"
    );
    assert!(reason.ends_with("database is locked"), "{reason}");
    assert_eq!(clock.elapsed(), ms(10_000));
    assert_eq!(owner_of(&fixture.path), before);
    assert_eq!(stored_payload(&fixture.path), None);
}

#[test]
fn the_system_clock_bounds_a_real_attempt() {
    let fixture = fixture();
    let lock = exclusive_lock(&fixture.path);
    let policy = RetryPolicy {
        deadline: Some(ms(300)),
        attempt_timeout: ms(100),
        ..RetryPolicy::default()
    };
    let result = fixture
        .permit
        .checkpoint_with(&messages(), &policy, &|| false);
    lock.execute_batch("ROLLBACK").unwrap();
    let error = result.expect_err("a held lock outlasts a short real budget");
    assert!(is_busy(&error), "{error:?}");
    assert!(
        matches!(
            error,
            RegistryError::Database {
                phase: DbPhase::Read,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_claim_committed_during_retries_fences_the_old_generation() {
    let fixture = fixture();
    let lock = exclusive_lock(&fixture.path);
    let next = claimed(&owner_of_locked(&lock));
    let clock = VirtualClock::new();
    // Past the first attempt's 2 s busy wait: the checkpoint is retrying.
    let committed = next.clone();
    at(&clock, ms(2400), move |_| {
        lock.execute(
            "UPDATE thread_owners SET state = ?1 WHERE thread_id = 'thread-1'",
            [serde_json::to_string(&committed).unwrap()],
        )
        .unwrap();
        lock.execute_batch("COMMIT").unwrap();
    });
    let result = checkpoint_on(
        &fixture.permit,
        &messages(),
        &RetryPolicy::default(),
        &clock,
        &|| false,
    );
    assert!(clock.elapsed() >= ms(2400));
    assert!(
        matches!(
            result,
            Err(RegistryError::Ownership(OwnershipError::Fenced))
        ),
        "{result:?}"
    );
    assert_eq!(owner_of(&fixture.path), next);
    assert_eq!(stored_payload(&fixture.path), None);
}

fn owner_of_locked(lock: &rusqlite::Connection) -> ThreadOwner {
    let state: String = lock
        .query_row(
            "SELECT state FROM thread_owners WHERE thread_id = 'thread-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&state).unwrap()
}

#[test]
fn an_interrupt_ends_retrying_within_one_attempt_without_committing() {
    let fixture = fixture();
    let before = owner_of(&fixture.path);
    let lock = exclusive_lock(&fixture.path);
    let interrupted = Arc::new(AtomicBool::new(false));
    let clock = VirtualClock::new();
    let flag = Arc::clone(&interrupted);
    at(&clock, ms(300), move |_| flag.store(true, Ordering::SeqCst));
    let result = checkpoint_on(
        &fixture.permit,
        &messages(),
        &RetryPolicy::default(),
        &clock,
        &|| interrupted.load(Ordering::SeqCst),
    );
    lock.execute_batch("ROLLBACK").unwrap();
    assert!(
        matches!(
            result,
            Err(RegistryError::Interrupted {
                operation: DbOperation::Checkpoint,
                attempts: 1
            })
        ),
        "{result:?}"
    );
    // The interrupt waited for the attempt in progress, no longer.
    assert!(clock.elapsed() <= ms(2000), "{:?}", clock.elapsed());
    assert_eq!(owner_of(&fixture.path), before);
    assert_eq!(stored_payload(&fixture.path), None);
}

#[test]
fn an_interrupt_during_the_last_busy_wait_ends_as_the_interrupt() {
    let fixture = fixture();
    let before = owner_of(&fixture.path);
    let reader = fixture.permit.mutation_guard().expect("reader guard");
    let interrupted = Arc::new(AtomicBool::new(false));
    let clock = VirtualClock::new();
    let flag = Arc::clone(&interrupted);
    at(&clock, ms(1000), move |_| {
        flag.store(true, Ordering::SeqCst)
    });
    // The busy COMMIT also exhausts the whole operation budget.
    let policy = RetryPolicy {
        deadline: Some(ms(2000)),
        ..RetryPolicy::default()
    };
    let result = checkpoint_on(&fixture.permit, &messages(), &policy, &clock, &|| {
        interrupted.load(Ordering::SeqCst)
    });
    drop(reader);
    assert_eq!(clock.elapsed(), ms(2000));
    assert!(
        matches!(
            result,
            Err(RegistryError::Interrupted {
                operation: DbOperation::Checkpoint,
                attempts: 1
            })
        ),
        "{result:?}"
    );
    assert_eq!(owner_of(&fixture.path), before);
    assert_eq!(stored_payload(&fixture.path), None);
}

#[tokio::test(flavor = "current_thread")]
async fn async_begin_turn_waits_off_the_runtime_and_observes_interrupts() {
    let fixture = fixture();
    fixture
        .permit
        .checkpoint(&[])
        .expect("close the fixture turn");
    let before = owner_of(&fixture.path);
    let lock = exclusive_lock(&fixture.path);
    let (interrupt, interrupts) = tokio::sync::watch::channel(false);
    // Only a runtime that keeps running while the turn waits for the lock
    // ticks enough to send the interrupt. A blocked runtime would leave the
    // retry to its 10 s deadline and a database error instead.
    let ticker = tokio::spawn(async move {
        for _ in 0..5 {
            tokio::time::sleep(ms(10)).await;
        }
        interrupt.send_replace(true);
        std::future::pending::<()>().await;
    });
    let result = fixture
        .permit
        .begin_turn_off_thread(RetryPolicy::default(), move || *interrupts.borrow())
        .await;
    ticker.abort();
    lock.execute_batch("ROLLBACK").unwrap();
    assert!(
        matches!(
            result,
            Err(RegistryError::Interrupted {
                operation: DbOperation::BeginTurn,
                ..
            })
        ),
        "{result:?}"
    );
    assert_eq!(owner_of(&fixture.path), before);
}

#[test]
fn begin_turn_retries_a_busy_commit_without_opening_the_run_twice() {
    let fixture = fixture();
    fixture
        .permit
        .checkpoint(&[])
        .expect("close the fixture turn");
    let reader = fixture.permit.mutation_guard().expect("reader guard");
    let clock = VirtualClock::new();
    let mut reader = Some(reader);
    at(&clock, ms(2100), move |_| drop(reader.take()));
    let records = Records::default();
    tracing::dispatcher::with_default(&records.dispatch(), || {
        fixture
            .permit
            .begin_turn_on(&RetryPolicy::default(), &clock.shared(), &|| false)
    })
    .expect("begin succeeds after the reader releases");
    let failures = records
        .find(|record| field(record, "message") == Some("ownership registry operation failed"));
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(field(&failures[0], "operation"), Some("begin_turn"));
    assert_eq!(field(&failures[0], "phase"), Some("commit"));
    assert_eq!(
        attempts_recorded(
            &records,
            "ownership registry operation succeeded after contention"
        ),
        ["2"]
    );
    let owner = owner_of(&fixture.path);
    assert_eq!(owner.active_runs.iter().collect::<Vec<_>>(), vec![RUN]);
    assert!(owner.active_turn);
    assert!(matches!(
        fixture.permit.begin_turn(),
        Err(RegistryError::Ownership(OwnershipError::Active))
    ));
}

#[test]
fn a_repeated_checkpoint_after_a_retried_one_changes_nothing() {
    let fixture = fixture();
    let reader = fixture.permit.mutation_guard().expect("reader guard");
    let clock = VirtualClock::new();
    let mut reader = Some(reader);
    at(&clock, ms(2100), move |_| drop(reader.take()));
    checkpoint_on(
        &fixture.permit,
        &messages(),
        &RetryPolicy::default(),
        &clock,
        &|| false,
    )
    .expect("checkpoint");
    let owner = owner_of(&fixture.path);
    let payload = stored_payload(&fixture.path);
    fixture
        .permit
        .checkpoint(&[])
        .expect("an already checkpointed run is skipped");
    assert_eq!(owner_of(&fixture.path), owner);
    assert_eq!(stored_payload(&fixture.path), payload);
}

#[test]
fn update_callbacks_are_never_retried() {
    let fixture = fixture();
    // Held until the update has failed: one busy wait always expires.
    let reader = fixture.permit.mutation_guard().expect("reader guard");
    let calls = AtomicUsize::new(0);
    let result = Registry::open_existing(&fixture.path)
        .unwrap()
        .update("thread-1", |owner| {
            calls.fetch_add(1, Ordering::SeqCst);
            owner.lease.expires_at = u64::MAX - 1;
            Ok(())
        });
    drop(reader);
    let error = result.expect_err("the reader outlasts one busy wait");
    assert!(is_busy(&error), "{error:?}");
    assert!(matches!(
        error,
        RegistryError::Database {
            operation: DbOperation::Update,
            phase: DbPhase::Commit,
            attempt: 1,
            ..
        }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(owner_of(&fixture.path).lease.expires_at, u64::MAX);
}

#[test]
fn child_preparation_is_never_retried() {
    let fixture = fixture();
    let lock = exclusive_lock(&fixture.path);
    let calls = AtomicUsize::new(0);
    let records = Records::default();
    let result = tracing::dispatcher::with_default(&records.dispatch(), || {
        fixture.permit.prepare_child("child", || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    lock.execute_batch("ROLLBACK").unwrap();
    let error = result.expect_err("an exclusive lock blocks child creation");
    assert!(error.contains("ownership child failed at begin"), "{error}");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let failures = records
        .find(|record| field(record, "message") == Some("ownership registry operation failed"));
    assert_eq!(failures.len(), 1, "a single attempt: {failures:?}");
}

#[test]
fn generation_preflight_retries_exclusive_contention_with_the_attempt_budget() {
    let fixture = fixture();
    let before = owner_of(&fixture.path);
    let lock = exclusive_lock(&fixture.path);
    let clock = VirtualClock::new();
    let records = Records::default();
    // The first 2 s attempt must fail; the second can read the same owner.
    at(&clock, ms(2100), move |_| {
        lock.execute_batch("ROLLBACK").unwrap();
    });
    tracing::dispatcher::with_default(&records.dispatch(), || {
        fixture
            .permit
            .validate_generation_on(&RetryPolicy::default(), &clock.shared(), &|| false)
    })
    .expect("a transient lock is not fencing");
    assert!(clock.elapsed() >= ms(2100));
    assert!(clock.elapsed() < ms(10_000));
    assert_eq!(
        attempts_recorded(
            &records,
            "ownership registry operation succeeded after contention"
        ),
        ["2"]
    );
    assert_eq!(owner_of(&fixture.path), before);
    assert_eq!(stored_payload(&fixture.path), None);
    // No guard may survive the preflight (even a write can start now).
    drop(exclusive_lock(&fixture.path));
}

#[test]
fn generation_preflight_exclusive_contention_expires_at_the_injected_deadline() {
    let fixture = fixture();
    let before = owner_of(&fixture.path);
    let lock = exclusive_lock(&fixture.path);
    let clock = VirtualClock::new();
    let policy = RetryPolicy {
        deadline: Some(ms(350)),
        attempt_timeout: ms(100),
        initial_backoff: ms(10),
        max_backoff: ms(10),
    };
    let error = fixture
        .permit
        .validate_generation_on(&policy, &clock.shared(), &|| false)
        .expect_err("a lock past the deadline is still contention, not fencing");
    assert!(is_busy(&error), "{error:?}");
    assert!(
        matches!(
            error,
            RegistryError::Database {
                operation: DbOperation::Guard,
                attempt: 3..,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(Some(clock.elapsed()), policy.deadline);
    drop(lock);
    assert_eq!(owner_of(&fixture.path), before);
    assert_eq!(stored_payload(&fixture.path), None);
}

#[test]
fn generation_preflight_observes_interrupt_after_a_busy_attempt() {
    let fixture = fixture();
    let lock = exclusive_lock(&fixture.path);
    let clock = VirtualClock::new();
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal = interrupted.clone();
    at(&clock, ms(50), move |_| {
        signal.store(true, Ordering::SeqCst)
    });
    let error = fixture
        .permit
        .validate_generation_on(
            &RetryPolicy {
                attempt_timeout: ms(100),
                ..RetryPolicy::default()
            },
            &clock.shared(),
            &|| interrupted.load(Ordering::SeqCst),
        )
        .unwrap_err();
    assert!(
        matches!(
            error,
            RegistryError::Interrupted {
                operation: DbOperation::Guard,
                attempts: 1
            }
        ),
        "{error:?}"
    );
    assert_eq!(clock.elapsed(), ms(100));
    drop(lock);
    assert!(owner_of(&fixture.path).active_runs.contains(RUN));
}

#[test]
fn generation_preflight_rechecks_fencing_and_released_after_contention() {
    for released in [false, true] {
        let fixture = fixture();
        let before = owner_of(&fixture.path);
        let lock = exclusive_lock(&fixture.path);
        let mut next = before.clone();
        if released {
            next.state = OwnerState::Released;
        } else {
            next.lease.generation += 1;
        }
        let committed = next.clone();
        let clock = VirtualClock::new();
        at(&clock, ms(2100), move |_| {
            lock.execute(
                "UPDATE thread_owners SET state=?1 WHERE thread_id='thread-1'",
                [serde_json::to_string(&committed).unwrap()],
            )
            .unwrap();
            lock.execute_batch("COMMIT").unwrap();
        });
        let error = fixture
            .permit
            .validate_generation_on(&RetryPolicy::default(), &clock.shared(), &|| false)
            .unwrap_err();
        assert!(
            matches!(error, RegistryError::Ownership(OwnershipError::Fenced)),
            "{error:?}"
        );
        assert!(clock.elapsed() >= ms(2100));
        assert!(clock.elapsed() < ms(10_000));
        assert_eq!(owner_of(&fixture.path), next);
        assert_eq!(stored_payload(&fixture.path), None);
    }
}
