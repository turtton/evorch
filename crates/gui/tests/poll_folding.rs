//! Repeated identical status checks share one card; changes stay visible.

use event_bus::{Event, ToolEvent};
use gui::model::transcript::{TranscriptEntry, TranscriptModel};
use gui::panes::transcript_tool::{card_len, draft_preview_lines};
use serde_json::{Value, json};

fn call(
    model: &mut TranscriptModel,
    id: &str,
    tool: &str,
    input: Value,
    output: &str,
    error: bool,
) {
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: tool.into(),
        call_id: id.into(),
        input: Some(input),
        run_id: Some("run-1".into()),
    }));
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: tool.into(),
        call_id: id.into(),
        is_error: error,
        output: Some(output.into()),
        detail: None,
        run_id: Some("run-1".into()),
    }));
}

fn lens(model: &TranscriptModel) -> Vec<usize> {
    let entries = model.entries();
    let mut lens = Vec::new();
    let mut at = 0;
    while at < entries.len() {
        let len = card_len(&entries[at..], model.shell_jobs());
        lens.push(len);
        at += len;
    }
    lens
}

#[test]
fn identical_polls_fold_but_changed_state_and_failures_stand_alone() {
    // Given: repeated inbox checks, then one that reports a new message, a failure,
    // and a check of another target.
    let mut model = TranscriptModel::new();
    for id in ["a", "b", "c"] {
        call(&mut model, id, "inbox", json!({}), "[]", false);
    }
    call(
        &mut model,
        "d",
        "inbox",
        json!({}),
        "[{\"from\":\"run-2\"}]",
        false,
    );
    call(
        &mut model,
        "e",
        "inbox",
        json!({}),
        "[{\"from\":\"run-2\"}]",
        false,
    );
    call(&mut model, "f", "inbox", json!({}), "boom", true);
    call(
        &mut model,
        "g",
        "run_output",
        json!({"run_id": "run-2"}),
        "same",
        false,
    );
    call(
        &mut model,
        "h",
        "run_output",
        json!({"run_id": "run-3"}),
        "same",
        false,
    );

    // Then: only identical consecutive observations share a card.
    assert_eq!(lens(&model), [3, 2, 1, 1, 1]);
}

#[test]
fn other_tools_and_a_running_poll_never_fold() {
    // Given: identical reads, and a poll that has not finished.
    let mut model = TranscriptModel::new();
    call(&mut model, "a", "read", json!({"path": "a"}), "x", false);
    call(&mut model, "b", "read", json!({"path": "a"}), "x", false);
    call(
        &mut model,
        "c",
        "wait",
        json!({"run_id": "run-2"}),
        "running",
        false,
    );
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "wait".into(),
        call_id: "d".into(),
        input: Some(json!({"run_id": "run-2"})),
        run_id: Some("run-1".into()),
    }));

    // Then: ordinary tools stay separate, and the running poll is its own card
    // until it reports.
    assert_eq!(lens(&model), [1, 1, 1, 1]);
    call(
        &mut model,
        "e",
        "wait",
        json!({"run_id": "run-2"}),
        "running",
        false,
    );
    assert!(matches!(
        model.entries().last(),
        Some(TranscriptEntry::Tool { .. })
    ));
}

#[test]
fn draft_preview_keeps_the_end_of_a_long_single_line() {
    // Given: a long one-line JSON and a many-line body.
    let long = format!("{{\"content\":\"{}TAIL\"}}", "x".repeat(10_000));
    let lines = draft_preview_lines(&long);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with('…') && lines[0].ends_with("TAIL\"}"));
    assert!(lines[0].chars().count() <= 121);

    // Then: only the newest lines are kept.
    let body = (0..20)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        draft_preview_lines(&body),
        ["line 16", "line 17", "line 18", "line 19"]
    );
}
