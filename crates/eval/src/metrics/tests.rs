use event_bus::{DiagnosticEvent, DiagnosticSeverity, ProviderFailureKind};

use super::*;
use crate::driver::{DeniedRequest, Status};
use crate::test_support::{usage, write_run_db};

fn started(run_id: &str, parent: Option<&str>, role: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run_id.into(),
        parent_run_id: parent.map(str::to_owned),
        agent_name: format!("{role}-agent"),
        role: role.into(),
    })
}

fn tool_call(call: &str, tool: &str, input: serde_json::Value, error: Option<&str>) -> [Event; 2] {
    [
        Event::new(ToolEvent::ToolStarted {
            tool_name: tool.into(),
            call_id: call.into(),
            input: Some(input),
            run_id: Some("child".into()),
            parent_call_id: None,
        }),
        Event::new(ToolEvent::ToolCompleted {
            tool_name: tool.into(),
            call_id: call.into(),
            is_error: error.is_some(),
            output: Some(error.unwrap_or("ok").into()),
            detail: None,
            run_id: Some("child".into()),
            parent_call_id: None,
        }),
    ]
}

fn outcome(denied: Vec<DeniedRequest>) -> Outcome {
    Outcome {
        status: Status::Completed,
        reason: "done".into(),
        user_turns: 2,
        tokens: 0,
        tool_calls: 0,
        elapsed_ms: 1_500,
        denied_requests: denied,
        pushed_refs: Vec::new(),
        verification: None,
    }
}

fn recorded(events: &[Event], requests: Vec<storage::usage::UsageRequestRecord>) -> RunRecord {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("evorch.db");
    write_run_db(&db, events, requests);
    RunRecord::load(&db).unwrap()
}

#[test]
fn metrics_count_the_agent_and_keep_the_simulated_user_apart() {
    // Given: a root that delegates to a worker, three tool calls of which one fails, a failed
    // provider request with a fallback, and a simulated user's request beside the agent's.
    let mut events = vec![
        started("root", None, "orchestrator"),
        started("child", Some("root"), "worker"),
        Event::new(ProviderEvent::FallbackTriggered {
            from_provider: "a".into(),
            from_model: None,
            to_provider: "b".into(),
            to_model: "m".into(),
            logical_model: "worker".into(),
            session_id: "s".into(),
            failure: ProviderFailureKind::RateLimited,
            request_id: None,
        }),
    ];
    events.extend(tool_call(
        "1",
        "shell",
        serde_json::json!({"command": "ls"}),
        None,
    ));
    events.extend(tool_call(
        "2",
        "shell",
        serde_json::json!({"command": "false"}),
        Some("exit 1"),
    ));
    events.extend(tool_call(
        "3",
        "read",
        serde_json::json!({"path": "a"}),
        None,
    ));
    let mut cached = usage("r1", "api", "gpt-9", "agent", 1_000);
    cached.cache_read_tokens = 400;
    cached.output_tokens = 50;
    let mut failed = usage("r2", "api", "gpt-9", "agent", 0);
    failed.status = storage::usage::UsageStatus::Failed;
    let record = recorded(
        &events,
        vec![
            cached,
            failed,
            usage("r3", "api", "gpt-9", "user_simulation", 9_000),
        ],
    );

    // When: the run's metrics are derived.
    let metrics = RunMetrics::of(&record, &outcome(Vec::new()));

    // Then: only the agent's requests count as its tokens; the simulated user is overhead.
    assert_eq!(metrics.tokens.requests, 2);
    assert_eq!(metrics.tokens.input(), 1_000);
    assert_eq!(metrics.tokens.input_uncached, 600);
    assert_eq!(metrics.tokens.output, 50);
    assert!((metrics.cache_hit_rate_percent - 40.0).abs() < 1e-9);
    assert_eq!(metrics.overhead["user_simulation"].input(), 9_000);
    assert_eq!(metrics.provider_failures, 1);
    assert_eq!(metrics.provider_fallbacks, 1);
    assert_eq!(
        metrics.tool_calls,
        BTreeMap::from([("read".into(), 1), ("shell".into(), 2)])
    );
    assert_eq!(metrics.tool_errors, BTreeMap::from([("shell".into(), 1)]));
    assert_eq!(
        metrics
            .delegations
            .iter()
            .map(|delegation| (
                delegation.role.as_str(),
                delegation.parent_run_id.as_deref()
            ))
            .collect::<Vec<_>>(),
        [("orchestrator", None), ("worker", Some("root"))]
    );
    assert_eq!((metrics.user_turns, metrics.elapsed_ms), (2, 1_500));
}

