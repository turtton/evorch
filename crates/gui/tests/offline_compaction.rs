//! Exercise the real GUI transcript reader, not a second implementation of it.

use std::{collections::BTreeMap, path::Path};

use event_bus::{Event, EventKind, MessageEvent, MessagePartAssembler, ToolEvent};
use gui::model::{transcript::TranscriptEntry, transcript_registry::TranscriptRegistry};
use rusqlite::params;
use serde_json::json;
use storage::{
    Database, Storage, StorageConfig,
    maintenance::{CompactOptions, compact_events},
};

fn message(run: &str, text: &str, reasoning: bool) -> Event {
    Event::new(if reasoning {
        MessageEvent::ReasoningDelta {
            run_id: Some(run.into()),
            delta: text.into(),
        }
    } else {
        MessageEvent::MessageDelta {
            run_id: Some(run.into()),
            delta: text.into(),
        }
    })
}

fn replay(path: &Path) -> BTreeMap<String, Vec<TranscriptEntry>> {
    let db = Database::open(&StorageConfig {
        db_path: path.into(),
        ..Default::default()
    })
    .unwrap();
    let mut parts = MessagePartAssembler::new();
    let mut transcripts = TranscriptRegistry::new();
    transcripts.bind_thread_root("thread", "a");
    transcripts.bind_run("b", "thread");
    transcripts.select_thread(Some("thread".into()));
    db.for_each_event_ordered(|stored| {
        if let Some(event) = parts.accept(&stored.event) {
            transcripts.apply(&event);
        }
    })
    .unwrap();
    let mut result: BTreeMap<_, _> = transcripts
        .run_ids()
        .map(|run| {
            (
                run.clone(),
                transcripts.run(run).unwrap().entries().to_vec(),
            )
        })
        .collect();
    result.insert("__thread__".into(), transcripts.thread().entries().to_vec());
    result
}

fn empty_sidebar(path: &Path) {
    std::fs::write(
        path.parent().unwrap().join("sidebar.json"),
        br#"{"user_messages":[]}"#,
    )
    .unwrap();
}

#[test]
fn child_terminal_notification_is_a_root_delta_boundary_in_both_run_and_thread() {
    for reasoning in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        empty_sidebar(&path);
        let storage = Storage::open(StorageConfig {
            db_path: path.clone(),
            ..Default::default()
        })
        .unwrap();
        for event in [
            message("a", "before ", reasoning),
            message("a", "child ", reasoning),
            Event::new(event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id: "b".into(),
                from: event_bus::AgentRunPhase::Running,
                to: event_bus::AgentRunPhase::Done,
                reason: None,
            }),
            message("a", "after ", reasoning),
            message("a", "notification", reasoning),
        ] {
            storage.handle().append_event(None, &event).unwrap();
        }
        storage.close();
        let before = replay(&path);
        assert!(before.contains_key("a") && before.contains_key("b"));
        assert!(before["__thread__"].len() >= 3);
        let options = CompactOptions {
            batch_rows: 2,
            provider_retention_days: 0,
            ..Default::default()
        };
        let preview = compact_events(&path, &options).unwrap();
        for _ in 0..2 {
            let result = compact_events(
                &path,
                &CompactOptions {
                    apply: true,
                    ..options.clone()
                },
            )
            .unwrap();
            assert_eq!(before, replay(&path), "reasoning={reasoning}");
            assert_eq!(preview.projected_event_rows, result.projected_event_rows);
        }
    }
}

