use egui::epaint::Shape;
use egui_kittest::{Harness, kittest::Queryable};
use gui::model::transcript::{ToolStatus, TranscriptEntry};

fn harness(tool: &str, input: serde_json::Value, output: &str) -> Harness<'static> {
    let entry = TranscriptEntry::Tool {
        tool_name: tool.into(),
        call_id: "cli-test".into(),
        input: Some(input),
        output: Some(output.into()),
        detail: None,
        is_error: false,
        status: ToolStatus::Succeeded,
    };
    let mut harness = Harness::new_ui(move |ui| {
        gui::theme::install(ui.ctx());
        gui::panes::transcript_tool::tool_card(ui, &entry, egui::Id::new("cli"));
    });
    harness.run_steps(2);
    harness
}

fn expand(harness: &mut Harness<'_>) {
    harness.get_by_label_contains("(cli-test)").click();
    harness.run_steps(3);
}

#[test]
fn tool_card_bash_shows_dollar_command_in_header() {
    // Given / When
    for tool in ["bash", "shell"] {
        let harness = harness(tool, serde_json::json!({"command": "git status"}), "");
        // Then
        assert!(harness.query_by_label_contains("$ git status").is_some());
    }
}

#[test]
fn tool_card_read_shows_path_in_header() {
    // Given / When
    for field in ["file_path", "path", "filePath", "file"] {
        let mut harness = harness("read", serde_json::json!({field: "src/main.rs"}), "");
        // Then
        assert!(
            harness
                .query_by_label_contains("Read: src/main.rs")
                .is_some()
        );
        expand(&mut harness);
        assert!(harness.query_by_label("src/main.rs").is_some());
    }
}

#[test]
fn tool_card_write_and_edit_show_path() {
    // Given / When
    for (tool, prefix) in [("write", "Write"), ("edit", "Edit")] {
        let mut harness = harness(tool, serde_json::json!({"file": "test.txt"}), "");
        // Then
        assert!(
            harness
                .query_by_label_contains(&format!("{prefix}: test.txt"))
                .is_some()
        );
        expand(&mut harness);
        assert!(harness.query_by_label("test.txt").is_some());
    }
}

#[test]
fn tool_card_output_combines_stdout_and_stderr() {
    // Given
    let mut harness = harness(
        "shell",
        serde_json::json!({"command": "check"}),
        "exit_code: 1\n--- stdout ---\nnormal\n--- stderr ---\nwarning",
    );
    // When
    expand(&mut harness);
    // Then
    assert!(
        harness
            .query_by_label("exit_code: 1\nnormal\nwarning")
            .is_some()
    );
    assert!(harness.query_by_label_contains("--- stdout ---").is_none());
    assert!(harness.query_by_label_contains("--- stderr ---").is_none());
}

#[test]
fn tool_card_preserves_plain_and_new_output() {
    // Given
    for output in [
        "exit_code: 0\n--- output ---\nnormal\nwarning",
        "--- stdout ---\nliteral",
    ] {
        let mut harness = harness("bash", serde_json::json!({"command": "check"}), output);
        // When
        expand(&mut harness);
        // Then
        assert!(harness.query_by_label(output).is_some());
    }
}

#[test]
fn tool_card_unknown_input_keeps_pretty_json() {
    // Given
    let mut harness = harness("custom", serde_json::json!({"query": "hello"}), "");
    // When
    expand(&mut harness);
    // Then
    assert!(
        harness
            .query_by_label("{\n  \"query\": \"hello\"\n}")
            .is_some()
    );
}

#[test]
fn tool_card_collapsed_shows_compact_preview() {
    // Given / When
    let mut harness = harness(
        "bash",
        serde_json::json!({"command": "check"}),
        "one\ntwo\nthree\nfour\nfive\nsix\nseven",
    );
    // Then
    assert!(
        harness
            .query_by_label("one\ntwo\nthree\nfour\nfive")
            .is_some()
    );
    assert!(harness.query_by_label_contains("six").is_none());
    expand(&mut harness);
    assert!(harness.query_by_label_contains("six\nseven").is_some());
}

#[test]
fn transcript_card_accent_line_does_not_overlap_text() {
    // Given
    let accent = gui::theme::tokens::INFO;
    let mut harness = Harness::new_ui(move |ui| {
        gui::theme::install(ui.ctx());
        gui::theme::widgets::card(ui, accent, |ui| {
            ui.label("Transcript content");
            ui.label("Second line");
        });
    });
    // When
    harness.run_steps(2);
    // Then
    let shapes = &harness.output().shapes;
    let bar = shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            Shape::Rect(rect) if rect.fill == accent => Some(rect.rect),
            _ => None,
        })
        .expect("accent remains visible");
    for label in ["Transcript content", "Second line"] {
        let text = shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                Shape::Text(text) if text.galley.text() == label => Some(text),
                _ => None,
            })
            .expect("content is painted");
        assert!(bar.right() < text.pos.x, "bar {bar:?}, text {:?}", text.pos);
        assert!(bar.bottom() >= text.pos.y);
    }
}
