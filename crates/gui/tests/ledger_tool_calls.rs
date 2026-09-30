#[path = "ledger_tool_calls/fixture.rs"]
mod fixture;

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, EventKind, ToolEvent};
use fixture::{Call, execute};
use gui::model::ledger::LedgerRegistry;
use gui::model::transcript::{ToolStatus, TranscriptEntry, TranscriptModel};
use gui::model::transcript_registry::TranscriptRegistry;
use gui::storage_bridge::StorageBridge;
use serde_json::{Value, json};
use storage::{Database, Storage, StorageConfig};

fn project(run: &str, events: &[Event]) -> TranscriptRegistry {
    let mut registry = TranscriptRegistry::new();
    registry.bind_thread_root("ledger-thread", run);
    registry.select_thread(Some("unrelated-thread".into()));
    for event in events {
        registry.apply(event);
    }
    assert!(tools(registry.thread()).is_empty());
    registry.select_thread(Some("ledger-thread".into()));
    registry
}

fn tools(model: &TranscriptModel) -> Vec<&TranscriptEntry> {
    model
        .entries()
        .iter()
        .filter(|entry| matches!(entry, TranscriptEntry::Tool { .. }))
        .collect()
}

fn assert_calls(
    run: &str,
    events: &[Event],
    calls: &[Call],
    errors: &[bool],
) -> TranscriptRegistry {
    let lifecycle: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Tool(
                event @ (ToolEvent::ToolStarted { .. } | ToolEvent::ToolCompleted { .. }),
            ) => Some(event),
            _ => None,
        })
        .collect();
    assert_eq!(
        lifecycle.len(),
        calls.len() * 2,
        "each ledger call emits exactly one lifecycle pair"
    );
    let mut running = TranscriptRegistry::new();
    for (index, ((name, input), is_error)) in calls.iter().zip(errors).enumerate() {
        let ToolEvent::ToolStarted {
            tool_name,
            call_id,
            input: actual_input,
            run_id,
        } = lifecycle[index * 2]
        else {
            panic!("start before completion")
        };
        assert_eq!(tool_name, name);
        assert_eq!(run_id.as_deref(), Some(run));
        assert_eq!(actual_input.as_ref(), Some(input));
        running.apply(&Event::new(lifecycle[index * 2].clone()));
        assert!(matches!(
            tools(running.run(run).unwrap()).last(),
            Some(TranscriptEntry::Tool {
                status: ToolStatus::Running,
                ..
            })
        ));
        let ToolEvent::ToolCompleted {
            tool_name,
            call_id: completed_id,
            run_id,
            is_error: actual_error,
            output,
            ..
        } = lifecycle[index * 2 + 1]
        else {
            panic!("completion after start")
        };
        assert_eq!(tool_name, name);
        assert_eq!(completed_id, call_id);
        assert_eq!(run_id.as_deref(), Some(run));
        assert_eq!(actual_error, is_error);
        assert!(output.is_some());
    }
    let registry = project(run, events);
    assert_eq!(tools(registry.thread()), tools(registry.run(run).unwrap()));
    let entries = tools(registry.thread());
    assert_eq!(entries.len(), calls.len());
    for (entry, is_error) in entries.iter().zip(errors) {
        let TranscriptEntry::Tool { status, .. } = entry else {
            unreachable!()
        };
        assert_eq!(
            *status,
            if *is_error {
                ToolStatus::Failed
            } else {
                ToolStatus::Succeeded
            }
        );
    }
    registry
}

fn assert_card(
    entry: &TranscriptEntry,
    name: &str,
    input: &Value,
    error: bool,
    output_contains: &str,
) {
    let mut harness = Harness::new_ui(|ui| {
        gui::theme::install(ui.ctx());
        gui::panes::transcript_tool::tool_card(ui, entry, egui::Id::new("ledger-test"));
    });
    harness.run_steps(2);
    let header = format!("{} {name}", if error { "✗" } else { "✓" });
    assert!(harness.query_by_label("Input").is_none());
    harness.get_by_label(&header).click();
    harness.run_steps(3);
    assert!(harness.query_by_label("Input").is_some());
    assert!(
        harness
            .query_by_label(if error { "Error" } else { "Output" })
            .is_some()
    );
    assert!(
        harness
            .query_by_label(&serde_json::to_string_pretty(input).unwrap())
            .is_some()
    );
    assert!(harness.query_by_label_contains(output_contains).is_some());
}

#[tokio::test]
async fn ledger_calls_render_as_tools_and_survive_history_replay() {
    // Given: a real run store, not synthetic GUI-only tool events.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("ledger.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let calls = [
        ("ledger_read", json!({})),
        ("ledger_append", json!({"body":"durable decision"})),
        ("ledger_read", json!({})),
    ];
    // When: the actual runtime dispatches the ledger meta operations.
    let (run, events) = execute(&calls, Some((&config, &storage))).await;
    let registry = assert_calls(&run, &events, &calls, &[false; 3]);
    let entries = tools(registry.thread());
    for ((entry, (name, input)), output) in
        entries
            .iter()
            .zip(&calls)
            .zip(["[]", "seq", "durable decision"])
    {
        assert_card(entry, name, input, false, output);
    }
    // Then: the durable memo remains independent of the read/append tool cards.
    let mut ledger = LedgerRegistry::default();
    let mut bridge = StorageBridge::new(storage.handle(), "ledger-test");
    for event in &events {
        ledger.apply(event);
        bridge.handle_event(event).unwrap();
    }
    assert_eq!(ledger.entries(&run).len(), 1);
    assert_eq!(ledger.entries(&run)[0].body, "durable decision");
    drop(bridge);
    drop(storage);
    let db = Database::open(&config).unwrap();
    assert_eq!(db.run_ledger(&run).unwrap().len(), 1);
    let restored: Vec<_> = db
        .events_all_ordered()
        .unwrap()
        .into_iter()
        .map(|row| row.event)
        .collect();
    let replay = project(&run, &restored);
    assert_eq!(tools(replay.thread()), entries);
}

#[tokio::test]
async fn missing_store_and_rejected_ledger_arguments_render_failed_tools() {
    let calls = [
        ("ledger_append", json!({"body":"decision"})),
        ("ledger_read", json!({})),
        ("ledger_append", json!({})),
        ("ledger_read", json!({"unexpected":true})),
    ];
    let (run, events) = execute(&calls, None).await;
    let registry = assert_calls(&run, &events, &calls, &[true; 4]);
    let expected_errors = [
        "run_store_unavailable",
        "run_store_unavailable",
        "ツールの引数が不正です",
        "ツールの引数が不正です",
    ];
    for ((entry, (name, input)), expected) in tools(registry.thread())
        .into_iter()
        .zip(&calls)
        .zip(expected_errors)
    {
        assert_card(entry, name, input, true, expected);
    }
}

#[tokio::test]
async fn storage_rejection_renders_failed_append_without_creating_a_memo() {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("ledger.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let calls = [("ledger_append", json!({"body":""}))];
    let (run, events) = execute(&calls, Some((&config, &storage))).await;
    let registry = assert_calls(&run, &events, &calls, &[true]);
    assert_card(
        tools(registry.thread())[0],
        calls[0].0,
        &calls[0].1,
        true,
        "storage_failure",
    );
    assert!(
        Database::open(&config)
            .unwrap()
            .run_ledger(&run)
            .unwrap()
            .is_empty()
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.kind, EventKind::Ledger(_)))
    );
}
