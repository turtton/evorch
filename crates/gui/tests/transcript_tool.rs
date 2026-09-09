use egui::{FontFamily, epaint::Shape};
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, ToolEvent};
use gui::model::transcript::TranscriptModel;

fn transcript(is_error: bool) -> TranscriptModel {
    let mut model = TranscriptModel::new();
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "bash".into(),
        call_id: "abcdefgh-1234".into(),
        input: Some(serde_json::json!({"command": "find missing"})),
        run_id: None,
    }));
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: "bash".into(),
        call_id: "abcdefgh-1234".into(),
        is_error,
        output: Some("**literal**: missing directory".into()),
        detail: Some(serde_json::json!({"exit_code": 1})),
        run_id: None,
    }));
    model
}

fn harness(is_error: bool) -> Harness<'static> {
    let model = transcript(is_error);
    let mut harness = Harness::new_ui(move |ui| {
        gui::theme::install(ui.ctx());
        gui::panes::agent::transcript_body(ui, &model);
    });
    harness.run_steps(2);
    harness
}

fn expand<State>(harness: &mut Harness<'_, State>) {
    harness.get_by_label_contains("bash (abcdefgh)").click();
    harness.run_steps(3);
}

fn output_shape(harness: &Harness<'_>) -> egui::epaint::TextShape {
    harness
        .output()
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            Shape::Text(text) if text.galley.text().contains("**literal**") => Some(text.clone()),
            _ => None,
        })
        .expect("tool output is painted")
}

#[test]
fn tool_card_collapsed_hides_sections() {
    // Given / When
    let harness = harness(false);
    // Then
    assert!(harness.query_by_label_contains("bash (abcdefgh)").is_some());
    assert!(harness.query_by_label_contains("find missing").is_some());
    assert!(harness.query_by_label("Input").is_none());
    assert!(harness.query_by_label("Output").is_none());
    assert!(harness.query_by_label_contains("**literal**").is_some());
}

#[test]
fn tool_card_expanded_contains_input_and_output_sections() {
    // Given
    let mut harness = harness(false);
    // When
    expand(&mut harness);
    // Then
    for label in ["Input", "Output", "Detail"] {
        assert!(harness.query_by_label(label).is_some());
    }
    assert!(harness.query_by_label_contains("exit_code").is_some());
}

#[test]
fn tool_card_error_status_shows_error_label_and_red_output() {
    // Given
    let mut harness = harness(true);
    // When
    expand(&mut harness);
    // Then
    assert!(harness.query_by_label_contains("ERROR").is_some());
    assert!(harness.query_by_label("Error").is_some());
    assert!(
        output_shape(&harness)
            .galley
            .job
            .sections
            .iter()
            .all(|section| { section.format.color == gui::theme::tokens::ERROR_FG })
    );
}

#[test]
fn tool_card_toggle_click_changes_expanded_state() {
    // Given
    let mut harness = harness(false);
    expand(&mut harness);
    assert!(harness.query_by_label("Output").is_some());
    // When
    harness.get_by_label_contains("bash (abcdefgh)").click();
    harness.run_steps(3);
    // Then
    assert!(harness.query_by_label("Output").is_none());
}

#[test]
fn tool_card_input_shows_command_only_for_bash() {
    // Given
    let mut harness = harness(false);
    // When
    expand(&mut harness);
    // Then
    assert!(harness.query_by_label("find missing").is_some());
    assert!(harness.query_by_label_contains("\"command\"").is_none());
}

#[test]
fn tool_card_output_renders_as_code_like() {
    // Given
    let mut harness = harness(false);
    // When
    expand(&mut harness);
    // Then
    let shape = output_shape(&harness);
    assert_eq!(shape.galley.text(), "**literal**: missing directory");
    assert!(
        shape
            .galley
            .job
            .sections
            .iter()
            .all(|section| { section.format.font_id.family == FontFamily::Monospace })
    );
}

#[test]
fn agent_details_tool_card_can_expand() {
    // Given
    let model = transcript(false);
    let mut harness = Harness::new_ui(move |ui| {
        gui::theme::install(ui.ctx());
        gui::panes::agent_transcript::agent_transcript_pane(ui, "run-1", Some(&model));
    });
    harness.run_steps(2);
    // When
    expand(&mut harness);
    // Then
    assert!(harness.query_by_label("Output").is_some());
}

#[test]
fn tool_card_state_uses_full_call_id_and_survives_entry_reordering() {
    // Given
    let model = transcript(false);
    let mut harness = Harness::new_ui_state(
        |ui, model| gui::panes::agent::transcript_body(ui, model),
        model,
    );
    harness.run_steps(2);
    expand(&mut harness);
    // When: the expanded call moves to index zero in a new visible window.
    harness.state_mut().push_tool(
        "read",
        "abcdefgh-5678",
        gui::model::transcript::ToolStatus::Succeeded,
    );
    harness.state_mut().set_view_window(1, 1);
    harness.run_steps(2);
    // Then: the other call sharing the display prefix stays collapsed.
    assert!(harness.query_by_label("Output").is_none());
    harness.state_mut().set_view_window(0, 1);
    harness.run_steps(2);
    assert!(harness.query_by_label("Output").is_some());
}

#[test]
fn tool_card_capture_states_when_evidence_directory_is_set() {
    // Given
    let Some(directory) = std::env::var_os("EVORCH_TOOL_CAPTURE_DIR") else {
        return;
    };
    let directory = std::path::Path::new(&directory);
    for is_error in [false, true] {
        let mut harness = harness(is_error);
        let kind = if is_error { "error" } else { "success" };
        // When / Then
        harness
            .render()
            .expect("render collapsed")
            .save(directory.join(format!("tool-{kind}-collapsed.png")))
            .expect("save capture");
        expand(&mut harness);
        harness
            .render()
            .expect("render expanded")
            .save(directory.join(format!("tool-{kind}-expanded.png")))
            .expect("save capture");
    }
}
