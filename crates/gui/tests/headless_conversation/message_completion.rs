use event_bus::{Event, MessageEvent, ToolEvent};
use gui::model::transcript::{TranscriptEntry, TranscriptModel};
use gui::model::transcript_registry::TranscriptRegistry;
use recorder::storage_bridge::{OwnedStorageBridge, StorageBridge};

fn delta(run: &str, text: &str) -> Event {
    Event::new(MessageEvent::MessageDelta {
        run_id: Some(run.into()),
        delta: text.into(),
    })
}

fn completed(run: &str, text: &str) -> Event {
    Event::new(MessageEvent::MessageCompleted {
        run_id: run.into(),
        text: text.into(),
    })
}

fn answer(run: &str, text: &str) -> TranscriptEntry {
    TranscriptEntry::Message {
        run_id: Some(run.into()),
        text: text.into(),
    }
}

#[test]
fn completed_answer_repairs_missing_deltas_in_live_and_persisted_history() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("history.sqlite3"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    let events = [
        delta("run", "first "),
        delta("run", "last"),
        completed("run", "first missing last"),
    ];
    let mut live = TranscriptRegistry::new();
    live.bind_thread_root("thread", "run");
    live.select_thread(Some("thread".into()));
    for event in &events {
        live.apply(event);
        bridge.handle_event(event).unwrap();
    }
    storage.close();

    let db = storage::Database::open(&config).unwrap();
    assert!(
        !db.events_all_ordered()
            .unwrap()
            .iter()
            .any(|stored| matches!(
                stored.event.kind,
                event_bus::EventKind::Message(MessageEvent::MessageDelta { .. })
            ))
    );
    let mut replayed = TranscriptRegistry::new();
    replayed.bind_thread_root("thread", "run");
    replayed.select_thread(Some("thread".into()));
    for stored in db.events_all_ordered().unwrap() {
        replayed.apply(&stored.event);
    }
    for registry in [&live, &replayed] {
        for model in [registry.thread(), registry.run("run").unwrap()] {
            assert_eq!(model.entries(), &[answer("run", "first missing last")]);
        }
    }
}

fn thought(run: &str, text: &str) -> Event {
    Event::new(MessageEvent::ReasoningDelta {
        run_id: Some(run.into()),
        delta: text.into(),
    })
}

fn tool_started(run: &str) -> Event {
    Event::new(ToolEvent::ToolStarted {
        run_id: Some(run.into()),
        tool_name: "read".into(),
        call_id: format!("{run}:call"),
        input: None,
        parent_call_id: None,
    })
}

#[test]
fn concurrent_runs_restore_answers_from_completion_and_reasoning_from_merged_rows() {
    // Given: two runs streaming at once through the real owned bridge.
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("history.sqlite3"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = std::sync::Arc::new(event_bus::EventBus::new(1024));
    let mut owner = OwnedStorageBridge::spawn(
        bus.clone(),
        storage,
        |handle| StorageBridge::new(handle, "session"),
        std::time::Duration::from_secs(3600),
    )
    .unwrap();
    let runs = ["run-a", "run-b"];
    let mut events = Vec::new();
    for round in 0..3 {
        for run in runs {
            events.push(thought(run, &format!("{run} plan {round} ")));
        }
    }
    for run in runs {
        events.push(tool_started(run));
    }
    for round in 0..3 {
        for run in runs {
            events.push(thought(run, &format!("{run} review {round} ")));
        }
    }
    for round in 0..3 {
        for run in runs {
            events.push(delta(run, &format!("{run} part {round} ")));
        }
    }
    for run in runs {
        events.push(completed(run, &format!("{run} answer")));
    }
    let mut live = TranscriptRegistry::new();
    for event in &events {
        live.apply(event);
        bus.emit(event.clone());
    }

    // When: the bridge persists the stream and the history is replayed.
    owner.flush().unwrap();
    owner.shutdown();
    drop(owner);
    let db = storage::Database::open(&config).unwrap();
    let rows = db.events_all_ordered().unwrap();
    let mut replayed = TranscriptRegistry::new();
    for stored in &rows {
        replayed.apply(&stored.event);
    }

    // Then: no streamed answer text is stored, each reasoning stretch is one
    // row, and the restored transcripts match the live ones entry for entry.
    let kinds = |run: &str| -> Vec<&'static str> {
        rows.iter()
            .filter_map(|stored| match &stored.event.kind {
                event_bus::EventKind::Message(MessageEvent::ReasoningDelta {
                    run_id: Some(id),
                    ..
                }) if id == run => Some("reasoning"),
                event_bus::EventKind::Message(MessageEvent::MessageCompleted {
                    run_id, ..
                }) if run_id == run => Some("completed"),
                event_bus::EventKind::Tool(ToolEvent::ToolStarted {
                    run_id: Some(id), ..
                }) if id == run => Some("tool"),
                event_bus::EventKind::Message(MessageEvent::MessageDelta { .. }) => {
                    panic!("streamed answer text must not be stored")
                }
                _ => None,
            })
            .collect()
    };
    for run in runs {
        assert_eq!(kinds(run), ["reasoning", "tool", "reasoning", "completed"]);
        let restored = replayed.run(run).unwrap().entries();
        assert_eq!(restored, live.run(run).unwrap().entries(), "{run}");
        assert_eq!(
            restored.last(),
            Some(&answer(run, &format!("{run} answer")))
        );
        let reasoning: Vec<_> = restored
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Reasoning { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning,
            [
                format!("{run} plan 0 {run} plan 1 {run} plan 2 "),
                format!("{run} review 0 {run} review 1 {run} review 2 ")
            ]
        );
    }
}

