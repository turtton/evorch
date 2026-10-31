use std::fs;

use event_bus::{Event, EventKind, MessageEvent};
use rusqlite::params;

use super::*;
use crate::{StorageConfig, codec};

fn seed(path: &Path) -> Database {
    let sidebar = path.parent().unwrap().join("sidebar.json");
    if !sidebar.exists() {
        fs::write(sidebar, br#"{"user_messages":[]}"#).unwrap();
    }
    let db = Database::open(&StorageConfig {
        db_path: path.into(),
        ..Default::default()
    })
    .unwrap();
    drop(offline::writer_lease(path).unwrap());
    db
}

#[test]
fn unavailable_sidebar_disables_aggregation_but_not_other_compaction_stages() {
    for saved in [
        None,
        Some("not json"),
        Some("[]"),
        Some(r#"{"user_messages":[{}]}"#),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        let db = seed(&path);
        insert(&db, delta("a", "one "), false);
        insert(&db, delta("a", "two"), false);
        drop(db);
        let sidebar = dir.path().join("sidebar.json");
        if let Some(saved) = saved {
            fs::write(&sidebar, saved).unwrap();
        } else {
            fs::remove_file(&sidebar).unwrap();
        }
        let source = fs::read(&path).unwrap();
        let preview = compact_events(&path, &options()).unwrap();
        assert!(!preview.delta_aggregation_enabled);
        assert_eq!(preview.warnings.len(), 1);
        assert_eq!(source, fs::read(&path).unwrap());
        let result = compact_events(
            &path,
            &CompactOptions {
                apply: true,
                ..options()
            },
        )
        .unwrap();
        assert_eq!(
            result.reasoning_deltas.input_rows,
            result.reasoning_deltas.output_rows
        );
        assert_eq!(preview.projected_event_rows, result.projected_event_rows);
        assert_eq!(result.integrity_check.as_deref(), Some("ok"));
    }
}

fn insert(db: &Database, event: Event, legacy: bool) {
    let kind = serde_json::to_value(&event.kind).unwrap();
    db.conn.execute(
        "INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,payload,subkind,run_id)
         VALUES(1,0,0,?1,?2,?3,?4)",
        params![kind["kind"].as_str().unwrap(), serde_json::to_string(&event.kind).unwrap(),
            (!legacy).then(|| event.kind.subkind_name()), event.primary_run_id()],
    ).unwrap();
}

fn delta(run: &str, text: &str) -> Event {
    Event::new(MessageEvent::ReasoningDelta {
        run_id: Some(run.into()),
        delta: text.into(),
    })
}

fn options() -> CompactOptions {
    CompactOptions {
        batch_rows: 2,
        provider_retention_days: 0,
        ..Default::default()
    }
}

#[test]
fn preview_is_read_only_and_counts_match_restartable_idempotent_apply() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = seed(&path);
    for i in 0..30 {
        insert(
            &db,
            delta(if i % 2 == 0 { "a" } else { "b" }, "考え\\\n"),
            true,
        );
    }
    db.conn.execute("INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,payload) VALUES(1,0,0,'Message','invalid legacy')", []).unwrap();
    drop(db);
    let before = fs::read(&path).unwrap();
    let preview = compact_events(&path, &options()).unwrap();
    assert_eq!(before, fs::read(&path).unwrap());
    assert_eq!(preview.backfill.remaining, 31);
    assert_eq!(preview.reasoning_deltas.input_rows, 30);
    assert_eq!(preview.reasoning_deltas.output_rows, 2);

    // Interrupt after a bounded backfill call, then let apply finish.
    let progress = backfill_events(
        &path,
        &EventBackfillOptions {
            batch_rows: 2,
            max_rows: Some(3),
        },
    )
    .unwrap();
    assert_eq!(
        (progress.converted, progress.batches, progress.remaining),
        (3, 2, 28)
    );
    let apply = compact_events(
        &path,
        &CompactOptions {
            apply: true,
            ..options()
        },
    )
    .unwrap();
    assert_eq!(apply.integrity_check.as_deref(), Some("ok"));
    assert_eq!(apply.backfill.undecodable, 1);
    assert!(apply.vacuum && apply.checkpoint_truncated);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(apply.backup.as_ref().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let rows = |snapshot: &Snapshot| {
        snapshot
            .events
            .iter()
            .map(|(k, v)| (k.clone(), v.rows))
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(preview.projected_event_rows, rows(&apply.after));
    let db = seed(&path);
    let logical = crate::repo::event::list_all_ordered(&db.conn).unwrap();
    drop(db);
    let second = compact_events(
        &path,
        &CompactOptions {
            apply: true,
            ..options()
        },
    )
    .unwrap();
    assert_eq!(
        second.reasoning_deltas.input_rows,
        second.reasoning_deltas.output_rows
    );
    assert_eq!(second.compressed_event_rows, 0);
    let db = seed(&path);
    assert_eq!(
        logical,
        crate::repo::event::list_all_ordered(&db.conn).unwrap()
    );
}

use std::collections::BTreeMap;

#[test]
fn escaped_utf8_chunks_reuse_first_ids_and_existing_oversize_row_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = seed(&path);
    let piece = "\"\\\n🙂".repeat(1200);
    for _ in 0..20 {
        insert(&db, delta("a", &piece), false);
    }
    let oversize = delta("huge", &"🦀".repeat(20_000));
    insert(&db, oversize.clone(), false);
    let before = crate::repo::event::list_all_ordered(&db.conn).unwrap();
    drop(db);
    let applied = compact_events(
        &path,
        &CompactOptions {
            apply: true,
            ..options()
        },
    )
    .unwrap();
    assert_eq!(applied.oversized_rows, 1);
    let db = seed(&path);
    let after = crate::repo::event::list_all_ordered(&db.conn).unwrap();
    let mut text = String::new();
    for stored in &after {
        if let EventKind::Message(MessageEvent::ReasoningDelta { run_id, delta }) =
            &stored.event.kind
        {
            if run_id.as_deref() == Some("huge") {
                assert_eq!(stored.event.kind, oversize.kind);
            } else {
                assert!(serde_json::to_vec(&stored.event.kind).unwrap().len() <= 64 * 1024);
                text.push_str(delta);
            }
        }
        assert_eq!(
            stored.event.meta,
            before
                .iter()
                .find(|old| old.id == stored.id)
                .unwrap()
                .event
                .meta
        );
    }
    assert_eq!(text, piece.repeat(20));
}

#[test]
fn failed_group_rolls_back_but_prior_groups_survive_and_retry_finishes() {
    let db = Database::open_in_memory().unwrap();
    db.conn
        .execute(
            "INSERT INTO sessions(id,status,created_at_ns,updated_at_ns) VALUES('s','running',0,0)",
            [],
        )
        .unwrap();
    for run in ["a", "b"] {
        insert(&db, delta(run, "first"), false);
        insert(&db, delta(run, "second"), false);
        insert(
            &db,
            Event::new(MessageEvent::MessageCompleted {
                run_id: run.into(),
                text: "answer".into(),
            }),
            false,
        );
    }
    db.conn
        .execute("UPDATE events SET session_id='s'", [])
        .unwrap();
    recalculate_accounting(&db.conn).unwrap();
    db.conn
        .execute_batch(
            "CREATE TRIGGER injected_failure BEFORE UPDATE OF payload ON events
         WHEN OLD.run_id='b' BEGIN SELECT RAISE(ABORT,'injected interruption'); END;",
        )
        .unwrap();
    let options = CompactOptions {
        apply: true,
        ..options()
    };
    assert!(deltas::compact(&db.conn, &options, true, None).is_err());
    let count = |run| {
        db.conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE run_id=?1 AND subkind='ReasoningDelta'",
                [run],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    assert_eq!((count("a"), count("b")), (1, 2));
    assert_accounting(&db.conn);
    db.conn
        .execute_batch("DROP TRIGGER injected_failure")
        .unwrap();
    deltas::compact(&db.conn, &options, true, None).unwrap();
    assert_eq!((count("a"), count("b")), (1, 1));
    assert_accounting(&db.conn);
}

fn assert_accounting(conn: &Connection) {
    for (actual, expected) in [
        (
            "SELECT total_event_bytes FROM sessions WHERE id='s'",
            "SELECT COALESCE(SUM(OCTET_LENGTH(payload)),0) FROM events WHERE session_id='s'",
        ),
        (
            "SELECT COALESCE(SUM(payload_bytes),0) FROM event_session_bytes",
            "SELECT COALESCE(SUM(OCTET_LENGTH(payload)),0) FROM events WHERE session_id IS NOT NULL",
        ),
        (
            "SELECT COALESCE(SUM(payload_bytes),0) FROM event_day_bytes",
            "SELECT COALESCE(SUM(OCTET_LENGTH(payload)),0) FROM events",
        ),
    ] {
        let read = |sql| conn.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
        assert_eq!(read(actual), read(expected), "{actual}");
    }
}

#[test]
fn failed_later_batch_keeps_all_accounting_consistent_for_each_rewrite() {
    for action in ["noise", "provider", "delete", "compress"] {
        let db = Database::open_in_memory().unwrap();
        db.conn.execute("INSERT INTO sessions(id,status,created_at_ns,updated_at_ns) VALUES('s','running',0,0)", []).unwrap();
        for _ in 0..2 {
            insert(&db, delta("a", &"body ".repeat(1000)), false);
        }
        db.conn
            .execute("UPDATE events SET session_id='s'", [])
            .unwrap();
        if let Some((kind, subkind)) = match action {
            "noise" => Some(("Fault", "SubscriberLagged")),
            "provider" => Some(("Provider", "RequestCompleted")),
            "compress" => Some(("Message", "MessageCompleted")),
            _ => None,
        } {
            db.conn
                .execute(
                    "UPDATE events SET kind=?1,subkind=?2",
                    params![kind, subkind],
                )
                .unwrap();
        }
        recalculate_accounting(&db.conn).unwrap();
        let operation = if action == "compress" {
            "UPDATE OF payload"
        } else {
            "DELETE"
        };
        db.conn
            .execute_batch(&format!(
                "CREATE TRIGGER injected_failure BEFORE {operation} ON events
             WHEN OLD.id=2 BEGIN SELECT RAISE(ABORT,'injected interruption'); END;"
            ))
            .unwrap();
        let options = CompactOptions {
            apply: true,
            batch_rows: 1,
            delta_mode: DeltaMode::Delete,
            ..options()
        };
        let apply = || match action {
            "noise" => delete_matching(&db.conn, &options, None).map(|_| ()),
            "provider" => delete_matching(&db.conn, &options, Some(1)).map(|_| ()),
            "delete" => deltas::compact(&db.conn, &options, true, None).map(|_| ()),
            "compress" => statistics::compress_events(&db.conn, &options, None).map(|_| ()),
            _ => unreachable!(),
        };
        assert!(apply().is_err(), "{action}");
        assert_accounting(&db.conn);
        let committed: bool = db
            .conn
            .query_row(
                if action == "compress" {
                    "SELECT typeof(payload)='blob' FROM events WHERE id=1"
                } else {
                    "SELECT NOT EXISTS(SELECT 1 FROM events WHERE id=1)"
                },
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(committed, "{action}");
        db.conn
            .execute_batch("DROP TRIGGER injected_failure")
            .unwrap();
        apply().unwrap();
        assert_accounting(&db.conn);
    }
}

#[test]
fn preview_samples_surviving_aggregates_that_cross_the_compression_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = seed(&path);
    for reasoning in [false, true] {
        for _ in 0..5 {
            let body = "考え".repeat(100);
            let event = if reasoning {
                delta("a", &body)
            } else {
                Event::new(MessageEvent::MessageDelta {
                    run_id: Some("a".into()),
                    delta: body,
                })
            };
            assert!(
                serde_json::to_string(&event.kind).unwrap().len() < codec::MIN_COMPRESSED_BYTES
            );
            insert(&db, event, true);
        }
    }
    drop(db);
    let source = fs::read(&path).unwrap();
    let preview = compact_events(&path, &options()).unwrap();
    assert_eq!(preview.event_compression.eligible_values, 2);
    assert_eq!(preview.event_compression.sampled_values, 2);
    assert_eq!(preview.event_compression.estimated_compressed_values, 2);
    assert_eq!(source, fs::read(&path).unwrap());
    let result = compact_events(
        &path,
        &CompactOptions {
            apply: true,
            ..options()
        },
    )
    .unwrap();
    assert_eq!(result.compressed_event_rows, 2);
    assert_eq!(
        serde_json::to_value(preview.event_compression).unwrap(),
        serde_json::to_value(result.event_compression).unwrap(),
    );
}

#[test]
fn preview_delete_mode_does_not_sample_deltas_that_will_be_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = seed(&path);
    let body = "large content ".repeat(1000);
    for event in [
        delta("a", &body),
        Event::new(MessageEvent::MessageDelta {
            run_id: Some("a".into()),
            delta: body.clone(),
        }),
        Event::new(MessageEvent::MessageCompleted {
            run_id: "a".into(),
            text: body,
        }),
    ] {
        insert(&db, event, true);
    }
    drop(db);
    let source = fs::read(&path).unwrap();
    let options = CompactOptions {
        delta_mode: DeltaMode::Delete,
        ..options()
    };
    let preview = compact_events(&path, &options).unwrap();
    assert_eq!(preview.event_compression.eligible_values, 1);
    assert_eq!(preview.event_compression.estimated_compressed_values, 1);
    assert_eq!(source, fs::read(&path).unwrap());
    let result = compact_events(
        &path,
        &CompactOptions {
            apply: true,
            ..options
        },
    )
    .unwrap();
    assert_eq!(result.compressed_event_rows, 1);
    assert_eq!(preview.projected_event_rows, result.projected_event_rows);
    assert_eq!(
        serde_json::to_value(preview.event_compression).unwrap(),
        serde_json::to_value(result.event_compression).unwrap(),
    );
}

#[test]
fn leases_nonempty_wal_existing_backup_and_sqlite_reader_refuse_without_payload_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = seed(&path);
    insert(&db, delta("a", "body"), false);
    let held = offline::writer_lease(&path).unwrap();
    assert!(
        compact_events(
            &path,
            &CompactOptions {
                apply: true,
                ..options()
            }
        )
        .is_err()
    );
    drop(held);
    assert!(compact_events(&path, &options()).is_err());
    // No cooperative lease: SQLite still refuses the active reader.
    db.conn
        .execute_batch("BEGIN; SELECT COUNT(*) FROM events;")
        .unwrap();
    assert!(
        compact_events(
            &path,
            &CompactOptions {
                apply: true,
                ..options()
            }
        )
        .is_err()
    );
    db.conn.execute_batch("ROLLBACK;").unwrap();
    drop(db);
    let before = fs::read(&path).unwrap();
    let backup = dir.path().join("existing.db");
    fs::write(&backup, b"do not overwrite").unwrap();
    assert!(
        compact_events(
            &path,
            &CompactOptions {
                apply: true,
                backup: Some(backup.clone()),
                ..options()
            }
        )
        .is_err()
    );
    assert_eq!(fs::read(&backup).unwrap(), b"do not overwrite");
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn compression_uses_writer_policy_and_recomputes_accounting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = seed(&path);
    db.conn
        .execute(
            "INSERT INTO sessions(id,status,total_event_bytes,created_at_ns,updated_at_ns)
         VALUES('s','running',999,0,0)",
            [],
        )
        .unwrap();
    let body = "bulky body ".repeat(1000);
    for (kind, subkind) in [
        ("Tool", "ToolCompleted"),
        ("Tool", "ApprovalRequested"),
        ("AgentMessage", "Delivered"),
        ("Message", "MessageCompleted"),
        ("compaction", "Compacted"),
        ("Lifecycle", "TaskPromptPublished"),
    ] {
        // Include non-decodable historical variants: attribution must not be
        // guessed and compression must preserve the original text byte-for-byte.
        db.conn.execute(
            "INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,subkind,payload) VALUES(1,0,0,?1,?2,?3)",
            params![kind, subkind, body],
        ).unwrap();
    }
    db.conn
        .execute("UPDATE events SET session_id='s'", [])
        .unwrap();
    drop(db);
    let summary = compact_events(
        &path,
        &CompactOptions {
            apply: true,
            ..options()
        },
    )
    .unwrap();
    assert_eq!(summary.compressed_event_rows, 3);
    let db = seed(&path);
    let sum: i64 = db
        .conn
        .query_row("SELECT SUM(OCTET_LENGTH(payload)) FROM events", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        sum,
        db.conn
            .query_row(
                "SELECT total_event_bytes FROM sessions WHERE id='s'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap()
    );
    assert_eq!(
        sum,
        db.conn
            .query_row(
                "SELECT payload_bytes FROM event_session_bytes WHERE session_id='s'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap()
    );
    let mut stmt = db.conn.prepare("SELECT payload FROM events").unwrap();
    for decoded in stmt
        .query_map([], |row| codec::text_column(row, 0))
        .unwrap()
    {
        assert_eq!(decoded.unwrap(), body);
    }
    assert_eq!(
        db.conn
            .query_row("SELECT SUM(OCTET_LENGTH(payload)) FROM events", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        db.conn
            .query_row(
                "SELECT SUM(payload_bytes) FROM event_day_bytes",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap()
    );
    let mut stmt = db
        .conn
        .prepare("SELECT kind,subkind,typeof(payload) FROM events ORDER BY id")
        .unwrap();
    let types = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for (kind, subkind, ty) in types {
        assert_eq!(
            ty == "blob",
            crate::repo::event::compressible_payload(&kind, &subkind)
        );
    }
}

#[test]
fn public_context_reader_round_trips_valid_messages_and_large_checkpoints_in_mixed_formats() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = seed(&path);
    let message = providers::Message {
        role: providers::Role::User,
        content: vec![providers::ContentBlock::Text {
            text: "保持する本文🙂".repeat(1000),
        }],
    };
    let messages = serde_json::to_string(&vec![&message]).unwrap();
    let checkpoints = serde_json::json!([{
        "id": "checkpoint", "summary": message, "range": [0,1]
    }])
    .to_string();
    let runs = ["plain", "blob", "mixed"];
    for run in runs {
        let messages_value = if run == "blob" {
            codec::encode(&messages)
        } else {
            codec::Encoded::Plain(&messages)
        };
        let checkpoints_value = if run == "plain" {
            codec::Encoded::Plain(&checkpoints)
        } else {
            codec::encode(&checkpoints)
        };
        db.conn.execute(
            "INSERT INTO run_contexts(run_id,role,name,config_json,messages_json,checkpoints_json,terminal_phase,restorable,updated_at_ns,messages_len,checkpoints_len)
             VALUES(?1,'Worker','context','{}',?2,?3,'Done',1,42,?4,?5)",
            params![run, messages_value, checkpoints_value, messages.len() as i64, checkpoints.len() as i64],
        ).unwrap();
    }
    let before: Vec<_> = runs
        .iter()
        .map(|run| db.run_context(run).unwrap().unwrap())
        .collect();
    for record in &before {
        assert_eq!(
            serde_json::from_str::<Vec<providers::Message>>(&record.messages_json).unwrap(),
            vec![message.clone()]
        );
        let checkpoint: serde_json::Value = serde_json::from_str(&record.checkpoints_json).unwrap();
        assert_eq!(
            checkpoint[0]["summary"],
            serde_json::to_value(&message).unwrap()
        );
    }
    drop(db);
    let preview = compact_events(&path, &options()).unwrap();
    assert_eq!(preview.context_compression.eligible_values, 3);
    for iteration in 0..2 {
        let result = compact_events(
            &path,
            &CompactOptions {
                apply: true,
                ..options()
            },
        )
        .unwrap();
        assert_eq!(
            result.compressed_context_values,
            if iteration == 0 { 3 } else { 0 }
        );
        let db = seed(&path);
        let after: Vec<_> = runs
            .iter()
            .map(|run| db.run_context(run).unwrap().unwrap())
            .collect();
        assert_eq!(before, after);
        for run in runs {
            let (messages_len, checkpoints_len): (usize, usize) = db
                .conn
                .query_row(
                    "SELECT messages_len,checkpoints_len FROM run_contexts WHERE run_id=?1",
                    [run],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(
                (messages_len, checkpoints_len),
                (messages.len(), checkpoints.len())
            );
        }
    }
}

#[test]
fn online_backfill_works_with_writer_lease_and_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = seed(&path);
    for _ in 0..7 {
        insert(&db, delta("a", "body"), true);
    }
    let _writer = offline::writer_lease(&path).unwrap();
    let first = backfill_events(
        &path,
        &EventBackfillOptions {
            batch_rows: 2,
            max_rows: Some(3),
        },
    )
    .unwrap();
    assert_eq!((first.converted, first.batches, first.remaining), (3, 2, 4));
    assert!(
        compact_events(
            &path,
            &CompactOptions {
                apply: true,
                ..options()
            }
        )
        .is_err()
    );
    assert_eq!(
        backfill_events(&path, &EventBackfillOptions::default())
            .unwrap()
            .remaining,
        0
    );
}

#[test]
fn retention_preview_matches_apply_and_disabled_retention_preserves_observations() {
    use event_bus::{CacheBaselineMissing, CacheComparison, FaultEvent, ProviderEvent};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = seed(&path);
    let provider = |event| Event::new(event);
    let observations = [
        provider(ProviderEvent::FirstTokenObserved {
            request_id: "req".into(),
            provider: "mock".into(),
            profile: None,
            protocol: "mock".into(),
            model: "mock".into(),
            ttft_ms: 1,
            run_id: Some("a".into()),
        }),
        provider(ProviderEvent::RequestCompleted {
            usage_reported: true,
            request_id: "req".into(),
            provider: "mock".into(),
            profile: None,
            protocol: "mock".into(),
            model: "mock".into(),
            streaming: true,
            duration_ms: 2,
            input_tokens: 1,
            output_tokens: 1,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            finish_reason: "stop".into(),
            run_id: Some("a".into()),
            reasoning_tokens: None,
            purpose: None,
            service_tier: None,
        }),
        provider(ProviderEvent::CacheReuseObserved {
            request_id: "req".into(),
            cache_read_tokens: 0,
            comparison: CacheComparison::NoBaseline {
                reason: CacheBaselineMissing::NoPreviousRequest,
            },
            run_id: Some("a".into()),
        }),
    ];
    insert(&db, delta("a", "before observation"), false);
    for event in &observations {
        insert(&db, event.clone(), true);
    }
    insert(&db, delta("a", "after observation"), false);
    insert(
        &db,
        provider(ProviderEvent::RequestStarted {
            request_id: "req".into(),
            provider: "mock".into(),
            profile: None,
            protocol: "mock".into(),
            model: "mock".into(),
            streaming: true,
            run_id: Some("a".into()),
            reasoning_effort: None,
            purpose: None,
        }),
        true,
    );
    insert(
        &db,
        Event::new(FaultEvent::SubscriberLagged {
            subscriber_id: 1,
            skipped: 4,
        }),
        true,
    );
    insert(
        &db,
        Event::new(event_bus::LifecycleEvent::RunProgress {
            run_id: "a".into(),
            activity: event_bus::RunActivity::Model,
            context: None,
        }),
        true,
    );
    // Retention boundaries are checked with explicit nanoseconds, not elapsed time.
    let cutoff = 100;
    assert_eq!(
        delete_matching(&db.conn, &options(), Some(cutoff))
            .unwrap()
            .input_rows,
        3
    );
    db.conn
        .execute(
            "UPDATE events SET wall_clock_ns=100 WHERE subkind IS NULL AND kind='Provider'",
            [],
        )
        .unwrap();
    assert_eq!(
        delete_matching(&db.conn, &options(), Some(cutoff))
            .unwrap()
            .input_rows,
        0
    );
    db.conn
        .execute("UPDATE events SET wall_clock_ns=0", [])
        .unwrap();
    drop(db);
    let disabled = compact_events(&path, &options()).unwrap();
    assert_eq!(disabled.provider_observations.input_rows, 0);
    let preview = compact_events(
        &path,
        &CompactOptions {
            provider_retention_days: 90,
            ..options()
        },
    )
    .unwrap();
    assert_eq!(preview.provider_observations.input_rows, 3);
    assert_eq!(preview.noise.input_rows, 2);
    let apply = compact_events(
        &path,
        &CompactOptions {
            apply: true,
            provider_retention_days: 90,
            ..options()
        },
    )
    .unwrap();
    assert_eq!(
        apply.provider_observations.input_rows,
        preview.provider_observations.input_rows
    );
    assert_eq!(apply.reasoning_deltas.output_rows, 1);
    assert_eq!(apply.after.events["Provider/RequestStarted"].rows, 1);
    let second = compact_events(
        &path,
        &CompactOptions {
            apply: true,
            provider_retention_days: 90,
            ..options()
        },
    )
    .unwrap();
    assert_eq!(
        second.reasoning_deltas.input_rows,
        second.reasoning_deltas.output_rows
    );
}

#[test]
fn destructive_delta_mode_is_explicit_and_keeps_completed_responses() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = seed(&path);
    insert(&db, delta("a", "reasoning to discard"), true);
    insert(
        &db,
        Event::new(MessageEvent::MessageDelta {
            run_id: Some("a".into()),
            delta: "incomplete text to discard".into(),
        }),
        true,
    );
    insert(
        &db,
        Event::new(MessageEvent::MessageCompleted {
            run_id: "a".into(),
            text: "accepted response".into(),
        }),
        true,
    );
    drop(db);
    let options = CompactOptions {
        delta_mode: DeltaMode::Delete,
        ..options()
    };
    let preview = compact_events(&path, &options).unwrap();
    assert_eq!(preview.message_deltas.input_rows, 1);
    assert_eq!(preview.reasoning_deltas.input_rows, 1);
    let summary = compact_events(
        &path,
        &CompactOptions {
            apply: true,
            ..options
        },
    )
    .unwrap();
    assert_eq!(summary.after.events.len(), 1);
    assert_eq!(summary.after.events["Message/MessageCompleted"].rows, 1);
}

#[test]
fn preview_compression_samples_at_most_one_thousand_values_without_rewriting_them() {
    let db = Database::open_in_memory().unwrap();
    let body = "compressible ".repeat(200);
    let tx = db.conn.unchecked_transaction().unwrap();
    for id in 0..1001 {
        tx.execute(
            "INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,subkind,payload)
             VALUES(1,0,0,'Tool','ToolCompleted',?1)",
            [&body],
        )
        .unwrap();
        tx.execute(
            "INSERT INTO run_contexts(run_id,role,name,config_json,messages_json,checkpoints_json,terminal_phase,restorable,updated_at_ns)
             VALUES(?1,'Worker','test','{}',?2,'[]','Done',0,0)", params![format!("run-{id}"), body]
        ).unwrap();
    }
    tx.commit().unwrap();
    let (events, converted) = statistics::compress_events(&db.conn, &options(), None).unwrap();
    assert_eq!(converted, 0);
    assert_eq!(
        (events.eligible_values, events.sampled_values),
        (1001, 1000)
    );
    assert_eq!(events.estimated_compressed_values, 1001);
    let (contexts, converted) = statistics::compress_contexts(&db.conn, &options()).unwrap();
    assert_eq!(converted, 0);
    assert_eq!(
        (contexts.eligible_values, contexts.sampled_values),
        (1001, 1000)
    );
    assert!(events.estimated_saved_bytes > 0 && contexts.estimated_saved_bytes > 0);
    assert_eq!(
        db.conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE typeof(payload)='text'",
                [],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
        1001
    );
}

#[test]
fn accounting_days_and_invalid_wall_clocks_are_independent_delta_boundaries() {
    let db = Database::open_in_memory().unwrap();
    for _ in 0..4 {
        insert(&db, delta("a", "body"), false);
    }
    db.conn
        .execute_batch(
            "UPDATE events SET wall_clock_ns=CASE id WHEN 1 THEN -2 WHEN 2 THEN -1
         WHEN 3 THEN 0 ELSE 86400000000000 END;",
        )
        .unwrap();
    let before = crate::repo::event::list_all_ordered(&db.conn).unwrap();
    let (summary, _) = deltas::compact(
        &db.conn,
        &CompactOptions {
            apply: true,
            ..options()
        },
        true,
        None,
    )
    .unwrap();
    assert_eq!((summary.input_rows, summary.output_rows), (4, 4));
    assert_eq!(
        before,
        crate::repo::event::list_all_ordered(&db.conn).unwrap()
    );
}

#[test]
fn compact_apply_switches_a_legacy_database_to_incremental_auto_vacuum() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let db = seed(&path);
    db.conn.pragma_update(None, "auto_vacuum", 0).unwrap();
    db.conn.execute_batch("VACUUM;").unwrap();
    drop(db);

    let preview = compact_events(&path, &options()).unwrap();
    assert_eq!(
        (preview.auto_vacuum_before, preview.auto_vacuum_after),
        (0, 0)
    );

    let applied = compact_events(
        &path,
        &CompactOptions {
            apply: true,
            ..options()
        },
    )
    .unwrap();
    assert_eq!(
        (applied.auto_vacuum_before, applied.auto_vacuum_after),
        (0, 2)
    );
    assert_eq!(applied.integrity_check.as_deref(), Some("ok"));
    let conn = Connection::open(&path).unwrap();
    let mode: i64 = conn
        .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
        .unwrap();
    assert_eq!(mode, 2);
}