fn seed(path: &Path) {
    empty_sidebar(path);
    let storage = Storage::open(StorageConfig {
        db_path: path.into(),
        ..Default::default()
    })
    .unwrap();
    storage.close();
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut events = vec![];
    for _ in 0..5 {
        events.push(message("a", "考え🙂\\\n", true));
        events.push(message("b", "other reasoning ", true));
    }
    events.extend([
        message("a", "old ", false),
        message("b", "unfinished ", false),
        message("a", "draft", false),
        // The completion updates a bubble before the following reasoning.
        message("a", "interleaved reasoning", true),
        Event::new(MessageEvent::MessageCompleted {
            run_id: "a".into(),
            text: "canonical answer".into(),
        }),
        message("b", "message", false),
        Event::new(ToolEvent::ToolStarted {
            run_id: Some("a".into()),
            call_id: "a:call".into(),
            tool_name: "read".into(),
            input: None,
            parent_call_id: None,
        }),
        Event::new(ToolEvent::ToolCompleted {
            run_id: Some("a".into()),
            call_id: "a:call".into(),
            tool_name: "read".into(),
            output: Some("large output ".repeat(1000)),
            is_error: false,
            detail: None,
            parent_call_id: None,
        }),
        message("a", "parts draft", false),
        Event::new(MessageEvent::MessagePart {
            run_id: "a".into(),
            response_id: "response".into(),
            index: 0,
            total: 2,
            text: "part ".into(),
        }),
        message("b", "still here", true),
        Event::new(MessageEvent::MessagePart {
            run_id: "a".into(),
            response_id: "response".into(),
            index: 1,
            total: 2,
            text: "answer".into(),
        }),
        message("b", "after reasoning", false),
        Event::new(MessageEvent::StreamRetried {
            run_id: Some("b".into()),
        }),
        message("b", "retry partial ", false),
        message("b", "tail", false),
        Event::new(MessageEvent::MessagePart {
            run_id: "b".into(),
            response_id: "incomplete".into(),
            index: 0,
            total: 2,
            text: "not a completion".into(),
        }),
    ]);
    for (i, event) in events.iter().enumerate() {
        let kind = match event.kind {
            EventKind::Message(_) => "Message",
            EventKind::Tool(_) => "Tool",
            _ => unreachable!(),
        };
        conn.execute(
            "INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,subkind,run_id,payload)
             VALUES(1,?1,0,?2,?3,?4,?5)",
            params![i, kind, (i%3 != 0).then(|| event.kind.subkind_name()),
                event.primary_run_id(), serde_json::to_string(&event.kind).unwrap()],
        ).unwrap();
    }
    // Historical TEXT alongside a BLOB produced by the public writer.
    let kind = Event::new(ToolEvent::ToolCompleted {
        run_id: Some("a".into()),
        call_id: "a:other".into(),
        tool_name: "read".into(),
        output: Some("compressed output ".repeat(1000)),
        is_error: false,
        detail: None,
        parent_call_id: None,
    });
    drop(conn);
    let storage = Storage::open(StorageConfig {
        db_path: path.into(),
        ..Default::default()
    })
    .unwrap();
    storage.handle().append_event(None, &kind).unwrap();
    storage.close();
}

#[test]
fn compact_preserves_gui_transcript_content_and_order_with_completed_and_incomplete_runs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    seed(&path);
    let before = replay(&path);
    assert!(before["a"].iter().any(
        |entry| matches!(entry, TranscriptEntry::Message { text, .. } if text=="canonical answer")
    ));
    assert!(before["a"].iter().any(
        |entry| matches!(entry, TranscriptEntry::Message { text, .. } if text=="part answer")
    ));
    assert!(
        before["b"]
            .iter()
            .any(|entry| matches!(entry, TranscriptEntry::Reasoning { .. }))
    );
    let options = CompactOptions {
        batch_rows: 3,
        provider_retention_days: 0,
        ..Default::default()
    };
    let preview = compact_events(&path, &options).unwrap();
    let summary = compact_events(
        &path,
        &CompactOptions {
            apply: true,
            ..options.clone()
        },
    )
    .unwrap();
    assert_eq!(
        summary.message_deltas.input_rows - summary.message_deltas.output_rows,
        preview.message_deltas.input_rows - preview.message_deltas.output_rows
    );
    assert_eq!(before, replay(&path));
    compact_events(
        &path,
        &CompactOptions {
            apply: true,
            ..options
        },
    )
    .unwrap();
    assert_eq!(before, replay(&path));
    assert_eq!(summary.integrity_check.as_deref(), Some("ok"));
    let json = serde_json::to_value(summary).unwrap();
    assert_eq!(json["after"]["unbackfilled_rows"], json!(0));
}
