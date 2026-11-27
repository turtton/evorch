use super::*;

/// Calls added to the explorer run of `Fixture::thread`.
fn explorer_calls(fixture: &Fixture, root: &str, calls: &[(&str, bool)]) {
    run_calls(fixture, &format!("{root}-explorer"), calls);
}

/// Calls recorded for `child`, each with its start and its end.
fn run_calls(fixture: &Fixture, child: &str, calls: &[(&str, bool)]) {
    let child = child.to_owned();
    let handle = fixture.recorded.storage.handle();
    for (index, (tool, is_error)) in calls.iter().enumerate() {
        let call_id = format!("extra-{index}");
        for event in [
            Event::new(ToolEvent::ToolStarted {
                tool_name: (*tool).into(),
                call_id: call_id.clone(),
                input: None,
                run_id: Some(child.clone()),
                parent_call_id: None,
            }),
            Event::new(ToolEvent::ToolCompleted {
                tool_name: (*tool).into(),
                call_id,
                is_error: *is_error,
                output: None,
                detail: None,
                run_id: Some(child.clone()),
                parent_call_id: None,
            }),
        ] {
            handle.append_event(Some("gui"), &event).unwrap();
        }
    }
}

fn state_change(run: &str, to: event_bus::AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from: event_bus::AgentRunPhase::Running,
        to,
        reason: None,
        waiting: None,
    })
}

/// The explorer of `Fixture::thread` ends, as a delegated run does.
fn end_explorer(fixture: &Fixture, root: &str) {
    fixture
        .recorded
        .storage
        .handle()
        .append_event(
            Some("gui"),
            &state_change(&format!("{root}-explorer"), event_bus::AgentRunPhase::Done),
        )
        .unwrap();
}

/// Closes the `read` call `Fixture::thread` leaves without a terminal event.
fn finish_read(fixture: &Fixture, root: &str) {
    let event = Event::new(ToolEvent::ToolCompleted {
        tool_name: "read".into(),
        call_id: "call-1".into(),
        is_error: false,
        output: None,
        detail: None,
        run_id: Some(format!("{root}-explorer")),
        parent_call_id: None,
    });
    fixture
        .recorded
        .storage
        .handle()
        .append_event(Some("gui"), &event)
        .unwrap();
}

fn grep_errors(max: u32) -> Value {
    json!({
        "type": "tool_errors", "tool": "grep", "max": max,
        "scope": {"roles": ["Explorer"]}, "calls": "direct",
    })
}

fn sample_of(fixture: &Fixture, thread_id: &str) -> Option<EvalSample> {
    let database = Database::open_read_only(&fixture.recorded.config.db_path).unwrap();
    database.eval_sample(thread_id).unwrap()
}

#[tokio::test]
async fn a_failure_sample_is_saved_only_when_a_check_fails_on_the_recording() {
    // Given: a recorded thread whose explorer's grep failed twice.
    let fixture = Fixture::new();
    let root = fixture.thread("thread-1").await;
    explorer_calls(&fixture, &root, &[("grep", true), ("grep", true)]);
    let save = |checks: Value| {
        fixture.call(
            "eval_save_sample",
            json!({
                "thread_id": "thread-1", "problem": "grep keeps failing",
                "kind": "failure", "tags": ["grep"],
                "source_refs": [{"type": "run", "run_id": format!("{root}-explorer")}],
                "checks": checks,
            }),
        )
    };

    // When: checks that miss the failure and checks that catch it are saved in turn.
    let missed = save(json!([{
        "type": "tool_errors", "tool": "glob", "max": 0, "scope": "tree", "calls": "direct",
    }]))
    .await;
    assert!(sample_of(&fixture, "thread-1").is_none());
    let caught = save(json!([grep_errors(0)])).await;

    // Then: the first is refused with the reason and the grading, nothing is stored;
    // the second is saved with its grading on the recording.
    assert!(
        missed.is_error && missed.content.contains("cannot detect the failure"),
        "{}",
        missed.content
    );
    assert!(!caught.is_error, "{}", caught.content);
    let saved: Value = serde_json::from_str(&caught.content).unwrap();
    assert_eq!(saved["checks_on_recording"]["quality"], "fail");
    assert_eq!(saved["sample"]["machine_judgment"], true);
    let stored = sample_of(&fixture, "thread-1").unwrap();
    assert_eq!(stored.kind, SampleKind::Failure);
    assert_eq!(stored.tags, ["grep"]);
    assert_eq!(
        stored.checks,
        [Check::ToolErrors {
            tool: "grep".into(),
            max: 0,
            scope: CheckScope::Roles(vec!["explorer".into()]),
            calls: storage::eval_sample::CallRange::Direct,
        }]
    );
}

