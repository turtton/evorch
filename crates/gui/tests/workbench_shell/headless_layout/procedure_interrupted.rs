//! The runtime-set interrupted status reads as unfinished work: it is never
//! counted as completed, never shown as "All procedures completed", and the
//! summary picks in-progress, then interrupted, then pending.
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{ThreadTodoItem, ThreadTodoSnapshot, ThreadTodoStatus};
use gui::model::{
    composer::ComposerModel, model_picker::ModelPickerState, transcript::TranscriptModel,
};

type State = (ComposerModel, ModelPickerState);

fn todo(statuses: &[ThreadTodoStatus]) -> ThreadTodoSnapshot {
    ThreadTodoSnapshot {
        list_id: "procedures-A".into(),
        thread_id: "A".into(),
        revision: 2,
        items: statuses
            .iter()
            .enumerate()
            .map(|(index, status)| ThreadTodoItem {
                content: format!("Step {index}"),
                status: *status,
            })
            .collect(),
    }
}

fn harness(todo: ThreadTodoSnapshot) -> Harness<'static, State> {
    let transcript = TranscriptModel::new();
    Harness::builder()
        .with_size(egui::vec2(800.0, 400.0))
        .build_ui_state(
            move |ui, state: &mut State| {
                gui::theme::install(ui.ctx());
                gui::panes::agent::agent_pane(
                    ui,
                    &transcript,
                    None,
                    gui::panes::agent::ConversationContext {
                        todo: Some(&todo),
                        goal: None,
                        requests: None,
                        task_rows: &[],
                        phase_unread: false,
                        has_project: true,
                        active_thread_title: Some("Chat"),
                        extra_read_dirs: &[],
                        parent_thread: None,
                        child_threads: Vec::new(),
                        lane: None,
                        lead: None,
                        thread_metrics: None,
                        phase: None,
                        model_picker: gui::panes::model_picker::ModelPickerContext {
                            profiles: &[],
                            preference: None,
                            default_model: None,
                            enabled: false,
                        },
                        sandbox_picker: Default::default(),
                        branch: None,
                        pull_request: None,
                        pull_request_chips: None,
                    },
                    &mut state.0,
                    &mut state.1,
                );
            },
            (ComposerModel::default(), ModelPickerState::default()),
        )
}

fn summary(statuses: &[ThreadTodoStatus]) -> Harness<'static, State> {
    let mut harness = harness(todo(statuses));
    harness.run();
    harness
}

#[test]
fn interrupted_only_list_is_not_reported_as_completed() {
    use ThreadTodoStatus::*;
    let harness = summary(&[Interrupted, Interrupted]);
    harness.get_by_label(
        "Procedures: Step 0 · interrupted: a work period ended before completion was recorded",
    );
    harness.get_by_label("0/2");
    assert!(harness.query_by_label("All procedures completed").is_none());
    assert!(
        harness
            .query_by_label_contains("Procedures: All procedures completed")
            .is_none()
    );
    harness.get_by_label(
        "Procedures interrupted: a work period ended before completion was recorded. This does not mean another lane stopped",
    );
}

#[test]
fn summary_prefers_in_progress_then_interrupted_then_pending() {
    use ThreadTodoStatus::*;
    summary(&[Pending, Interrupted, InProgress]).get_by_label("Procedures: Step 2");
    let harness = summary(&[Completed, Pending, Interrupted]);
    harness.get_by_label_contains("Procedures: Step 2 · interrupted");
    summary(&[Completed, Pending]).get_by_label("Procedures: Step 1");
}

#[test]
fn only_completed_items_count_and_an_all_completed_list_reads_as_done() {
    use ThreadTodoStatus::*;
    let harness = summary(&[Completed, Interrupted, Completed]);
    harness.get_by_label("2/3");
    let harness = summary(&[Completed, Completed]);
    harness.get_by_label("2/2");
    harness.get_by_label("Procedures: All procedures completed");
    harness.get_by_label("Procedures completed");
}

#[test]
fn details_name_each_interrupted_item() {
    use ThreadTodoStatus::*;
    let mut harness = summary(&[Completed, Interrupted]);
    harness.get_by_label("Show procedure details").click();
    harness.run();
    harness.get_by_label("Step 1");
    assert!(
        harness
            .get_all_by_label("Interrupted (not completed)")
            .count()
            >= 1
    );
}
