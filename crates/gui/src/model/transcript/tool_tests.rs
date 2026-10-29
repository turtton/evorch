use super::{ToolStatus, TranscriptEntry, TranscriptModel};
use event_bus::{Event, ToolEvent};
use serde_json::json;

fn started(call_id: &str) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: "read_file".into(),
        call_id: call_id.into(),
        input: Some(json!({"path": "README.md"})),
        run_id: Some("run-1".into()),
        parent_call_id: None,
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
        parent_call_id: None,
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
            parent_call_id: None,
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
                parent_call_id: None,
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
            parent_call_id: None,
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
        parent_call_id: None,
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
            parent_call_id: None,
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
        parent_call_id: None,
    }));
    // Then: the reported outcome wins.
    assert_eq!(statuses(&model), vec![("c1", &ToolStatus::Failed)]);
}

fn run_js_started(
    call_id: &str,
    tool_name: &str,
    input: serde_json::Value,
    parent: Option<&str>,
) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: tool_name.into(),
        call_id: call_id.into(),
        input: Some(input),
        run_id: Some("run-1".into()),
        parent_call_id: parent.map(str::to_owned),
    })
}

fn run_js_completed(
    call_id: &str,
    tool_name: &str,
    output: &str,
    detail: Option<serde_json::Value>,
    parent: Option<&str>,
) -> Event {
    Event::new(ToolEvent::ToolCompleted {
        tool_name: tool_name.into(),
        call_id: call_id.into(),
        output: Some(output.into()),
        detail,
        is_error: false,
        run_id: Some("run-1".into()),
        parent_call_id: parent.map(str::to_owned),
    })
}

fn tools(model: &TranscriptModel) -> Vec<(&str, Option<&str>, &ToolStatus)> {
    model
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Tool {
                call_id,
                parent_call_id,
                status,
                ..
            } => Some((call_id.as_str(), parent_call_id.as_deref(), status)),
            _ => None,
        })
        .collect()
}

#[test]
fn run_js_children_keep_their_parent_through_completion() {
    // Given: a run_js call that makes a read and then an edit.
    let mut model = TranscriptModel::new();
    model.apply(&run_js_started(
        "js",
        "run_js",
        json!({"code": "return 1"}),
        None,
    ));
    model.apply(&run_js_started(
        "js/1",
        "read",
        json!({"path": "a.rs"}),
        Some("js"),
    ));
    model.apply(&run_js_completed(
        "js/1",
        "read",
        "fn a() {}",
        None,
        Some("js"),
    ));
    model.apply(&run_js_started(
        "js/2",
        "edit",
        json!({"path": "a.rs"}),
        Some("js"),
    ));
    // Then: while it runs, the children follow the parent in start order.
    assert_eq!(
        tools(&model),
        vec![
            ("js", None, &ToolStatus::Running),
            ("js/1", Some("js"), &ToolStatus::Succeeded),
            ("js/2", Some("js"), &ToolStatus::Running),
        ]
    );
    // When: the edit and then the run_js call complete.
    model.apply(&run_js_completed("js/2", "edit", "+b", None, Some("js")));
    let detail = json!({"run_js": {"calls": [], "timed_out": false}});
    model.apply(&run_js_completed(
        "js",
        "run_js",
        "=> 1\n[run_js: 2 calls, 0 failed; wrote: a.rs]",
        Some(detail.clone()),
        None,
    ));
    // Then: each call keeps its own card and only the parent has the summary.
    assert_eq!(
        tools(&model),
        vec![
            ("js", None, &ToolStatus::Succeeded),
            ("js/1", Some("js"), &ToolStatus::Succeeded),
            ("js/2", Some("js"), &ToolStatus::Succeeded),
        ]
    );
    assert!(matches!(
        &model.entries()[0],
        TranscriptEntry::Tool { detail: Some(current), .. } if current == &detail
    ));
}

#[test]
fn a_child_completion_without_its_start_still_records_the_parent() {
    // Given: a restored history that lost the child's start event.
    let mut model = TranscriptModel::new();
    // When: only the completion arrives.
    model.apply(&run_js_completed("js/1", "grep", "hit", None, Some("js")));
    // Then: the card still names its parent.
    assert_eq!(
        tools(&model),
        vec![("js/1", Some("js"), &ToolStatus::Succeeded)]
    );
}

#[test]
fn running_child_shell_resolves_its_job_from_live_output() {
    use crate::panes::transcript_tool::resolved_shell_job_key;
    // Given: a run_js call whose shell child is still running.
    let mut model = TranscriptModel::new();
    model.apply(&run_js_started(
        "js",
        "run_js",
        json!({"code": "shell(...)"}),
        None,
    ));
    model.apply(&run_js_started(
        "js/1",
        "shell",
        json!({"command": "cargo build"}),
        Some("js"),
    ));
    let child = model.entries()[1].clone();
    assert_eq!(resolved_shell_job_key(&child, model.shell_jobs()), None);
    // When: the job streams output naming the call that started it.
    model.apply(&Event::new(ToolEvent::ShellJobOutput {
        job_uid: Some("uid-1".into()),
        job_id: "job-0".into(),
        call_id: Some("js/1".into()),
        run_id: Some("run-1".into()),
        offset: 0,
        chunk: "Compiling gui\n".into(),
        status: "running".into(),
        exit_code: None,
    }));
    // Then: the child's card resolves that job and its live log.
    let key = resolved_shell_job_key(&child, model.shell_jobs()).expect("job of the child");
    assert_eq!(
        key,
        super::shell_jobs::ShellJobKey::new(Some("run-1"), "job-0", Some("uid-1"))
    );
    assert_eq!(
        model.shell_jobs().get_key(&key).unwrap().tail(1),
        vec!["Compiling gui"]
    );
    // And: another run's call with the same ID does not claim it.
    let mut stranger = child.clone();
    if let TranscriptEntry::Tool { run_id, .. } = &mut stranger {
        *run_id = Some("run-2".into());
    }
    assert_eq!(resolved_shell_job_key(&stranger, model.shell_jobs()), None);
}