#[tokio::test]
async fn a_regression_sample_needs_its_checks_to_pass_on_a_complete_recording() {
    // Given: a thread whose read call never ended, and a check for one successful read.
    let fixture = Fixture::new();
    let root = fixture.thread("thread-1").await;
    let save = || {
        fixture.call(
            "eval_save_sample",
            json!({
                "thread_id": "thread-1", "problem": "reads before editing", "kind": "regression",
                "checks": [{
                    "type": "successful_calls", "tool": "read", "min": 1,
                    "scope": {"roles": ["explorer"]}, "calls": "direct",
                }],
            }),
        )
    };

    // When: it is saved before and after the call's terminal event is recorded.
    let incomplete = save().await;
    finish_read(&fixture, &root);
    end_explorer(&fixture, &root);
    let complete = save().await;

    // Then: only the complete recording gives a pass.
    assert!(incomplete.is_error, "{}", incomplete.content);
    assert!(!complete.is_error, "{}", complete.content);
    let saved: Value = serde_json::from_str(&complete.content).unwrap();
    assert_eq!(saved["checks_on_recording"]["quality"], "pass");
}

#[tokio::test]
async fn source_references_must_belong_to_the_thread() {
    // Given: two recorded threads.
    let fixture = Fixture::new();
    let root = fixture.thread("thread-1").await;
    let other = fixture.thread("thread-2").await;
    let save = |run: &str| {
        fixture.call(
            "eval_save_sample",
            json!({
                "thread_id": "thread-1", "problem": "slow",
                "source_refs": [{"type": "run", "run_id": run}],
            }),
        )
    };

    // When/Then: a run of the thread is accepted; a run of another thread is refused.
    assert!(!save(&root).await.is_error);
    let refused = save(&other).await;
    assert!(
        refused.is_error && refused.content.contains("not a run of this thread"),
        "{}",
        refused.content
    );
}

#[tokio::test]
async fn invalid_checks_are_refused_before_anything_is_read() {
    let fixture = Fixture::new();
    fixture.thread("thread-1").await;
    let save = |checks: Value| {
        fixture.call(
            "eval_save_sample",
            json!({"thread_id": "thread-1", "problem": "slow", "checks": checks}),
        )
    };

    let unknown_role = save(json!([{
        "type": "tool_errors", "tool": "grep", "max": 0,
        "scope": {"roles": ["Janitor"]}, "calls": "direct",
    }]))
    .await;
    let unknown_code = save(json!([{
        "type": "diagnostics", "code": "NoSuchCode", "max": 0, "scope": "tree",
    }]))
    .await;
    let unbounded = save(json!([{
        "type": "successful_calls", "tool": "read", "scope": "tree", "calls": "direct",
    }]))
    .await;

    assert!(unknown_role.is_error && unknown_role.content.contains("Janitor"));
    assert!(unknown_code.is_error && unknown_code.content.contains("NoSuchCode"));
    assert!(unbounded.is_error && unbounded.content.contains("min, max or both"));
    assert!(sample_of(&fixture, "thread-1").is_none());
}

