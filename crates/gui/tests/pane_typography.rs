use egui::{FontFamily, FontId, epaint::Shape};
use egui_kittest::{Harness, kittest::Queryable};
use gui::theme::tokens::{FONT_H3, FONT_H4};

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
fn sidebar_header_uses_h4_when_project_is_selected() {
    // Given: a real sidebar with its selected demo project.
    let dir = tempfile::tempdir().unwrap();
    let sidebar = gui::fixture::demo_sidebar(dir.path()).unwrap();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 600.0))
        .build_ui(|ui| {
            gui::theme::install(ui.ctx());
            gui::panes::sidebar::sidebar_pane(
                ui,
                &sidebar,
                &Default::default(),
                &gui::model::telemetry::TelemetryOverlay::new(),
            );
        });
    // When: the sidebar renders.
    harness.run_steps(2);
    // Then: its compact title retains the h4 proportional role.
    assert_header(&harness, "Threads", FONT_H4);
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
    assert_header(&harness, "Transcript: run-1", FONT_H3);
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
                    task_rows: &[],
                    phase_unread: false,
                    has_project: true,
                    active_thread_title: Some("Chat"),
                    thread_metrics: None,
                    phase: None,
                    next_thread_title: String::new(),
                    model_picker: gui::panes::model_picker::ModelPickerContext {
                        profiles: &[],
                        preference: None,
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
    assert_header(&harness, "run-1 / Worker / explorer", FONT_H3);
}

#[test]
fn arena_header_uses_h3_when_storage_is_connected() {
    // Given: a real evaluation pane backed by an isolated database.
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("arena.db"),
        ..Default::default()
    };
    let mut pane = gui::panes::arena::ArenaPane::default();
    let mut harness = Harness::new_ui(|ui| {
        gui::theme::install(ui.ctx());
        pane.render(ui, Some((&config, "project")));
    });
    // When: the pane renders.
    harness.run_steps(2);
    // Then: the workbench title uses the same h3 role as conversation.
    assert_header(&harness, "Role evaluation arena", FONT_H3);
}
