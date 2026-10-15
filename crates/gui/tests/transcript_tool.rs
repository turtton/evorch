use egui::{FontFamily, epaint::Shape};
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, ToolEvent};
use gui::model::transcript::TranscriptModel;
use gui::panes::agent::transcript_body_with_repo_root;

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

fn grep_harness(input: serde_json::Value) -> Harness<'static> {
    let mut model = TranscriptModel::new();
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "grep".into(),
        call_id: "grep-1234".into(),
        input: Some(input),
        run_id: None,
    }));
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: "grep".into(),
        call_id: "grep-1234".into(),
        is_error: false,
        output: Some(String::new()),
        detail: None,
        run_id: None,
    }));
    let mut harness = Harness::new_ui(move |ui| {
        gui::theme::install(ui.ctx());
        transcript_body_with_repo_root(ui, &model, Some(std::path::Path::new("/repo")));
    });
    harness.run_steps(2);
    harness
}

fn expand<State>(harness: &mut Harness<'_, State>) {
    harness.get_by_label_contains("Shell find missing").click();
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
    assert!(harness.query_by_label("✓ Shell find missing").is_some());
    assert!(harness.query_by_label_contains("abcdefgh").is_none());
    assert!(harness.query_by_label_contains("OK").is_none());
    assert!(harness.query_by_label_contains("ERROR").is_none());
    assert!(harness.query_by_label("find missing").is_none());
    assert!(harness.query_by_label("Input").is_none());
    assert!(harness.query_by_label("Output").is_none());
    assert!(harness.query_by_label_contains("**literal**").is_none());
}

#[test]
fn grep_tool_card_header_shows_pattern_and_root_relative_path() {
    // Given
    let harness = grep_harness(serde_json::json!({"pattern": "foo", "path": "/repo/src/main.rs"}));
    // When / Then
    assert!(
        harness
            .query_by_label("✓ Grep \"foo\" in src/main.rs · 0 matches")
            .is_some()
    );
}

#[test]
fn grep_tool_card_header_shows_pattern_when_path_is_absent() {
    // Given
    let harness = grep_harness(serde_json::json!({"pattern": "foo"}));
    // When / Then
    assert!(
        harness
            .query_by_label("✓ Grep \"foo\" · 0 matches")
            .is_some()
    );
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
    assert!(harness.query_by_label("✗ Shell find missing").is_some());
    assert!(harness.query_by_label_contains("ERROR").is_none());
    assert!(harness.query_by_label("Error").is_some());
    assert!(
        output_shape(&harness)
            .galley
            .job
            .sections
            .iter()
            .all(|section| { section.format.color == gui::theme::tokens::palette().ERROR_FG })
    );
}

#[test]
fn tool_card_toggle_click_changes_expanded_state() {
    // Given
    let mut harness = harness(false);
    expand(&mut harness);
    assert!(harness.query_by_label("Output").is_some());
    // When
    harness.get_by_label("✓ Shell find missing").click();
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

#[test]
fn read_tool_card_keeps_file_name_visible_in_narrow_pane() {
    // Given: a read of a deep repository path rendered in a narrow pane.
    let path = "/repo/crates/gui/src/panes/agent/thinking_block.rs";
    let mut model = TranscriptModel::new();
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "read".into(),
        call_id: "read-2".into(),
        input: Some(serde_json::json!({"path": path})),
        run_id: None,
    }));
    let mut harness = Harness::builder()
        .with_size(egui::vec2(300.0, 200.0))
        .build_ui(move |ui| {
            gui::theme::install(ui.ctx());
            transcript_body_with_repo_root(ui, &model, Some(std::path::Path::new("/repo")));
        });
    harness.run_steps(2);
    // When / Then: the directory is shortened from the left, not the file name.
    let painted = harness
        .output()
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            Shape::Text(text) if text.galley.text().starts_with("Read thinking_block.rs") => {
                Some(text.galley.text().to_owned())
            }
            _ => None,
        })
        .expect("painted read header");
    assert!(painted.contains("…/"), "{painted}");
    assert!(!painted.contains(".evorch"), "{painted}");
}

#[test]
fn reviewed_shell_card_shows_its_verdict_while_collapsed() {
    // Given: a shell call whose sandbox escape was denied.
    let mut model = TranscriptModel::new();
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: "call-secret-77".into(),
        input: Some(serde_json::json!({"command": "curl example.com"})),
        run_id: Some("run-77".into()),
    }));
    model.apply(&Event::new(event_bus::DiagnosticEvent {
        source: "sandbox".into(),
        severity: event_bus::DiagnosticSeverity::Warning,
        code: "escalation_review".into(),
        detail: "denied: network is not needed".into(),
        run_id: Some("run-77".into()),
        thread_id: None,
        call_id: Some("call-secret-77".into()),
    }));
    // When: the collapsed card renders.
    let mut harness = Harness::new_ui(move |ui| {
        gui::theme::install(ui.ctx());
        ui.ctx()
            .global_style_mut(|style| style.interaction.tooltip_delay = 0.0);
        gui::panes::agent::transcript_body(ui, &model);
    });
    harness.run_steps(2);
    // Then: the verdict sits in the card, without a separate review card or IDs.
    harness.get_by_label("Sandbox escape reviewed: denied: network is not needed");
    assert!(harness.query_by_label("Input").is_none());
    assert_eq!(
        harness
            .query_all_by_label_contains("curl example.com")
            .count(),
        1
    );
    harness
        .get_by_label_contains("Shell curl example.com")
        .hover();
    harness.run_steps(2);
    harness.get_by_label("curl example.com");
    assert!(harness.query_by_label_contains("call-secret-77").is_none());
    assert!(harness.query_by_label_contains("run-77").is_none());
}

#[test]
fn agent_messages_name_their_peer_and_keep_the_run_id_copyable() {
    // Given: one message to a known reviewer and one from an unknown run.
    let unknown = "run-01HZX3K9M2Q8ABCDEFGHJKMN";
    let mut registry = gui::model::transcript_registry::TranscriptRegistry::new();
    for (sender, recipient, content) in [
        ("run-1", "run-3", "review run-2"),
        (unknown, "run-1", "done"),
    ] {
        registry.apply(&Event::new(event_bus::AgentMessageEvent::Delivered {
            message: event_bus::AgentMessage {
                message_id: content.into(),
                sender_run_id: sender.into(),
                recipient_run_id: recipient.into(),
                kind: event_bus::AgentMessageKind::Send,
                content: content.into(),
                reply_to: None,
            },
            disposition: event_bus::DeliveryDisposition::Aside,
        }));
    }
    let model = registry.run("run-1").unwrap().clone();
    let agents = [gui::model::tasks::TaskRow {
        run_id: runtime::RunId::new(3),
        name: "reviewer-r1".into(),
        role: "reviewer".into(),
        status: event_bus::AgentRunPhase::Running,
        model: String::new(),
        category: None,
        cancelled: false,
    }];
    // When: the run transcript renders with the task rows.
    let harness = Harness::new_ui(move |ui| {
        gui::theme::install(ui.ctx());
        gui::panes::agent::transcript_body_with_agents(ui, &model, None, &agents);
    });
    // Then: peers read as names, unknown runs are shortened, and IDs are copyable.
    harness.get_by_label("→ reviewer-r1 (reviewer): review run-2");
    harness.get_by_label("← run-01HZ…: done");
    assert_eq!(harness.query_all_by_label("Copy run ID").count(), 2);
    assert!(harness.query_by_label_contains(unknown).is_none());
}
