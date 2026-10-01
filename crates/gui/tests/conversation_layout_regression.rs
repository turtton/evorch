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
        let info = harness.get_by_label("ℹ").rect();
        let title = harness
            .get_by_label("Thread: A conversation with a fairly long title")
            .rect();
        assert!(
            info.right() < title.left()
                && (info.center().y - title.center().y).abs() < title.height(),
            "info and title should share a row at width {width}: {info:?}, {title:?}"
        );
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
                category: None,
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
fn multiline_composer_grows_upward_and_shrinks_without_covering_the_transcript() {
    let mut model = TranscriptModel::new();
    model.apply(&Event::new(event_bus::MessageEvent::MessageDelta {
        delta: "AI response".into(),
        run_id: None,
    }));
    let mut harness = Harness::builder()
        .with_size(egui::vec2(800.0, 600.0))
        .build_ui_state(
            move |ui, state: &mut (ComposerModel, ModelPickerState)| {
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
                        active_thread_title: Some("Chat"),
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
                    input: "first".into(),
                    ..Default::default()
                },
                ModelPickerState::default(),
            ),
        );
    harness.run_steps(4);
    let one_line = harness.get_by_label("Message or /command").rect();
    harness.state_mut().0.input = "first\nsecond\nthird\nfourth\nfifth".into();
    harness.run_steps(4);
    let many_lines = harness.get_by_label("Message or /command").rect();
    let reply = harness.get_by_label("AI response").rect();
    assert!(
        many_lines.top() < one_line.top(),
        "{many_lines:?} vs {one_line:?}"
    );
    // Compare growth and visible bounds, not the font-dependent bottom of the
    // text document (which can be shorter than its reserved scroll viewport).
    assert!(harness.get_by_label("Send").rect().bottom() <= 600.0);
    assert!(many_lines.bottom() <= 600.0);
    assert!(many_lines.height() > one_line.height());
    assert!(
        reply.bottom() <= many_lines.top(),
        "{reply:?} overlaps {many_lines:?}"
    );
    harness.state_mut().0.input = "short".into();
    harness.run_steps(4);
    assert!(harness.get_by_label("Message or /command").rect().top() > many_lines.top());
}

#[test]
fn subagent_cards_show_category_and_model_provider_on_one_line() {
    let mut tasks = gui::model::tasks::TasksModel::new(Source(vec![runtime::AgentSummary {
        run_id: runtime::RunId::new(2),
        parent_run_id: Some(runtime::RunId::new(1)),
        name: "child".into(),
        role_name: "Worker".into(),
        phase: event_bus::AgentRunPhase::Done,
        model: "local/worker-model".into(),
        category: Some("plan".into()),
    }]));
    tasks.refresh();
    let mut harness = Harness::builder().build_ui(move |ui| {
        gui::panes::agents::subagents_pane(
            ui,
            &tasks,
            &Default::default(),
            &Default::default(),
            &["run-2".into()],
        );
    });
    harness.run_steps(2);
    let role = harness.get_by_label("Worker(plan) · Done").rect();
    let model = harness.get_by_label("local/worker-model · local").rect();
    assert!((role.center().y - model.center().y).abs() < role.height());
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
