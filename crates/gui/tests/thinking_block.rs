use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent};
use gui::model::transcript::TranscriptModel;

const THOUGHT: &str = "Checking the constraints before choosing an approach.";

fn reasoning(model: &mut TranscriptModel, run: &str, text: &str) {
    model.apply(&Event::new(MessageEvent::ReasoningDelta {
        run_id: Some(run.into()),
        delta: text.into(),
    }));
}

fn answer(model: &mut TranscriptModel, run: &str) {
    model.apply(&Event::new(MessageEvent::MessageDelta {
        run_id: Some(run.into()),
        delta: "Here is the final answer.".into(),
    }));
}

fn harness(model: TranscriptModel) -> Harness<'static, TranscriptModel> {
    Harness::builder()
        .with_size(egui::vec2(640.0, 320.0))
        .build_ui_state(
            |ui, model| {
                gui::theme::install(ui.ctx());
                gui::panes::agent::transcript_body(ui, model);
            },
            model,
        )
}

#[test]
fn thinking_expands_when_reasoning_streams() {
    // Given: a live reasoning delta.
    let mut model = TranscriptModel::new();
    reasoning(&mut model, "run-1", THOUGHT);
    // When: the actual transcript pane renders.
    let mut pane = harness(model);
    pane.run_steps(3);
    // Then: the thinking header and body are visible.
    assert!(pane.query_by_label("thinking").is_some());
    assert!(pane.query_by_label(THOUGHT).is_some());
    if let Some(dir) = std::env::var_os("THINKING_EVIDENCE_DIR") {
        pane.render()
            .unwrap()
            .save(std::path::PathBuf::from(dir).join("thinking-streaming.png"))
            .unwrap();
    }
}

#[test]
fn thinking_collapses_when_following_answer_arrives() {
    // Given: reasoning already displayed expanded.
    let mut model = TranscriptModel::new();
    reasoning(&mut model, "run-1", THOUGHT);
    let mut pane = harness(model);
    pane.run_steps(3);
    // When: the same run starts its answer.
    answer(pane.state_mut(), "run-1");
    pane.run_steps(3);
    // Then: reasoning is hidden while the answer remains visible.
    assert!(pane.query_by_label("thinking").is_some());
    assert!(pane.query_by_label(THOUGHT).is_none());
    assert!(pane.query_by_label("Here is the final answer.").is_some());
    if let Some(dir) = std::env::var_os("THINKING_EVIDENCE_DIR") {
        pane.render()
            .unwrap()
            .save(std::path::PathBuf::from(dir).join("thinking-completed.png"))
            .unwrap();
    }
}

#[test]
fn thinking_toggles_when_user_clicks_completed_header() {
    // Given: completed reasoning.
    let mut model = TranscriptModel::new();
    reasoning(&mut model, "run-1", THOUGHT);
    answer(&mut model, "run-1");
    let mut pane = harness(model);
    pane.run_steps(3);
    // When: the user expands the header.
    pane.get_by_label("thinking").click();
    pane.run_steps(3);
    // Then: the body is visible and stays open across frames.
    assert!(pane.query_by_label(THOUGHT).is_some());
    pane.run_steps(3);
    assert!(pane.query_by_label(THOUGHT).is_some());
}

#[test]
fn thinking_collapses_when_run_finishes_without_answer() {
    // Given: live reasoning.
    let mut model = TranscriptModel::new();
    reasoning(&mut model, "run-1", THOUGHT);
    let mut pane = harness(model);
    pane.run_steps(3);
    // When: the run completes without a message.
    pane.state_mut()
        .apply(&Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        }));
    pane.run_steps(3);
    // Then: only the header remains.
    assert!(pane.query_by_label("thinking").is_some());
    assert!(pane.query_by_label(THOUGHT).is_none());
}

#[test]
fn thinking_is_collapsed_when_loaded_as_history() {
    // Given: a historical reasoning entry, not a live delta.
    let mut model = TranscriptModel::new();
    model.push_reasoning(THOUGHT);
    // When: the pane first renders.
    let mut pane = harness(model);
    pane.run_steps(3);
    // Then: historical reasoning starts collapsed.
    assert!(pane.query_by_label("thinking").is_some());
    assert!(pane.query_by_label(THOUGHT).is_none());
}

#[test]
fn thinking_stays_closed_when_more_deltas_arrive_after_manual_collapse() {
    // Given: live reasoning manually collapsed by its reader.
    let mut model = TranscriptModel::new();
    reasoning(&mut model, "run-1", THOUGHT);
    let mut pane = harness(model);
    pane.run_steps(3);
    pane.get_by_label("thinking").click();
    pane.run_steps(3);
    // When: another delta extends the same entry.
    reasoning(pane.state_mut(), "run-1", " More details.");
    pane.run_steps(3);
    // Then: the user's collapsed state survives coalescing.
    assert!(
        pane.query_by_label(&format!("{THOUGHT} More details."))
            .is_none()
    );
    assert_eq!(pane.state().entries().len(), 1);
}

#[test]
fn thinking_completion_is_scoped_to_its_run_through_registry() {
    // Given: concurrent runs with separate live reasoning.
    let mut registry = gui::model::transcript_registry::TranscriptRegistry::new();
    for run in ["run-1", "run-2"] {
        registry.apply(&Event::new(MessageEvent::ReasoningDelta {
            run_id: Some(run.into()),
            delta: THOUGHT.into(),
        }));
    }
    // When: one run finishes without an answer.
    registry.apply(&Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: "run-1".into(),
        from: AgentRunPhase::Running,
        to: AgentRunPhase::Done,
        reason: None,
    }));
    // Then: both the thread and run projections close only that run.
    assert!(!registry.thread().thinking_is_streaming(0));
    assert!(registry.thread().thinking_is_streaming(1));
    assert!(!registry.run("run-1").unwrap().thinking_is_streaming(0));
    assert!(registry.run("run-2").unwrap().thinking_is_streaming(0));
}

#[test]
fn thinking_keeps_expansion_when_visible_window_changes() {
    // Given: a manually expanded completed block following another entry.
    let mut model = TranscriptModel::with_capacity(3);
    model.push_notice("earlier");
    reasoning(&mut model, "run-1", THOUGHT);
    answer(&mut model, "run-1");
    let mut pane = harness(model);
    pane.run_steps(3);
    pane.get_by_label("thinking").click();
    pane.run_steps(3);
    // When: capacity eviction shifts its array index.
    pane.state_mut().push_notice("later");
    pane.run_steps(3);
    // Then: the same block keeps its expanded state.
    assert!(pane.query_by_label(THOUGHT).is_some());
}