#[test]
fn metrics_leave_out_calls_made_inside_a_parent_tool_call() {
    // Given: a run_js call whose child read call failed.
    let mut events = vec![started("child", None, "worker")];
    events.extend(tool_call("1", "run_js", serde_json::json!({}), None));
    let mut inner = tool_call(
        "1/0",
        "read",
        serde_json::json!({"path": "a"}),
        Some("boom"),
    );
    for event in &mut inner {
        match &mut event.kind {
            EventKind::Tool(
                ToolEvent::ToolStarted { parent_call_id, .. }
                | ToolEvent::ToolCompleted { parent_call_id, .. },
            ) => *parent_call_id = Some("1".into()),
            other => panic!("unexpected event: {other:?}"),
        }
    }
    events.extend(inner);
    let record = recorded(&events, Vec::new());

    // When: the run's metrics are derived.
    let metrics = RunMetrics::of(&record, &outcome(Vec::new()));

    // Then: only the model-issued call counts toward the turn totals.
    assert_eq!(metrics.tool_calls, BTreeMap::from([("run_js".into(), 1)]));
    assert!(metrics.tool_errors.is_empty());
}

#[test]
fn external_dependencies_flag_denials_failed_web_tools_and_network_errors() {
    // Given: a denied request, a failed fetch, a shell that could not resolve a host, and a
    // shell that failed for an unrelated reason.
    let mut events = Vec::new();
    events.extend(tool_call(
        "1",
        "web_fetch",
        serde_json::json!({"url": "https://example.test"}),
        Some("blocked"),
    ));
    events.extend(tool_call(
        "2",
        "shell",
        serde_json::json!({"command": "git fetch"}),
        Some("fatal: Could not resolve host: github.com"),
    ));
    events.extend(tool_call(
        "3",
        "shell",
        serde_json::json!({"command": "cargo test"}),
        Some("test failed"),
    ));
    let record = recorded(&events, Vec::new());
    let denied = DeniedRequest {
        tool: "shell".into(),
        input: "gh pr view".into(),
        reason: "denied".into(),
    };

    // When: the external dependencies are listed.
    let flags = external_dependencies(&record, &outcome(vec![denied]));

    // Then: all but the unrelated failure are flagged.
    assert_eq!(flags.len(), 3, "{flags:?}");
    assert!(flags[0].starts_with("denied shell: gh pr view"));
    assert!(flags[1].starts_with("failed web_fetch:"));
    assert!(flags[2].contains("git fetch"));
}

#[test]
fn the_timeline_summarizes_events_and_keeps_both_ends_of_long_runs() {
    // Given: a delegated worker that makes many tool calls and hits a notable diagnostic.
    let mut events = vec![
        started("root", None, "orchestrator"),
        started("child", Some("root"), "worker"),
    ];
    for call in 0..400 {
        events.extend(tool_call(
            &call.to_string(),
            "read",
            serde_json::json!({"path": format!("file-{call}")}),
            None,
        ));
    }
    events.push(Event::new(DiagnosticEvent {
        source: "runtime".into(),
        severity: DiagnosticSeverity::Warning,
        code: diagnostic_codes::IDENTICAL_TOOL_CALLS.into(),
        detail: "read repeated".into(),
        run_id: Some("child".into()),
        thread_id: None,
        call_id: None,
    }));
    let record = recorded(&events, vec![usage("r1", "api", "gpt-9", "agent", 10)]);

    // When: the timeline is built.
    let lines = timeline(&record);

    // Then: it starts with token totals per role, attributes calls to roles, and drops the
    // middle with a marker instead of growing without bound.
    assert!(
        lines[0].starts_with("tokens [worker]: 1 requests"),
        "{lines:?}"
    );
    assert!(lines[1].ends_with("[orchestrator] root run started (root)"));
    assert!(lines[2].ends_with("[orchestrator] delegated to worker (child)"));
    assert!(lines[3].contains("[worker] read {\"path\":\"file-0\"} -> ok"));
    assert_eq!(lines.len(), 1 + TIMELINE_HEAD + 1 + TIMELINE_TAIL);
    assert!(
        lines.contains(&"[103 events omitted]".to_owned()),
        "{lines:?}"
    );
    assert!(
        lines
            .last()
            .unwrap()
            .contains("IdenticalToolCalls: read repeated")
    );
}
