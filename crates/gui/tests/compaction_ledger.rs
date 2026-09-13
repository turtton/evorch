use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{CompactionEvent, CompactionReason, Event};
use gui::{model::transcript_registry::TranscriptRegistry, storage_bridge::StorageBridge};
use storage::{Database, Storage, StorageConfig};

#[test]
fn compaction_survives_replay_without_loss_or_duplication() {
    // Given: one persisted checkpoint per reason, owned by a non-active thread.
    let dir = tempfile::tempdir().expect("tempdir");
    let config = StorageConfig {
        db_path: dir.path().join("ledger.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage");
    let mut bridge = StorageBridge::new(storage.handle(), "session");
    let events: Vec<_> = [
        CompactionReason::Automatic,
        CompactionReason::Manual,
        CompactionReason::Agent,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, reason)| {
        let mut event = Event::new(CompactionEvent::Compacted {
            run_id: "run-1".into(),
            reason,
            threshold: 0.8,
            context_window_tokens: 200_000,
            estimated_tokens_before: 180_000,
            estimated_tokens_after: 60_000,
            compacted_range_start: index,
            compacted_range_end: index + 42,
            checkpoint_id: format!("checkpoint-{index}"),
            summary: "圧縮要約: 次の作業を継続します。".into(),
        });
        // Reverse timestamps ensure replay follows the ledger ID, not the clock.
        event.meta.wall_clock = std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(3 - u64::try_from(index).expect("index"));
        event
    })
    .collect();
    for event in &events {
        bridge.handle_event(event).expect("append");
    }
    drop(bridge);
    drop(storage);
    let db = Database::open(&config).expect("reopen");
    let stored = db.events_all_ordered().expect("ordered events");
    let mut registry = TranscriptRegistry::new();
    registry.bind_run("run-1", "thread-1");
    registry.select_thread(Some("other-thread".into()));

    // When: the same stored list is replayed twice into a fresh registry.
    for _ in 0..2 {
        for event in &stored {
            registry.apply(&event.event);
        }
        // Then: both projections contain exactly one entry per checkpoint.
        assert!(registry.thread().entries().is_empty());
        registry.select_thread(Some("thread-1".into()));
        assert_eq!(registry.thread().entries().len(), 3);
        assert_eq!(registry.run("run-1").expect("run").entries().len(), 3);
        assert_eq!(
            registry.thread().entries(),
            registry.run("run-1").expect("run").entries()
        );
        registry.select_thread(Some("other-thread".into()));
    }
    assert_eq!(
        stored
            .iter()
            .map(|row| row.event.clone())
            .collect::<Vec<_>>(),
        events
    );
    assert!(stored.windows(2).all(|pair| pair[0].id < pair[1].id));
    registry.select_thread(Some("thread-1".into()));
    let mut harness = Harness::builder()
        .with_size(egui::vec2(768.0, 600.0))
        .build_ui(|ui| {
            gui::theme::install(ui.ctx());
            gui::panes::agent_transcript::agent_transcript_pane(
                ui,
                "thread-1",
                Some(registry.thread()),
            );
        });
    harness.run_steps(2);
    let mut previous_bottom = 0.0;
    for (index, reason) in ["automatic", "manual", "agent"].into_iter().enumerate() {
        let expected_label = format!("Compaction ({reason}): 180000 → 60000 tokens");
        let label = harness
            .query_by_label_contains(&expected_label)
            .expect("reason and token transition");
        assert!(label.rect().top() > previous_bottom);
        previous_bottom = label.rect().bottom();
        assert!(
            harness
                .query_by_label_contains(&format!("range {index}..{}", index + 42))
                .is_some()
        );
    }
    if let Some(directory) = std::env::var_os("COMPACTION_EVIDENCE_DIR") {
        harness
            .render()
            .expect("render")
            .save(std::path::PathBuf::from(directory).join("compaction.png"))
            .expect("save");
    }
}