#[test]
fn completed_responses_stay_separate_even_if_text_repeats_or_all_deltas_are_lost() {
    let mut model = TranscriptModel::new();
    model.apply(&delta("run", "repeat"));
    let first = completed("run", "repeat");
    model.apply(&first);
    model.apply(&first); // Redelivery of this event is idempotent.
    model.apply(&delta("run", "rep"));
    model.apply(&completed("run", "repeat"));
    model.apply(&completed("run", "repeat")); // A new response with no received deltas.
    assert_eq!(model.entries(), &vec![answer("run", "repeat"); 3]);
}

#[test]
fn completion_consolidates_split_fragments_and_keeps_other_run_tracking() {
    let mut model = TranscriptModel::new();
    model.apply(&delta("first", "a"));
    model.apply(&Event::new(MessageEvent::ReasoningDelta {
        run_id: Some("first".into()),
        delta: "thought".into(),
    }));
    model.apply(&delta("first", "c"));
    model.apply(&delta("second", "x"));
    model.apply(&completed("first", "abc"));
    model.apply(&completed("second", "xyz"));
    assert_eq!(
        model.entries(),
        &[
            answer("first", "abc"),
            TranscriptEntry::Reasoning {
                text: "thought".into(),
                run_id: Some("first".into())
            },
            answer("second", "xyz"),
        ]
    );
    assert!(!model.thinking_is_streaming(model.visible_entry_id(1)));
}

#[test]
fn legacy_tool_and_user_boundaries_preserve_earlier_messages() {
    let mut model = TranscriptModel::new();
    model.apply(&delta("run", "old answer"));
    model.push_user_message("continue");
    model.apply(&delta("run", "progress"));
    model.apply(&Event::new(ToolEvent::ToolStarted {
        run_id: Some("run".into()),
        tool_name: "read".into(),
        call_id: "call".into(),
        input: None,
        parent_call_id: None,
    }));
    model.apply(&delta("run", "partial"));
    model.apply(&completed("run", "complete answer"));
    assert_eq!(model.entries()[0], answer("run", "old answer"));
    assert_eq!(model.entries()[2], answer("run", "progress"));
    assert_eq!(model.entries()[4], answer("run", "complete answer"));
    assert_eq!(model.entries().len(), 5);
}

#[test]
fn completion_after_capacity_eviction_restores_one_full_answer() {
    let mut model = TranscriptModel::with_capacity(2);
    model.apply(&delta("run", "evicted"));
    model.push_notice("first notice");
    model.push_notice("second notice");
    model.apply(&completed("run", "complete answer"));
    assert_eq!(
        model.entries().last(),
        Some(&answer("run", "complete answer"))
    );
    assert_eq!(model.entries().len(), 2);
}
