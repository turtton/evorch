//! Repeated identical status checks share one card; changes stay visible.

use event_bus::{Event, ToolEvent};
use gui::model::transcript::{TranscriptEntry, TranscriptModel};
use gui::panes::transcript_tool::card_len;
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
        parent_call_id: None,
    }));
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: tool.into(),
        call_id: id.into(),
        is_error: error,
        output: Some(output.into()),
        detail: None,
        run_id: Some("run-1".into()),
        parent_call_id: None,
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
        parent_call_id: None,
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
