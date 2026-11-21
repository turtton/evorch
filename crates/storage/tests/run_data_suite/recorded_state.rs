//! `recorded_state` of `inspect run`: the newest phase event of a run, read from the log
//! through a fresh connection.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use event_bus::{
    AgentRunPhase, Event, EventKind, LifecycleEvent, WaitingCause, WaitingMode, WaitingRelation,
    WaitingRun,
};
use rusqlite::Connection;
use storage::{Database, Storage, StorageConfig};
use tempfile::TempDir;

fn open(dir: &TempDir) -> (Storage, StorageConfig) {
    let config = StorageConfig {
        db_path: dir.path().join("recorded-state.db"),
        ..StorageConfig::default()
    };
    (Storage::open(config.clone()).expect("storage"), config)
}

fn changed(
    run: &str,
    from: AgentRunPhase,
    to: AgentRunPhase,
    waiting: Option<WaitingCause>,
) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from,
        to,
        reason: None,
        waiting,
    })
}

fn started(run: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: None,
        agent_name: "worker".into(),
        role: "worker".into(),
    })
}

fn wait_tool() -> WaitingCause {
    WaitingCause::WaitTool {
        call_id: Some("call-1".into()),
        explicit: true,
        mode: WaitingMode::All,
        runs: vec![WaitingRun {
            run_id: "run-2".into(),
            relation: WaitingRelation::Child,
        }],
    }
}

fn wall_clock_ns(event: &Event) -> i64 {
    i64::try_from(
        event
            .meta
            .wall_clock
            .duration_since(UNIX_EPOCH)
            .expect("after epoch")
            .as_nanos(),
    )
    .expect("fits")
}

fn recorded(config: &StorageConfig, run: &str) -> serde_json::Value {
    Database::open_read_only(&config.db_path)
        .expect("read-only open")
        .inspect_recorded_state(run)
        .expect("recorded state")
}

#[test]
fn a_waiting_cause_is_read_back_with_the_event_time_after_reopening() {
    // Given: a run that entered Waiting for a typed cause, then the database was closed.
    let dir = TempDir::new().unwrap();
    let (storage, config) = open(&dir);
    let handle = storage.handle();
    let mut event = changed(
        "run-1",
        AgentRunPhase::Running,
        AgentRunPhase::Waiting,
        Some(wait_tool()),
    );
    event.meta.wall_clock = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    handle
        .append_event(
            Some("s"),
            &changed(
                "run-1",
                AgentRunPhase::Pending,
                AgentRunPhase::Running,
                None,
            ),
        )
        .unwrap();
    handle.append_event(Some("s"), &event).unwrap();
    drop(storage);

    // When: it is read through a new connection.
    let state = recorded(&config, "run-1");

    // Then: the cause is the typed one, the string reason stays empty, and the time is
    // the event's own wall clock.
    assert_eq!(state["event"], "AgentRunStateChanged");
    assert_eq!(state["phase"], "Waiting");
    assert!(state["reason"].is_null());
    assert_eq!(
        serde_json::from_value::<WaitingCause>(state["waiting"].clone()).unwrap(),
        wait_tool()
    );
    assert_eq!(state["recorded_at_ns"], wall_clock_ns(&event));
    assert!(state.get("waiting_unrecorded").is_none());
}

#[test]
fn a_later_phase_or_registration_replaces_the_earlier_waiting_cause() {
    // Given: a run that waited, resumed, waited again and was registered again.
    let dir = TempDir::new().unwrap();
    let (storage, config) = open(&dir);
    let handle = storage.handle();
    let append = |event: Event| handle.append_event(Some("s"), &event).unwrap();
    append(changed(
        "run-1",
        AgentRunPhase::Pending,
        AgentRunPhase::Running,
        None,
    ));
    append(changed(
        "run-1",
        AgentRunPhase::Running,
        AgentRunPhase::Waiting,
        Some(WaitingCause::TurnCompleted),
    ));
    // Another run's state never leaks into this one.
    append(changed(
        "run-2",
        AgentRunPhase::Pending,
        AgentRunPhase::Running,
        None,
    ));

    // When/Then: Running clears the cause.
    append(changed(
        "run-1",
        AgentRunPhase::Waiting,
        AgentRunPhase::Running,
        None,
    ));
    let running = recorded(&config, "run-1");
    assert_eq!(running["phase"], "Running");
    assert!(running["waiting"].is_null());

    // When/Then: the newer cause replaces the older one.
    append(changed(
        "run-1",
        AgentRunPhase::Running,
        AgentRunPhase::Waiting,
        Some(WaitingCause::BlockingQuestions),
    ));
    assert_eq!(
        recorded(&config, "run-1")["waiting"]["kind"],
        "blocking_questions"
    );

    // When/Then: a registration after the wait (a restart) leaves no phase and no cause.
    append(started("run-1"));
    let restarted = recorded(&config, "run-1");
    assert_eq!(restarted["event"], "AgentRunStarted");
    assert!(restarted["phase"].is_null());
    assert!(restarted["waiting"].is_null());
    assert_eq!(recorded(&config, "run-2")["phase"], "Running");
    assert!(recorded(&config, "run-3").is_null());
}

