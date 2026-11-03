use event_bus::{
    CompactionEvent, CompactionMode, CompactionReason, Event, LifecycleEvent, RunActivity,
};
use gui::model::transcript::{TranscriptEntry, TranscriptModel};
fn start(run: &str, operation: &str) -> Event {
    Event::new(LifecycleEvent::RunProgress {
        run_id: run.into(),
        activity: RunActivity::Compaction,
        compaction_operation_id: Some(operation.into()),
        context: None,
    })
}
fn progress(
    run: &str,
    operation: &str,
    mode: CompactionMode,
    attempt: u64,
    revision: u64,
    text: &str,
) -> Event {
    Event::new(CompactionEvent::Progress {
        run_id: run.into(),
        operation_id: operation.into(),
        mode,
        attempt,
        revision,
        delta: text.into(),
    })
}
fn completed(operation: &str) -> Event {
    Event::new(CompactionEvent::Compacted {
        run_id: "run".into(),
        operation_id: operation.into(),
        reason: CompactionReason::Manual,
        threshold: 0.8,
        context_window_tokens: 1000,
        window_source: Default::default(),
        estimated_tokens_before: 900,
        estimated_tokens_after: 100,
        compacted_range_start: 1,
        compacted_range_end: 4,
        checkpoint_id: operation.into(),
        summary: "accepted summary".into(),
    })
}
#[test]
fn only_committed_compaction_replaces_the_live_card_in_place() {
    let mut model = TranscriptModel::new();
    model.apply(&start("run", "op"));
    model.apply(&progress(
        "run",
        "op",
        CompactionMode::ModelSummary,
        1,
        1,
        "summary in progress",
    ));
    assert!(
        matches!(&model.entries()[0], TranscriptEntry::CompactionProgress { progress } if progress.active && progress.text == "summary in progress")
    );
    model.apply(&completed("op"));
    model.apply(&start("run", "op"));
    model.apply(&progress(
        "run",
        "op",
        CompactionMode::ModelSummary,
        1,
        2,
        "stale",
    ));
    assert!(
        matches!(model.entries(), [TranscriptEntry::Compaction { summary, .. }] if summary == "accepted summary")
    );
}
#[test]
fn retries_stale_revisions_parallel_runs_and_aborts_are_isolated() {
    let mut model = TranscriptModel::new();
    model.apply(&start("run", "op"));
    model.apply(&start("other", "parallel"));
    model.apply(&progress(
        "run",
        "op",
        CompactionMode::ModelSummary,
        1,
        1,
        "old attempt",
    ));
    model.apply(&progress(
        "run",
        "op",
        CompactionMode::ModelSummary,
        2,
        1,
        "new attempt",
    ));
    model.apply(&progress(
        "run",
        "op",
        CompactionMode::ModelSummary,
        1,
        2,
        "stale attempt",
    ));
    model.apply(&progress(
        "run",
        "op",
        CompactionMode::ModelSummary,
        2,
        1,
        "duplicate",
    ));
    assert!(
        matches!(&model.entries()[0], TranscriptEntry::CompactionProgress { progress } if progress.text == "new attempt")
    );
    model.apply(&Event::new(CompactionEvent::Aborted {
        run_id: "run".into(),
        operation_id: "op".into(),
    }));
    model.apply(&start("run", "op"));
    assert_eq!(model.entries().len(), 2);
    assert!(
        matches!(&model.entries()[0], TranscriptEntry::CompactionProgress { progress } if !progress.active && progress.interrupted)
    );
    assert!(
        matches!(&model.entries()[1], TranscriptEntry::CompactionProgress { progress } if progress.active)
    );
}
#[test]
fn only_model_summary_text_is_shown_and_split_secrets_are_redacted() {
    let mut model = TranscriptModel::new();
    model.apply(&start("run", "op"));
    model.apply(&progress(
        "run",
        "op",
        CompactionMode::Official,
        1,
        1,
        "encrypted blob",
    ));
    assert!(
        matches!(&model.entries()[0], TranscriptEntry::CompactionProgress { progress } if progress.text.is_empty())
    );
    model.apply(&progress(
        "run",
        "op",
        CompactionMode::ModelSummary,
        2,
        1,
        "key=AKIAIOSFODNN",
    ));
    model.apply(&progress(
        "run",
        "op",
        CompactionMode::ModelSummary,
        2,
        2,
        "7EXAMPLE",
    ));
    assert!(
        matches!(&model.entries()[0], TranscriptEntry::CompactionProgress { progress } if !progress.text.contains("AKIAIOSFODNN7EXAMPLE") && progress.text.contains("REDACTED"))
    );
    model.apply(&progress(
        "run",
        "op",
        CompactionMode::Structural,
        3,
        1,
        "not displayed",
    ));
    assert!(
        matches!(&model.entries()[0], TranscriptEntry::CompactionProgress { progress } if progress.text.is_empty())
    );
}
#[test]
fn a_missing_revision_stops_the_spinner_without_displaying_partial_secrets() {
    let mut model = TranscriptModel::new();
    model.apply(&start("run", "op"));
    model.apply(&progress(
        "run",
        "op",
        CompactionMode::ModelSummary,
        1,
        3,
        "fragment after loss",
    ));
    assert!(
        matches!(&model.entries()[0], TranscriptEntry::CompactionProgress { progress } if !progress.active && progress.text.is_empty())
    );
}

#[test]
fn dedicated_card_renders_progress_then_one_completed_entry() {
    use egui_kittest::{Harness, kittest::Queryable};
    let mut model = TranscriptModel::new();
    model.apply(&start("run", "op"));
    model.apply(&progress(
        "run",
        "op",
        CompactionMode::ModelSummary,
        1,
        1,
        "live summary preview",
    ));
    let mut harness = Harness::builder()
        .with_size(egui::vec2(768.0, 600.0))
        .build_ui(|ui| {
            gui::theme::install(ui.ctx());
            gui::panes::agent_transcript::agent_transcript_pane(ui, "run", Some(&model));
        });
    harness.run_steps(2);
    assert!(harness.query_by_label("Compacting context…").is_some());
    assert!(harness.query_by_label("live summary preview").is_some());
}

#[test]
fn ownership_loss_settles_saved_progress_in_only_its_thread_and_run() {
    use gui::model::transcript_registry::TranscriptRegistry;
    let mut registry = TranscriptRegistry::new();
    registry.bind_thread_root("thread-a", "run-a");
    registry.bind_thread_root("thread-b", "run-b");
    registry.apply(&start("run-a", "op-a"));
    registry.apply(&start("run-b", "op-b"));
    registry.apply(&Event::new(event_bus::OwnershipEvent {
        thread_id: "thread-a".into(),
        owner_id: "old".into(),
        generation: 1,
        action: event_bus::OwnershipAction::Released,
    }));
    assert!(
        matches!(&registry.run("run-a").unwrap().entries()[0], TranscriptEntry::CompactionProgress { progress } if !progress.active)
    );
    assert!(
        matches!(&registry.run("run-b").unwrap().entries()[0], TranscriptEntry::CompactionProgress { progress } if progress.active)
    );
    registry.select_thread(Some("thread-a".into()));
    assert!(
        matches!(&registry.thread().entries()[0], TranscriptEntry::CompactionProgress { progress } if !progress.active)
    );
}
