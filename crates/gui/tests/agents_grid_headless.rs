use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, ProviderEvent, ToolEvent};
use gui::model::tasks::{AgentRunSource, TasksModel};
use gui::model::telemetry::TelemetryOverlay;
use runtime::{AgentSummary, RunId};

#[derive(Clone)]
struct MockSource(Vec<AgentSummary>);

impl AgentRunSource for MockSource {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

#[test]
fn agents_grid_keeps_all_columns_when_wide() {
    let mut tasks = TasksModel::new(MockSource(vec![long_summary()]));
    tasks.refresh();
    let mut telemetry = TelemetryOverlay::new();
    telemetry.apply_event(&request_started(
        "run-1",
        "openai-compatible-local-gateway",
        "claude-sonnet-4-5-20250929-extended-thinking",
    ));
    telemetry.apply_event(&request_completed("run-1", 120, 34));
    telemetry.apply_event(&tool_started(
        "run-1",
        "read_file_with_an_extremely_long_tool_name",
    ));

    let mut harness = Harness::builder()
        .with_size(vec2(2400.0, 320.0))
        .build_ui_state(
            |ui, state: &mut (TasksModel<MockSource>, TelemetryOverlay)| {
                gui::panes::agents::agents_pane(ui, &state.0, &state.1);
            },
            (tasks, telemetry),
        );
    harness.run();

    let headers = [
        "run",
        "name",
        "role",
        "phase",
        "model",
        "provider",
        "current tool",
        "tokens (in/out)",
    ];
    let mut last_min_x: Option<f32> = None;
    for header in headers {
        let rect = harness.get_by_label(header).rect();
        assert!(
            rect.max.x <= 2400.0 + 0.5,
            "{header} overflows right edge: {rect:?}"
        );
        assert!(rect.min.x >= 0.0, "{header} leaks left: {rect:?}");
        if let Some(prev) = last_min_x {
            assert_ne!(rect.min.x, prev, "columns must have distinct positions");
        }
        last_min_x = Some(rect.min.x);
    }

    let tokens = harness
        .query_by_label("120 / 34")
        .expect("tokens label missing");
    let rect = tokens.rect();
    assert!(rect.max.x <= 2400.0 + 0.5, "tokens overflow: {rect:?}");
    assert!(rect.min.x >= 0.0, "tokens leak left: {rect:?}");
}

#[test]
fn agents_grid_prioritizes_identity_when_default_right_pane_is_narrow() {
    // Given: Agents gets 37.5% after the 20% sidebar; also cover tighter dock constraints.
    for width in [1280.0 * 0.8 * 0.375, 320.0] {
        let mut tasks = TasksModel::new(MockSource(vec![long_summary()]));
        tasks.refresh();
        let mut telemetry = TelemetryOverlay::new();
        telemetry.apply_event(&request_started(
            "run-1",
            "openai-compatible-local-gateway",
            "claude-sonnet-4-5-20250929-extended-thinking",
        ));
        telemetry.apply_event(&request_completed("run-1", 120, 34));
        telemetry.apply_event(&tool_started(
            "run-1",
            "read_file_with_an_extremely_long_tool_name",
        ));

        let mut harness = Harness::builder()
            .with_size(vec2(width, 320.0))
            .build_ui_state(
                |ui, state: &mut (TasksModel<MockSource>, TelemetryOverlay)| {
                    gui::theme::install(ui.ctx());
                    egui::ScrollArea::horizontal().show(ui, |ui| {
                        gui::panes::agents::agents_pane(ui, &state.0, &state.1);
                    });
                },
                (tasks, telemetry),
            );
        // When: the table is painted at its initial horizontal scroll position.
        harness.run();

        // Then: priority headers and status are painted in full, not just accessible labels.
        for label in ["name", "phase", "Running"] {
            let text = harness
                .output()
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::epaint::Shape::Text(text) if text.galley.text() == label => Some(text),
                    _ => None,
                })
                .expect("priority text is painted");
            assert!(!text.galley.elided, "{label} is ellipsized");
            assert!(
                text.pos.x + text.galley.size().x <= width,
                "{label} clipped"
            );
        }
        assert!(harness.get_by_label("tokens (in/out)").rect().left() > width);
    }
}

#[test]
fn agents_grid_telemetry_is_reachable_when_scrolled() {
    // Given: a narrow table with usage beyond the priority columns.
    let mut tasks = TasksModel::new(MockSource(vec![long_summary()]));
    tasks.refresh();
    let mut telemetry = TelemetryOverlay::new();
    telemetry.apply_event(&request_completed("run-1", 120, 34));
    let mut harness = Harness::builder()
        .with_size(vec2(480.0, 320.0))
        .build_ui(|ui| {
            gui::theme::install(ui.ctx());
            gui::panes::agents::agents_pane(ui, &tasks, &telemetry);
        });
    harness.run();
    // When: scrolling the last column into view through accessibility.
    harness.get_by_label("120 / 34").scroll_to_me();
    harness.run();
    // Then: the actual usage is fully reachable inside the pane.
    let rect = harness.get_by_label("120 / 34").rect();
    assert!(rect.left() >= 0.0 && rect.right() <= 480.0, "{rect:?}");
}

fn long_summary() -> AgentSummary {
    AgentSummary {
        run_id: RunId::new(1),
        name: "worker-with-a-really-long-agent-name".into(),
        role_name: "reviewer-with-long-role".into(),
        phase: AgentRunPhase::Running,
        model: "task-model-1".into(),
    }
}

fn request_started(run_id: &str, provider: &str, model: &str) -> Event {
    Event::new(ProviderEvent::RequestStarted {
        request_id: format!("request-{run_id}"),
        provider: provider.into(),
        profile: None,
        protocol: "fixture".into(),
        model: model.into(),
        streaming: true,
        run_id: Some(run_id.into()),
    })
}

fn request_completed(run_id: &str, input_tokens: u64, output_tokens: u64) -> Event {
    Event::new(ProviderEvent::RequestCompleted {
        request_id: format!("request-{run_id}"),
        provider: "anthropic".into(),
        profile: None,
        protocol: "fixture".into(),
        model: "claude".into(),
        streaming: true,
        duration_ms: 10,
        input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        finish_reason: "stop".into(),
        run_id: Some(run_id.into()),
    })
}

fn tool_started(run_id: &str, tool_name: &str) -> Event {
    Event::new(ToolEvent::ToolStarted {
        input: None,
        tool_name: tool_name.into(),
        call_id: "call-1".into(),
        run_id: Some(run_id.into()),
    })
}
