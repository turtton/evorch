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
            run_id: Some("run-1".into()),
            tool_name: "read_file".into(),
            call_id: "c1".into(),
            input: Some(json!({"path": "README.md"})),
            output: None,
            detail: None,
            is_error: false,
            status: ToolStatus::Running,
            reviews: Vec::new(),
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
                run_id: Some("run-1".into()),
                tool_name: "read_file".into(),
                call_id: "c1".into(),
                input: Some(json!({"path": "README.md"})),
                output: Some("file contents".into()),
                detail: Some(json!({"bytes": 13})),
                is_error: false,
                status: ToolStatus::Succeeded,
                reviews: Vec::new(),
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
            run_id: Some("run-1".into()),
            tool_name: "read_file".into(),
            call_id: "c1".into(),
            input: None,
            output: Some("file contents".into()),
            detail: Some(json!({"bytes": 13})),
            is_error: false,
            status: ToolStatus::Succeeded,
            reviews: Vec::new(),
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
        input: None,
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

fn started_in(run_id: &str, call_id: &str) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: call_id.into(),
        input: None,
        run_id: Some(run_id.into()),
    })
}

fn run_state(run_id: &str, to: event_bus::AgentRunPhase) -> Event {
    Event::new(event_bus::LifecycleEvent::AgentRunStateChanged {
        run_id: run_id.into(),
        from: event_bus::AgentRunPhase::Running,
        to,
        reason: None,
    })
}

fn statuses(model: &TranscriptModel) -> Vec<(&str, &ToolStatus)> {
    model
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Tool {
                call_id, status, ..
            } => Some((call_id.as_str(), status)),
            _ => None,
        })
        .collect()
}

#[test]
fn ending_a_run_interrupts_only_its_calls_that_are_still_running() {
    for phase in [
        event_bus::AgentRunPhase::Stopped,
        event_bus::AgentRunPhase::Done,
        event_bus::AgentRunPhase::Error,
    ] {
        // Given: a finished and a running call of run-1, and a running call of run-2.
        let mut model = TranscriptModel::new();
        model.apply(&started_in("run-1", "done"));
        model.apply(&Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: "done".into(),
            output: None,
            detail: None,
            is_error: false,
            run_id: Some("run-1".into()),
        }));
        model.apply(&started_in("run-1", "lost"));
        model.apply(&started_in("run-2", "other"));
        // When: run-1 ends without completing its second call.
        model.apply(&run_state("run-1", phase));
        // Then: only that call stops running.
        assert_eq!(
            statuses(&model),
            vec![
                ("done", &ToolStatus::Succeeded),
                ("lost", &ToolStatus::Interrupted),
                ("other", &ToolStatus::Running),
            ],
            "{phase:?}"
        );
    }
}

#[test]
fn waiting_keeps_running_calls_and_a_late_result_replaces_interrupted() {
    // Given: a running call.
    let mut model = TranscriptModel::new();
    model.apply(&started_in("run-1", "c1"));
    // When: the run only waits for input.
    model.apply(&run_state("run-1", event_bus::AgentRunPhase::Waiting));
    // Then: the call keeps running.
    assert_eq!(statuses(&model), vec![("c1", &ToolStatus::Running)]);
    // When: the run stops, and the call's result arrives afterwards.
    model.apply(&run_state("run-1", event_bus::AgentRunPhase::Stopped));
    assert_eq!(statuses(&model), vec![("c1", &ToolStatus::Interrupted)]);
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: "shell".into(),
        call_id: "c1".into(),
        output: Some("late".into()),
        detail: None,
        is_error: true,
        run_id: Some("run-1".into()),
    }));
    // Then: the reported outcome wins.
    assert_eq!(statuses(&model), vec![("c1", &ToolStatus::Failed)]);
}
