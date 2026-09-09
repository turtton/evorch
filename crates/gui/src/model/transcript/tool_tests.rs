use super::{ToolStatus, TranscriptEntry, TranscriptModel};
use event_bus::{Event, ToolEvent};
use serde_json::json;

fn started(call_id: &str) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: "read_file".into(),
        call_id: call_id.into(),
        input: Some(json!({"path": "README.md"})),
        run_id: Some("run-1".into()),
    })
}

fn completed(is_error: bool) -> Event {
    Event::new(ToolEvent::ToolCompleted {
        tool_name: "read_file".into(),
        call_id: "c1".into(),
        output: Some("file contents".into()),
        detail: Some(json!({"bytes": 13})),
        is_error,
        run_id: Some("run-1".into()),
    })
}

#[test]
fn tool_started_stores_input_and_running_status() {
    // Given: an empty transcript.
    let mut model = TranscriptModel::new();
    // When: a tool starts with JSON arguments.
    model.apply(&started("c1"));
    // Then: arguments and initial state are retained.
    assert_eq!(
        model.entries(),
        &[TranscriptEntry::Tool {
            tool_name: "read_file".into(),
            call_id: "c1".into(),
            input: Some(json!({"path": "README.md"})),
            output: None,
            detail: None,
            is_error: false,
            status: ToolStatus::Running,
        }]
    );
}

#[test]
fn tool_completed_updates_output_detail_and_status() {
    // Given: two calls, with another entry after the target.
    let mut model = TranscriptModel::new();
    model.apply(&started("c1"));
    model.apply(&started("c2"));
    let other = model.entries()[1].clone();
    // When: the first call completes.
    model.apply(&completed(false));
    // Then: only that call changes, preserving its input.
    assert_eq!(
        model.entries(),
        &[
            TranscriptEntry::Tool {
                tool_name: "read_file".into(),
                call_id: "c1".into(),
                input: Some(json!({"path": "README.md"})),
                output: Some("file contents".into()),
                detail: Some(json!({"bytes": 13})),
                is_error: false,
                status: ToolStatus::Succeeded,
            },
            other
        ]
    );
}

#[test]
fn tool_completed_error_marks_is_error_true() {
    // Given: a running call.
    let mut model = TranscriptModel::new();
    model.apply(&started("c1"));
    // When: the tool reports an error.
    model.apply(&completed(true));
    // Then: both the error flag and failed status are retained with the result.
    assert!(matches!(model.entries(), [TranscriptEntry::Tool {
        is_error: true, status: ToolStatus::Failed, output: Some(output),
        detail: Some(detail), ..
    }] if output == "file contents" && detail == &json!({"bytes": 13})));
}

#[test]
fn tool_completed_without_prior_started_creates_entry() {
    // Given: no preceding start (for example, it was evicted).
    let mut model = TranscriptModel::new();
    // When: completion arrives.
    model.apply(&completed(false));
    // Then: the full result is retained with unknown input.
    assert_eq!(
        model.entries(),
        &[TranscriptEntry::Tool {
            tool_name: "read_file".into(),
            call_id: "c1".into(),
            input: None,
            output: Some("file contents".into()),
            detail: Some(json!({"bytes": 13})),
            is_error: false,
            status: ToolStatus::Succeeded,
        }]
    );
}

#[test]
fn tool_approval_requested_updates_status() {
    // Given: a call with arguments.
    let mut model = TranscriptModel::new();
    model.apply(&started("c1"));
    let mut expected = model.entries()[0].clone();
    if let TranscriptEntry::Tool { status, .. } = &mut expected {
        *status = ToolStatus::AwaitingApproval;
    }
    // When: approval is requested.
    model.apply(&Event::new(ToolEvent::ApprovalRequested {
        tool_name: "read_file".into(),
        call_id: "c1".into(),
    }));
    // Then: only status changes.
    assert_eq!(model.entries(), &[expected]);
}

#[test]
fn legacy_tool_events_keep_absent_payloads() {
    // Given: serialized events predating input/output.
    let events = [
        json!({"kind": "ToolStarted", "payload": {"tool_name": "read", "call_id": "c1"}}),
        json!({"kind": "ToolCompleted", "payload": {"tool_name": "read", "call_id": "c1", "is_error": false}}),
    ];
    let mut model = TranscriptModel::new();
    // When: replayed through the current event type.
    for value in events {
        model.apply(&Event::new(
            serde_json::from_value::<ToolEvent>(value).expect("legacy event"),
        ));
        // Then: missing payloads remain absent at both lifecycle stages.
        assert!(matches!(
            model.entries(),
            [TranscriptEntry::Tool {
                input: None,
                output: None,
                detail: None,
                is_error: false,
                ..
            }]
        ));
    }
}
