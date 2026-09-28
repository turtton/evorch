use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{DiagnosticEvent, DiagnosticSeverity, Event};
use gui::model::{
    composer::ComposerModel, model_picker::ModelPickerState, transcript::TranscriptModel,
};

#[test]
fn narrow_conversation_keeps_message_input_and_send_inside_its_width() {
    for width in [240.0, 320.0, 480.0] {
        let mut model = TranscriptModel::new();
        model.apply(&Event::new(event_bus::MessageEvent::MessageDelta {
            delta: "A response with a long URL https://example.com/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa and ordinary words.".into(), run_id: None,
        }));
        let mut harness = Harness::builder()
            .with_size(egui::vec2(width, 600.0))
            .build_ui_state(
                |ui, state: &mut (ComposerModel, ModelPickerState)| {
                    gui::theme::install(ui.ctx());
                    gui::panes::agent::agent_pane(
                        ui,
                        &model,
                        None,
                        gui::panes::agent::ConversationContext {
                            requests: None,
                            task_rows: &[],
                            phase_unread: false,
                            has_project: true,
                            active_thread_title: Some("A conversation with a fairly long title"),
                            parent_thread: None,
                            child_threads: Vec::new(),
                            thread_metrics: None,
                            phase: None,
                            next_thread_title: String::new(),
                            model_picker: gui::panes::model_picker::ModelPickerContext {
                                profiles: &[],
                                preference: None,
                                enabled: false,
                            },
                            sandbox_picker: Default::default(),
                        },
                        &mut state.0,
                        &mut state.1,
                    );
                },
                (
                    ComposerModel {
                        input: "Unsent content".into(),
                        ..Default::default()
                    },
                    ModelPickerState::default(),
                ),
            );
        harness.run_steps(4);
        for label in ["Message or /command", "Send"] {
            let rect = harness.get_by_label(label).rect();
            assert!(
                rect.left() >= 0.0 && rect.right() <= width,
                "{label} outside width {width}: {rect:?}"
            );
        }
    }
}

#[test]
fn sandbox_review_ids_are_hidden_until_expanded_for_both_verdicts() {
    for (severity, verdict) in [
        (DiagnosticSeverity::Info, "approved"),
        (DiagnosticSeverity::Warning, "denied: prohibited command"),
    ] {
        let mut model = TranscriptModel::new();
        model.apply(&Event::new(DiagnosticEvent {
            source: "sandbox".into(),
            severity,
            code: "escalation_review".into(),
            detail: verdict.into(),
            run_id: Some("run-93".into()),
            call_id: Some("call-secret".into()),
            thread_id: None,
        }));
        let label = format!(
            "[{}] sandbox (escalation_review): {verdict}",
            severity.as_str()
        );
        let mut harness = Harness::builder()
            .with_size(egui::vec2(700.0, 300.0))
            .build_ui(|ui| {
                gui::panes::agent::transcript_body(ui, &model);
            });
        harness.run_steps(2);
        assert!(harness.query_by_label("run_id: run-93").is_none());
        assert!(harness.query_by_label("call_id: call-secret").is_none());
        harness.get_by_label(&label).click();
        harness.run_steps(3);
        harness.get_by_label("run_id: run-93");
        harness.get_by_label("call_id: call-secret");
    }
}

#[derive(Clone)]
struct Source(Vec<runtime::AgentSummary>);
impl gui::model::tasks::AgentRunSource for Source {
    fn list(&self) -> Vec<runtime::AgentSummary> {
        self.0.clone()
    }
}

#[test]
fn subagents_list_only_displays_delegated_runs_in_selected_thread() {
    let source = Source(
        (1..=3)
            .map(|id| runtime::AgentSummary {
                run_id: runtime::RunId::new(id),
                parent_run_id: (id > 1).then(|| runtime::RunId::new(1)),
                name: format!("Agent {id}"),
                role_name: "worker".into(),
                phase: event_bus::AgentRunPhase::Running,
                model: "gpt".into(),
            })
            .collect(),
    );
    let mut tasks = gui::model::tasks::TasksModel::new(source);
    tasks.refresh();
    let mut harness = Harness::builder().build_ui(|ui| {
        gui::panes::agents::subagents_pane(
            ui,
            &tasks,
            &Default::default(),
            &Default::default(),
            &["run-1".into(), "run-2".into()],
        );
    });
    harness.run_steps(2);
    harness.get_by_label("Agent 2");
    assert!(harness.query_by_label("Agent 1").is_none());
    assert!(harness.query_by_label("Agent 3").is_none());
}

#[test]
fn real_dock_conversation_and_footer_stay_inside_their_regions() {
    let mut state =
        gui::app::WorkbenchState::new(Source(Vec::new()), &workspace_ui::UiSettings::default())
            .unwrap();
    state.composer_mut().input = "Draft".into();
    let mut workbench = gui::headless::HeadlessWorkbench::new(state, [800.0, 700.0]);
    workbench.run();
    let path = workbench
        .state()
        .dock()
        .find_tab(&workspace_ui::PanelId::new("agent-main"))
        .unwrap();
    let viewport = workbench
        .state()
        .dock()
        .leaf(path.node_path())
        .unwrap()
        .viewport;
    for label in ["Message or /command", "Send"] {
        let rect = workbench.label_rects(label)[0];
        assert!(
            rect.left() >= viewport.left() && rect.right() <= viewport.right(),
            "{label}: {rect:?}, viewport {viewport:?}"
        );
    }
    let settings = workbench.label_rects("⚙")[0];
    let quota = workbench.label_rects("Codex · unavailable")[0];
    assert!(settings.bottom() > 650.0 && quota.bottom() > 650.0);
    assert!(settings.right() < quota.left());
}
