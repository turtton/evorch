use egui::epaint::Shape;
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, ToolEvent};
use gui::model::transcript::TranscriptModel;
use gui::model::transcript_registry::TranscriptRegistry;

fn started() -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: "read".into(),
        call_id: "pending".into(),
        input: Some(serde_json::json!({"file": "."})),
        run_id: Some("run-1".into()),
    })
}

fn completed() -> Event {
    Event::new(ToolEvent::ToolCompleted {
        tool_name: "read".into(),
        call_id: "pending".into(),
        output: Some("file contents".into()),
        detail: None,
        is_error: false,
        run_id: Some("run-1".into()),
    })
}

fn harness(model: TranscriptModel) -> Harness<'static, TranscriptModel> {
    let mut harness = Harness::new_ui_state(
        |ui, model| {
            gui::theme::install(ui.ctx());
            for entry in model.visible_entries() {
                gui::panes::transcript_tool::tool_card(ui, entry, egui::Id::new("pending"));
            }
        },
        model,
    );
    harness.run_steps(2);
    harness
}

fn spinner_rect(harness: &Harness<'_, TranscriptModel>) -> Option<egui::Rect> {
    harness
        .output()
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            Shape::Path(path) if !path.closed => Some(path.visual_bounding_rect()),
            _ => None,
        })
}

#[test]
fn tool_card_pending_shows_spinner_and_no_output() {
    // Given: ToolStarted has arrived, without a result.
    let mut model = TranscriptModel::new();
    model.apply(&started());
    let mut harness = harness(model);
    // When: attempting to expand a running card.
    harness.get_by_label_contains("(pending)").click();
    harness.run_steps(3);
    // Then: only the header and an inline spinner are shown.
    assert!(harness.query_by_label("Input").is_none());
    assert!(harness.query_by_label("Output").is_none());
    let spinner = spinner_rect(&harness).expect("running spinner");
    let header = harness
        .output()
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            Shape::Text(text) if text.galley.text().contains("(pending)") => Some(text),
            _ => None,
        })
        .expect("painted header");
    assert!((spinner.center().y - header.pos.y - header.galley.size().y / 2.0).abs() < 4.0);
}

#[test]
fn tool_card_pending_is_visible_when_collapsed() {
    // Given: the same run-scoped event used by the production GUI.
    let mut registry = TranscriptRegistry::new();
    // When: it reaches the main thread before completion.
    registry.apply(&started());
    let harness = harness(registry.thread().clone());
    // Then: the default collapsed view exposes the work immediately.
    assert!(harness.query_by_label_contains("Running").is_some());
    assert!(harness.query_by_label_contains("Read: .").is_some());
    assert!(spinner_rect(&harness).is_some());
    assert!(harness.query_by_label("Output").is_none());
    registry.apply(&completed());
    assert_eq!(registry.thread().entries().len(), 1);
    let harness = self::harness(registry.thread().clone());
    assert!(spinner_rect(&harness).is_none());
    assert!(harness.query_by_label("file contents").is_some());
}

#[test]
fn read_tool_input_shows_path_not_json() {
    // Given: a completed native read call uses the `file` key.
    let mut model = TranscriptModel::new();
    model.apply(&started());
    model.apply(&completed());
    let mut harness = harness(model);
    // When: expanding its details.
    harness.get_by_label_contains("(pending)").click();
    harness.run_steps(3);
    // Then: both summary and Input use the path, not JSON.
    assert!(harness.query_by_label_contains("Read: .").is_some());
    assert!(harness.query_by_label(".").is_some());
    assert!(harness.query_by_label_contains("\"file\"").is_none());
}

#[test]
fn tool_completed_stops_spinner_and_reveals_result_in_place() {
    // Given: a rendered running call.
    let mut model = TranscriptModel::new();
    model.apply(&started());
    let mut harness = harness(model);
    assert!(spinner_rect(&harness).is_some());
    // When: completion arrives on a later frame.
    harness.state_mut().apply(&completed());
    harness.run_steps(3);
    // Then: the existing card stops animating and previews its result.
    assert_eq!(harness.state().entries().len(), 1);
    assert!(spinner_rect(&harness).is_none());
    assert!(harness.query_by_label_contains("OK read").is_some());
    assert!(harness.query_by_label("file contents").is_some());
}

#[test]
fn capture_tool_pending_states_when_evidence_directory_is_set() {
    let Ok(directory) = std::env::var("EVORCH_TOOL_EVIDENCE_DIR") else {
        return;
    };
    let directory = std::path::Path::new(&directory);
    let mut model = TranscriptModel::new();
    model.apply(&started());
    let mut harness = harness(model);
    harness
        .render()
        .expect("render pending")
        .save(directory.join("pending.png"))
        .expect("save pending");
    harness.run_steps(6);
    harness
        .render()
        .expect("render animation")
        .save(directory.join("pending-next.png"))
        .expect("save animation");
    harness.state_mut().apply(&completed());
    harness.run_steps(3);
    harness
        .render()
        .expect("render completed")
        .save(directory.join("completed.png"))
        .expect("save completed");
    harness.get_by_label_contains("(pending)").click();
    harness.run_steps(3);
    harness
        .render()
        .expect("render expanded")
        .save(directory.join("expanded.png"))
        .expect("save expanded");
}
