use super::{background::*, build::*};
use crate::{Database, Storage, StorageConfig};
use event_bus::{Event, MessageEvent};

struct At(i64);
impl Clock for At {
    fn now_ns(&self) -> i64 {
        self.0
    }
}

fn append(writer: &crate::StorageHandle, text: &str) {
    writer
        .append_event(
            None,
            &Event::new(MessageEvent::MessageCompleted {
                run_id: "run".into(),
                text: text.into(),
            }),
        )
        .unwrap();
}
fn phase(db: &Database) -> i64 {
    db.conn
        .query_row(
            "SELECT phase FROM history_run_state WHERE run_id='run'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}
fn complete(driver: &mut Driver, db: &Database, writer: &crate::StorageHandle, at: i64) {
    for _ in 0..128 {
        let tick = driver
            .tick(db, writer, BuildBudget::default(), &At(at))
            .unwrap();
        if tick.progress.ready {
            return;
        }
    }
    panic!("finite fixture must become ready");
}

#[test]
fn discovery_debounce_checkpoints_reopen_and_ordinary_commands_are_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("history.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let writer = storage.handle();
    for _ in 0..20 {
        append(&writer, "answer");
    }
    let db = Database::open(&config).unwrap();
    db.conn
        .execute("DELETE FROM history_run_state", [])
        .unwrap();
    let mut driver = Driver::default();
    let budget = BuildBudget {
        rows: 8,
        writes: 16,
        ..Default::default()
    };
    for _ in 0..8 {
        let tick = driver.tick(&db, &writer, budget, &At(0)).unwrap();
        assert!(tick.progress.rows + tick.discovery_rows <= budget.rows);
        assert!(tick.progress.writes + tick.discovery_writes <= budget.writes);
        assert!(tick.progress.bytes + tick.discovery_bytes <= budget.bytes);
    }
    assert_eq!(phase(&db), 0);
    for _ in 0..5 {
        driver.tick(&db, &writer, budget, &At(250_000_000)).unwrap();
    }
    assert_eq!(phase(&db), 1);
    let checkpoint: i64 = db
        .conn
        .query_row("SELECT checkpoint FROM history_run_state", [], |r| r.get(0))
        .unwrap();
    assert!(checkpoint > 0);
    drop(db);
    drop(writer);
    storage.close();
    let storage = Storage::open(config.clone()).unwrap();
    let writer = storage.handle();
    let db = Database::open(&config).unwrap();
    let same: i64 = db
        .conn
        .query_row("SELECT checkpoint FROM history_run_state", [], |r| r.get(0))
        .unwrap();
    assert_eq!(same, checkpoint);
    append(&writer, "tail while building");
    complete(&mut Driver::default(), &db, &writer, 1_000_000_000);
    assert_eq!(phase(&db), 2);
    assert_eq!(
        db.conn
            .query_row::<i64, _, _>("SELECT COUNT(*) FROM history_messages", [], |r| r.get(0))
            .unwrap(),
        21
    );
    let mut owner = Owner::start(config, writer.clone()).unwrap();
    owner.stop();
    // Joining the history reader must not shut down the ordinary storage writer.
    append(&writer, "after owner stop");
}

#[test]
fn destructive_change_resets_debounce_and_unreadable_rows_back_off_without_skipping() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("history.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let writer = storage.handle();
    append(&writer, "answer");
    let db = Database::open(&config).unwrap();
    let original: String = db
        .conn
        .query_row("SELECT payload FROM events", [], |r| r.get(0))
        .unwrap();
    db.conn
        .execute("UPDATE events SET payload='broken'", [])
        .unwrap();
    let mut driver = Driver::default();
    driver
        .tick(&db, &writer, BuildBudget::default(), &At(0))
        .unwrap();
    assert!(
        driver
            .tick(&db, &writer, BuildBudget::default(), &At(250_000_000))
            .is_err()
    );
    assert_ne!(phase(&db), 2);
    let meter = crate::test_support::meter_history(&db);
    for _ in 0..4 {
        driver
            .tick(&db, &writer, BuildBudget::default(), &At(1_000_000_000))
            .unwrap();
    }
    assert_eq!(meter.counts().payload_decodes, 0);
    db.conn
        .execute("UPDATE events SET payload=?1", [original])
        .unwrap();
    driver
        .tick(&db, &writer, BuildBudget::default(), &At(1_000_000_000))
        .unwrap();
    assert_eq!(phase(&db), 0);
    complete(&mut driver, &db, &writer, 1_250_000_000);
    assert_eq!(phase(&db), 2);
}

#[test]
fn competing_preparations_and_hook_free_tail_compare_durable_source_and_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("history.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let writer = storage.handle();
    append(&writer, "first");
    append(&writer, "second");
    let reader = Database::open_read_only(&config.db_path).unwrap();
    let peer = Database::open(&config).unwrap();
    peer.conn
        .execute("UPDATE events SET wall_clock_ns=wall_clock_ns+1", [])
        .unwrap();
    let budget = BuildBudget::default();
    let start = writer.history_start("run", budget).unwrap();
    let a = super::build::prepare(&reader, "run", budget, &At(0), start)
        .unwrap()
        .unwrap();
    let b = super::build::prepare(&peer, "run", budget, &At(0), start)
        .unwrap()
        .unwrap();
    assert!(!writer.history_apply(a).unwrap().conflicted);
    assert!(writer.history_apply(b).unwrap().conflicted);
    let start = writer.history_start("run", budget).unwrap();
    let stale = super::build::prepare(&reader, "run", budget, &At(0), start)
        .unwrap()
        .unwrap();
    append(&writer, "concurrent tail");
    assert!(writer.history_apply(stale).unwrap().conflicted);
    let mut driver = Driver::default();
    complete(&mut driver, &reader, &writer, 1_000_000_000);
    assert_eq!(phase(&peer), 2);
    assert_eq!(
        peer.conn
            .query_row::<i64, _, _>("SELECT COUNT(*) FROM history_messages", [], |r| r.get(0))
            .unwrap(),
        3
    );
}

#[test]
fn reused_scanned_ids_rewind_even_without_a_cut_and_reject_prepared_ready() {
    for committed_prefix in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("history.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let writer = storage.handle();
        if committed_prefix {
            append(&writer, "first");
        }
        writer
            .append_event(
                None,
                &Event::new(MessageEvent::MessageDelta {
                    run_id: Some("run".into()),
                    delta: "not committed".into(),
                }),
            )
            .unwrap();
        let peer = Database::open(&config).unwrap();
        let reader = Database::open_read_only(&config.db_path).unwrap();
        peer.conn
            .execute("UPDATE history_run_state SET phase=0", [])
            .unwrap();
        let budget = BuildBudget {
            rows: 1,
            ..Default::default()
        };
        for _ in 0..if committed_prefix { 2 } else { 1 } {
            assert!(
                !peer
                    .history_build_step("run", budget, &At(0))
                    .unwrap()
                    .ready
            );
        }
        let before = super::index::state(&peer.conn, "run").unwrap().unwrap();
        let tail = before.checkpoint.unwrap();
        assert_eq!(before.phase, 1);
        assert_eq!(before.last_indexed, committed_prefix.then_some(tail - 1));
        let start = writer.history_start("run", BuildBudget::default()).unwrap();
        let stale = super::build::prepare(&reader, "run", BuildBudget::default(), &At(0), start)
            .unwrap()
            .unwrap();
        peer.conn
            .execute("DELETE FROM events WHERE id=?1", [tail])
            .unwrap();
        append(&writer, "reused");
        let after = super::index::state(&peer.conn, "run").unwrap().unwrap();
        assert_eq!(after.generation, before.generation);
        assert_eq!(after.checkpoint, before.last_indexed);
        assert!(writer.history_apply(stale).unwrap().conflicted);
        assert_eq!(phase(&peer), 1);
        let mut driver = Driver::default();
        complete(&mut driver, &reader, &writer, 1_000_000_000);
        assert_eq!(
            peer.conn
                .query_row::<i64, _, _>(
                    "SELECT COUNT(*) FROM history_messages WHERE run_id='run'",
                    [],
                    |r| r.get(0),
                )
                .unwrap(),
            if committed_prefix { 2 } else { 1 },
        );
        let ready = super::index::state(&peer.conn, "run").unwrap().unwrap();
        assert_eq!(ready.source, ready.applied);
        assert_eq!(ready.last_indexed, Some(tail));
        storage.close();
    }
}

#[test]
fn compressed_source_and_completed_parts_obey_decode_and_source_budgets() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("history.db"),
        ..Default::default()
    };
    let db = Database::open(&config).unwrap();
    let event = serde_json::to_vec(&event_bus::EventKind::from(
        MessageEvent::MessageCompleted {
            run_id: "run".into(),
            text: "a".repeat(32 * 1024),
        },
    ))
    .unwrap();
    let compressed = zstd::stream::encode_all(event.as_slice(), 1).unwrap();
    assert!(compressed.len() < 1024);
    db.conn.execute("INSERT INTO events(schema_version,session_id,run_id,kind,subkind,wall_clock_ns,monotonic_ns,payload) VALUES (1,NULL,'run','Message','MessageCompleted',0,0,?1)", [compressed]).unwrap();
    let budget = BuildBudget {
        bytes: 1024,
        ..Default::default()
    };
    assert!(db.history_build_step("run", budget, &At(0)).is_err());
    assert_ne!(phase(&db), 2);
    for _ in 0..4 {
        if db
            .history_build_step("run", BuildBudget::default(), &At(0))
            .unwrap()
            .ready
        {
            break;
        }
    }
    assert_eq!(phase(&db), 2);
    for index in 0..3 {
        let kind = event_bus::EventKind::from(MessageEvent::MessagePart {
            run_id: "run".into(),
            response_id: "response".into(),
            index,
            total: 3,
            text: "part".into(),
        });
        db.conn.execute("INSERT INTO events(schema_version,run_id,kind,subkind,wall_clock_ns,monotonic_ns,payload) VALUES (1,'run','Message','MessagePart',0,0,?1)", [serde_json::to_string(&kind).unwrap()]).unwrap();
    }
    let small = BuildBudget {
        rows: 1,
        ..Default::default()
    };
    for _ in 0..2 {
        db.history_build_step("run", small, &At(0)).unwrap();
    }
    let meter = crate::test_support::meter_history(&db);
    assert!(db.history_build_step("run", small, &At(0)).is_err());
    assert_eq!(meter.counts().payload_decodes, 1);
    assert_ne!(phase(&db), 2);
    for _ in 0..4 {
        if db
            .history_build_step("run", BuildBudget::default(), &At(0))
            .unwrap()
            .ready
        {
            break;
        }
    }
    assert_eq!(phase(&db), 2);
}
