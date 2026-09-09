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
fn agents_grid_fits_nine_columns_without_horizontal_scroll() {
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
        .with_size(vec2(480.0, 320.0))
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
            rect.max.x <= 480.0 + 0.5,
            "{header} overflows right edge: {rect:?}"
        );
        assert!(rect.min.x >= 0.0, "{header} leaks left: {rect:?}");
        if let Some(prev) = last_min_x {
            assert!(
                rect.min.x > prev,
                "{header} not strictly after previous column"
            );
        }
        last_min_x = Some(rect.min.x);
    }

    let tokens = harness
        .query_by_label("120 / 34")
        .expect("tokens label missing");
    let rect = tokens.rect();
    assert!(rect.max.x <= 480.0 + 0.5, "tokens overflow: {rect:?}");
    assert!(rect.min.x >= 0.0, "tokens leak left: {rect:?}");
}

#[test]
fn agents_grid_respects_clip_rect_when_parent_scrolls_horizontally() {
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
        .with_size(vec2(340.0, 320.0))
        .build_ui_state(
            |ui, state: &mut (TasksModel<MockSource>, TelemetryOverlay)| {
                egui::ScrollArea::horizontal().show(ui, |ui| {
                    gui::panes::agents::agents_pane(ui, &state.0, &state.1);
                });
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
    for header in headers {
        let rect = harness.get_by_label(header).rect();
        assert!(
            rect.max.x <= 340.0 + 1.0,
            "{header} overflows 340px clip: {rect:?}"
        );
        assert!(rect.min.x >= 0.0, "{header} leaks left: {rect:?}");
    }

    let tokens = harness
        .query_by_label("120 / 34")
        .expect("tokens label missing in 340px clip");
    let rect = tokens.rect();
    assert!(
        rect.max.x <= 340.0 + 1.0,
        "tokens overflow 340px clip: {rect:?}"
    );
    assert!(rect.min.x >= 0.0, "tokens leak left: {rect:?}");
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
