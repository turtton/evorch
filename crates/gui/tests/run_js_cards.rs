//! The run_js card (ADR 0039) nests the calls its code made and streams a
//! child shell's live output inside the card.

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, ToolEvent};
use gui::model::transcript::TranscriptModel;
use serde_json::json;

const CODE: &str = "const src = await read({path: 'a.rs'});\nawait shell({command: 'cargo build'});\nreturn {ok: true};";

fn start(
    model: &mut TranscriptModel,
    call: &str,
    tool: &str,
    input: serde_json::Value,
    parent: Option<&str>,
) {
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: tool.into(),
        call_id: call.into(),
        input: Some(input),
        run_id: Some("run-1".into()),
        parent_call_id: parent.map(str::to_owned),
    }));
}

fn complete(
    model: &mut TranscriptModel,
    call: &str,
    tool: &str,
    output: &str,
    detail: Option<serde_json::Value>,
    parent: Option<&str>,
) {
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: tool.into(),
        call_id: call.into(),
        is_error: false,
        output: Some(output.into()),
        detail,
        run_id: Some("run-1".into()),
        parent_call_id: parent.map(str::to_owned),
    }));
}

/// A run_js call that has read a file and is running a shell command.
fn running() -> TranscriptModel {
    let mut model = TranscriptModel::new();
    start(&mut model, "js", "run_js", json!({"code": CODE}), None);
    start(
        &mut model,
        "js/1",
        "read",
        json!({"path": "a.rs"}),
        Some("js"),
    );
    complete(&mut model, "js/1", "read", "fn a() {}\n", None, Some("js"));
    start(
        &mut model,
        "js/2",
        "shell",
        json!({"command": "cargo build"}),
        Some("js"),
    );
    model.apply(&Event::new(ToolEvent::ShellJobOutput {
        job_uid: Some("uid-1".into()),
        job_id: "job-0".into(),
        call_id: Some("js/2".into()),
        run_id: Some("run-1".into()),
        offset: 0,
        chunk: "Compiling gui v0.1.0\n".into(),
        status: "running".into(),
        exit_code: None,
    }));
    model
}

fn finished() -> TranscriptModel {
    let mut model = running();
    complete(
        &mut model,
        "js/2",
        "shell",
        "exit_code: 0\n--- stdout ---\n\n--- stderr ---\n",
        None,
        Some("js"),
    );
    complete(
        &mut model,
        "js",
        "run_js",
        "read 9 bytes\n=> {\"ok\":true}\n[run_js: 2 calls, 0 failed; wrote: none]",
        Some(json!({"run_js": {
            "calls": [
                {"seq": 1, "name": "read", "is_error": false, "paths": ["a.rs"]},
                {"seq": 2, "name": "shell", "is_error": false, "paths": []},
            ],
            "output_bytes": 13,
            "return_bytes": 11,
            "timed_out": false,
            "limit_hit": false,
            "written_paths": [],
        }})),
        None,
    );
    model
}

fn harness(model: TranscriptModel, width: f32) -> Harness<'static> {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(width, 600.0))
        .build_ui(move |ui| {
            gui::theme::install(ui.ctx());
            gui::panes::agent::transcript_body(ui, &model);
        });
    harness.run_steps(2);
    harness
}

fn capture(harness: &mut Harness<'_>, name: &str) {
    if let Some(directory) = std::env::var_os("EVORCH_TOOL_CAPTURE_DIR") {
        harness
            .render()
            .expect("render run_js card")
            .save(std::path::Path::new(&directory).join(name))
            .expect("save capture");
    }
}

#[test]
fn running_card_lists_children_once_and_streams_the_child_shell() {
    let mut harness = harness(running(), 640.0);
    assert!(
        harness
            .query_by_label("Run JS const src = await read({path: 'a.rs'}); · 2 calls")
            .is_some()
    );
    // Each child appears once, inside the parent card rather than beside it.
    assert_eq!(harness.query_all_by_label_contains("Read a.rs").count(), 1);
    assert_eq!(
        harness
            .query_all_by_label_contains("Shell cargo build")
            .count(),
        1
    );
    assert!(harness.query_by_label("Compiling gui v0.1.0").is_some());
    capture(&mut harness, "tool-run-js-running.png");
}

#[test]
fn finished_card_expands_to_code_children_and_result() {
    let mut harness = harness(finished(), 640.0);
    let card = "✓ Run JS const src = await read({path: 'a.rs'}); · 2 calls";
    assert!(harness.query_by_label("Compiling gui v0.1.0").is_none());
    harness.get_by_label(card).click();
    harness.run_steps(3);
    for label in ["Code", "Calls", "Output", "Return value"] {
        assert!(harness.query_by_label(label).is_some(), "{label}");
    }
    assert!(harness.query_by_label(CODE).is_some());
    assert!(harness.query_by_label("read 9 bytes").is_some());
    assert!(harness.query_by_label_contains("\"ok\": true").is_some());
    assert!(
        harness
            .query_by_label("[run_js: 2 calls, 0 failed; wrote: none]")
            .is_some()
    );
    capture(&mut harness, "tool-run-js-expanded.png");
}

#[test]
fn narrow_pane_keeps_the_nested_cards() {
    let mut harness = harness(running(), 320.0);
    assert_eq!(harness.query_all_by_label_contains("Read a.rs").count(), 1);
    capture(&mut harness, "tool-run-js-narrow.png");
}

#[test]
fn child_without_its_parent_renders_at_top_level() {
    let mut model = TranscriptModel::new();
    complete(
        &mut model,
        "lost/1",
        "grep",
        "a.rs:1:hit\n",
        None,
        Some("lost"),
    );
    let harness = harness(model, 640.0);
    assert!(harness.query_by_label_contains("Grep").is_some());
}
