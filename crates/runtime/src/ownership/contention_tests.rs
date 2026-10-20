//! Contention, retry and observability tests for ownership registry writes.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::layer::SubscriberExt;

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
    assert_eq!(
        field(record, "payload_bytes"),
        Some(
            serde_json::to_string(&messages)
                .unwrap()
                .len()
                .to_string()
                .as_str()
        )
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
