//! The composer input keeps priority over the goal/procedure band: it grows
//! with typed and soft-wrapped lines and stays fully visible above the status
//! strip even when expanded band details compete for a short pane.
use egui::Rect;
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, ThreadGoalPhase, ThreadGoalSnapshot, ThreadTodoSnapshot};
use gui::model::{
    composer::ComposerModel, model_picker::ModelPickerState, transcript::TranscriptModel,
};

const INPUT: &str = "Message or /command";
const STATUS: &str = "wall 0s";

fn goal() -> ThreadGoalSnapshot {
    ThreadGoalSnapshot {
        related_root_run_ids: Vec::new(),
        max_tokens: None,
        goal_id: "goal-A".into(),
        thread_id: "A".into(),
        root_run_id: "run-1".into(),
        objective: "Investigate the slow operation".into(),
        original_request: "Explain the cause".into(),
        criteria: vec!["Cause supported by observations".into()],
        checks: Vec::new(),
        phase: ThreadGoalPhase::Working,
        review_enabled: false,
        checks_paused: false,
        work_stopped: false,
        epoch: 1,
        review_round: 0,
        findings: Vec::new(),
        reason: None,
        usage: Default::default(),
        max_review_rounds: 3,
    }
}

fn todo() -> ThreadTodoSnapshot {
    use event_bus::{ThreadTodoItem, ThreadTodoStatus::*};
    ThreadTodoSnapshot {
        list_id: "procedures-A".into(),
        thread_id: "A".into(),
        revision: 1,
        items: (0..12)
            .map(|index| ThreadTodoItem {
                content: format!("Procedure step {index}"),
                status: match index {
                    0..=2 => Completed,
                    3 => InProgress,
                    _ => Pending,
                },
            })
            .collect(),
    }
}

type State = (ComposerModel, ModelPickerState);

fn harness(size: egui::Vec2, input: &str) -> Harness<'static, State> {
    let mut transcript = TranscriptModel::new();
    transcript.apply(&Event::new(event_bus::MessageEvent::MessageDelta {
        delta: "AI response".into(),
        run_id: None,
    }));
    let (goal, todo) = (goal(), todo());
    Harness::builder().with_size(size).build_ui_state(
        move |ui, state: &mut State| {
            gui::theme::install(ui.ctx());
            gui::panes::agent::agent_pane(
                ui,
                &transcript,
                None,
                gui::panes::agent::ConversationContext {
                    todo: Some(&todo),
                    goal: Some(&goal),
                    requests: None,
                    task_rows: &[],
                    phase_unread: false,
                    has_project: true,
                    active_thread_title: Some("Chat"),
                    parent_thread: None,
                    child_threads: Vec::new(),
                    thread_metrics: None,
                    phase: None,
                    next_thread_title: String::new(),
                    model_picker: gui::panes::model_picker::ModelPickerContext {
                        profiles: &[],
                        preference: None,
                        default_model: None,
                        enabled: false,
                    },
                    sandbox_picker: Default::default(),
                    branch: None,
                },
                &mut state.0,
                &mut state.1,
            );
        },
        (
            ComposerModel {
                input: input.into(),
                ..Default::default()
            },
            ModelPickerState::default(),
        ),
    )
}

fn rect(harness: &Harness<'_, State>, label: &str) -> Rect {
    harness.get_by_label(label).rect()
}

/// The whole text document sits between the band and the status strip, so no
/// typed row is clipped or hidden behind a scroll viewport.
fn assert_input_fully_visible(harness: &Harness<'_, State>, height: f32, context: &str) {
    let input = rect(harness, INPUT);
    let status = rect(harness, STATUS);
    let band = rect(harness, "Procedures: Procedure step 3");
    let reply = rect(harness, "AI response");
    assert!(
        band.bottom() <= input.top() + 0.5,
        "{context}: band {band:?} overlaps input {input:?}"
    );
    assert!(
        input.bottom() <= status.top() + 0.5,
        "{context}: input {input:?} runs under the status strip {status:?}"
    );
    assert!(
        status.bottom() <= height,
        "{context}: status strip clipped: {status:?}"
    );
    assert!(
        rect(harness, "Send").bottom() <= height,
        "{context}: Send clipped"
    );
    assert!(
        reply.bottom() <= band.top(),
        "{context}: transcript {reply:?} covered by {band:?}"
    );
}

#[test]
fn input_grows_with_lines_while_todo_band_is_visible() {
    let size = egui::vec2(800.0, 400.0);
    let mut harness = harness(size, "first");
    harness.run();
    let one_line = rect(&harness, INPUT);
    harness.state_mut().0.input = "first\nsecond\nthird\nfourth\nfifth\nsixth".into();
    harness.run();
    let six_lines = rect(&harness, INPUT);
    assert!(
        six_lines.height() > one_line.height() * 3.0,
        "{six_lines:?} vs {one_line:?}"
    );
    assert!(six_lines.top() < one_line.top());
    assert_input_fully_visible(&harness, size.y, "collapsed band");
}

#[test]
fn input_keeps_priority_over_expanded_procedure_details_in_a_short_pane() {
    let size = egui::vec2(800.0, 400.0);
    let mut harness = harness(size, "first");
    harness.run();
    let one_line = rect(&harness, INPUT);
    harness.get_by_label("Show procedure details").click();
    harness.run();
    harness.get_by_label("Procedure step 0");
    harness.state_mut().0.input = "first\nsecond\nthird\nfourth\nfifth\nsixth".into();
    harness.run();
    let six_lines = rect(&harness, INPUT);
    assert!(
        six_lines.height() > one_line.height() * 3.0,
        "{six_lines:?} vs {one_line:?}"
    );
    assert_input_fully_visible(&harness, size.y, "expanded details");
    harness.get_by_label("Hide procedure details");
    harness.state_mut().0.input = "short".into();
    harness.run();
    assert!(rect(&harness, INPUT).top() > six_lines.top());
    assert_input_fully_visible(&harness, size.y, "after shrinking");
}

#[test]
fn soft_wrapped_input_grows_instead_of_scrolling() {
    let size = egui::vec2(800.0, 500.0);
    let mut harness = harness(size, "short");
    harness.run();
    let one_line = rect(&harness, INPUT);
    harness.state_mut().0.input = "wrapping words ".repeat(40);
    harness.run();
    let wrapped = rect(&harness, INPUT);
    assert!(
        wrapped.height() > one_line.height() * 2.0,
        "{wrapped:?} vs {one_line:?}"
    );
    assert_input_fully_visible(&harness, size.y, "soft-wrapped line");
}

#[test]
fn typed_newlines_resize_without_waiting_for_another_event() {
    let size = egui::vec2(800.0, 400.0);
    let mut harness = harness(size, "first");
    harness.get_by_label(INPUT).focus();
    harness.run();
    let one_line = rect(&harness, INPUT);
    for _ in 0..4 {
        harness.key_press_modifiers(egui::Modifiers::SHIFT, egui::Key::Enter);
        harness.run();
    }
    assert_eq!(harness.state().0.input.matches('\n').count(), 4);
    assert!(rect(&harness, INPUT).height() > one_line.height() * 2.0);
    assert_input_fully_visible(&harness, size.y, "typed newlines");
}