#[tokio::test]
async fn a_sample_without_checks_gets_no_machine_judgment_and_inspection_grades_the_rest() {
    // Given: a thread pinned without checks.
    let fixture = Fixture::new();
    let root = fixture.thread("thread-1").await;
    let plain = fixture
        .json(
            "eval_save_sample",
            json!({"thread_id": "thread-1", "problem": "slow"}),
        )
        .await;
    let before = fixture
        .json("eval_inspect_thread", json!({"thread_id": "thread-1"}))
        .await;

    // When: it is pinned again with a check, and inspected.
    explorer_calls(&fixture, &root, &[("grep", true)]);
    fixture
        .json(
            "eval_save_sample",
            json!({"thread_id": "thread-1", "problem": "slow", "checks": [grep_errors(0)]}),
        )
        .await;
    let after = fixture
        .json("eval_inspect_thread", json!({"thread_id": "thread-1"}))
        .await;

    // Then: the first says so, the second shows the grading with its evidence.
    assert_eq!(plain["sample"]["machine_judgment"], false);
    assert_eq!(before["sample_checks"]["quality"], "no machine judgment");
    assert_eq!(after["sample_checks"]["quality"], "fail");
    let result = &after["sample_checks"]["grading"]["results"][0];
    assert_eq!(result["verdict"], "fail");
    assert_eq!(result["observed"], 1);
    assert_eq!(result["evidence"][0]["call_id"], "extra-0");
}

#[tokio::test]
async fn a_check_on_the_root_role_is_graded_though_the_root_started_before_the_capture() {
    // Given: a thread whose root run announced itself before its start state was captured
    // (as every real thread does), and a root-run grep that failed after the capture.
    let fixture = Fixture::new();
    let root = fixture.thread("thread-1").await;
    let captured_at_ns = Database::open_read_only(&fixture.recorded.config.db_path)
        .unwrap()
        .thread_start_state("thread-1")
        .unwrap()
        .unwrap()
        .captured_at_ns;
    let mut start = Event::new(LifecycleEvent::AgentRunStarted {
        run_id: root.clone(),
        parent_run_id: None,
        agent_name: "worker".into(),
        role: "worker".into(),
    });
    start.meta.wall_clock = std::time::UNIX_EPOCH
        + Duration::from_nanos(u64::try_from(captured_at_ns).unwrap() - 1_000_000);
    let handle = fixture.recorded.storage.handle();
    handle.append_event(Some("gui"), &start).unwrap();
    run_calls(&fixture, &root, &[("grep", true)]);
    handle
        .append_event(
            Some("gui"),
            &state_change(&root, event_bus::AgentRunPhase::Done),
        )
        .unwrap();
    let database = Database::open_read_only(&fixture.recorded.config.db_path).unwrap();
    let from_capture = |from_ns| {
        database
            .events_for_runs(std::slice::from_ref(&root), from_ns, 1_000)
            .unwrap()
            .0
            .into_iter()
            .any(|stored| {
                matches!(
                    stored.event.kind,
                    EventKind::Lifecycle(LifecycleEvent::AgentRunStarted { .. })
                )
            })
    };
    assert!(
        !from_capture(captured_at_ns),
        "the root's start must precede the capture for this test to mean anything"
    );
    assert!(from_capture(i64::MIN));

    // When: a failure sample with a check scoped to the root's own role is saved.
    let saved = fixture
        .call(
            "eval_save_sample",
            json!({
                "thread_id": "thread-1", "problem": "grep failed in the root",
                "checks": [{
                    "type": "tool_errors", "tool": "grep", "max": 0,
                    "scope": {"roles": ["Worker"]}, "calls": "direct",
                }],
            }),
        )
        .await;

    // Then: the root's role is known, so the check fails on the recording and the
    // sample is accepted.
    assert!(!saved.is_error, "{}", saved.content);
    let saved: Value = serde_json::from_str(&saved.content).unwrap();
    assert_eq!(saved["checks_on_recording"]["quality"], "fail");
}