#[test]
fn a_waiting_row_without_a_cause_is_reported_as_unrecorded() {
    // Given: a Waiting row written before causes existed (no `waiting` field).
    let dir = TempDir::new().unwrap();
    let (storage, config) = open(&dir);
    storage
        .handle()
        .append_event(
            Some("s"),
            &changed(
                "run-1",
                AgentRunPhase::Running,
                AgentRunPhase::Waiting,
                None,
            ),
        )
        .unwrap();

    // When/Then: nothing is inferred for it.
    let state = recorded(&config, "run-1");
    assert_eq!(state["phase"], "Waiting");
    assert!(state["waiting"].is_null());
    assert_eq!(state["waiting_unrecorded"], true);
}

#[test]
fn the_newest_state_is_found_after_more_events_than_the_listing_bound() {
    // Given: a Waiting then a Running, then more than the 10,000 events the run listing
    // shows.
    let dir = TempDir::new().unwrap();
    let (storage, config) = open(&dir);
    let handle = storage.handle();
    handle
        .append_event(
            Some("s"),
            &changed(
                "run-1",
                AgentRunPhase::Running,
                AgentRunPhase::Waiting,
                Some(WaitingCause::InputBoundary),
            ),
        )
        .unwrap();
    handle
        .append_event(
            Some("s"),
            &changed(
                "run-1",
                AgentRunPhase::Waiting,
                AgentRunPhase::Running,
                None,
            ),
        )
        .unwrap();
    drop(storage);
    let turn = serde_json::to_string(&EventKind::Lifecycle(LifecycleEvent::TurnCompleted {
        run_id: "run-1".into(),
        context_len: 1,
    }))
    .unwrap();
    Connection::open(&config.db_path)
        .unwrap()
        .execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 10001) \
             INSERT INTO events \
             (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind, run_id) \
             SELECT 's', ?1, 0, 0, 'Lifecycle', ?2, 'TurnCompleted', 'run-1' FROM n",
            rusqlite::params![event_bus::SCHEMA_VERSION, turn],
        )
        .unwrap();

    // When: the state is read.
    let state = recorded(&config, "run-1");

    // Then: the Running that cleared the Waiting is the newest phase event, not the
    // older Waiting that the bounded listing would reach first.
    assert_eq!(state["phase"], "Running");
    assert!(state["waiting"].is_null());
}

#[test]
fn an_unreadable_newest_row_is_an_error_not_an_older_waiting() {
    // Given: a Waiting followed by a phase row whose payload no longer decodes.
    let dir = TempDir::new().unwrap();
    let (storage, config) = open(&dir);
    storage
        .handle()
        .append_event(
            Some("s"),
            &changed(
                "run-1",
                AgentRunPhase::Running,
                AgentRunPhase::Waiting,
                Some(WaitingCause::TurnCompleted),
            ),
        )
        .unwrap();
    drop(storage);
    let broken = r#"{"kind":"Lifecycle","payload":{"kind":"AgentRunStateChanged","payload":{"run_id":"run-1"}}}"#;
    Connection::open(&config.db_path)
        .unwrap()
        .execute(
            "INSERT INTO events \
             (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind, run_id) \
             VALUES ('s', ?1, 0, 0, 'Lifecycle', ?2, 'AgentRunStateChanged', 'run-1')",
            rusqlite::params![event_bus::SCHEMA_VERSION, broken],
        )
        .unwrap();

    // When: the state is read.
    let result = Database::open_read_only(&config.db_path)
        .unwrap()
        .inspect_recorded_state("run-1");

    // Then: the older Waiting is not reported as current.
    assert!(result.is_err(), "{result:?}");
}

#[test]
fn inspect_run_carries_the_recorded_state_in_every_detail() {
    // Given: a waiting run.
    let dir = TempDir::new().unwrap();
    let (storage, config) = open(&dir);
    storage
        .handle()
        .append_event(
            Some("s"),
            &changed(
                "run-1",
                AgentRunPhase::Running,
                AgentRunPhase::Waiting,
                Some(wait_tool()),
            ),
        )
        .unwrap();
    drop(storage);
    let database = Database::open_read_only(&config.db_path).unwrap();

    // When/Then: summary and full both show the same recorded state.
    let summary = database.inspect_run("run-1", false).unwrap();
    let full = database.inspect_run("run-1", true).unwrap();
    assert_eq!(summary["recorded_state"]["phase"], "Waiting");
    assert_eq!(summary["recorded_state"], full["recorded_state"]);
    assert_eq!(summary["recorded_state"]["waiting"]["kind"], "wait_tool");
}
