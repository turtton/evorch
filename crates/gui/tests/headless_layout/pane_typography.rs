use egui::{FontFamily, FontId, epaint::Shape};
use egui_kittest::{Harness, kittest::Queryable};
use gui::theme::tokens::FONT_H3;

fn assert_header(harness: &Harness<'_, ()>, label: &str, size: f32) {
    harness.get_by_label(label);
    let text = harness
        .output()
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            Shape::Text(text) if text.galley.text() == label => Some(text),
            _ => None,
        })
        .expect("header is painted");
    assert!(!text.galley.job.sections.is_empty());
    for section in &text.galley.job.sections {
        assert_eq!(
            section.format.font_id,
            FontId::new(size, FontFamily::Proportional)
        );
    }
}

#[test]
fn transcript_header_uses_h3_when_run_is_selected() {
    // Given: a transcript pane for a selected run.
    let mut harness = Harness::new_ui(|ui| {
        gui::theme::install(ui.ctx());
        gui::panes::agent_transcript::agent_transcript_pane(ui, "run-1", None);
    });
    // When: the conversation transcript renders.
    harness.run_steps(2);
    // Then: the title uses the h3 proportional role, not code typography.
    assert_header(&harness, "Transcript", FONT_H3);
}

#[test]
fn team_header_uses_h3_when_no_team_has_started() {
    // Given: a workbench Team pane without tasks.
    let mut harness = Harness::new_ui(|ui| {
        gui::theme::install(ui.ctx());
        gui::panes::team::team_pane(ui, &[]);
    });
    // When: the pane renders.
    harness.run_steps(2);
    // Then: the pane title uses h3 rather than egui's Heading style.
    assert_header(&harness, "Team", FONT_H3);
}

#[test]
fn conversation_header_uses_h3_when_agent_is_selected() {
    // Given: the conversation pane displays an agent rather than the thread title.
    let model = gui::model::transcript::TranscriptModel::default();
    let mut composer = gui::model::composer::ComposerModel::default();
    let mut picker = gui::model::model_picker::ModelPickerState::default();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 600.0))
        .build_ui(|ui| {
            gui::theme::install(ui.ctx());
            gui::panes::agent::agent_pane(
                ui,
                &model,
                Some(gui::panes::agent::AgentIdentity {
                    run_id: "run-1",
                    name: Some("Worker"),
                    role: Some("explorer"),
                    ledger: &[],
                }),
                gui::panes::agent::ConversationContext {
                    todo: None,
                    goal: None,
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
                    sandbox_picker: Default::default(),
                    branch: None,
                    model_picker: gui::panes::model_picker::ModelPickerContext {
                        profiles: &[],
                        preference: None,
                        default_model: None,
                        enabled: false,
                    },
                },
                &mut composer,
                &mut picker,
            );
        });
    // When: the selected agent's conversation renders.
    harness.run_steps(2);
    // Then: switching from a thread to an agent preserves the title hierarchy.
    assert_header(&harness, "Worker / explorer", FONT_H3);
    // And: the run ID left the title but stays one click away.
    assert!(harness.query_by_label_contains("run-1").is_none());
    harness.get_by_label("Copy run ID").click();
    harness.step();
    assert!(
        harness.output().platform_output.commands.iter().any(
            |command| matches!(command, egui::OutputCommand::CopyText(text) if text == "run-1")
        )
    );
}
