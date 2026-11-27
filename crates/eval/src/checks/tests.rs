use event_bus::{DiagnosticEvent, DiagnosticSeverity, event::diagnostic_codes};

use super::*;

fn started(run_id: &str, role: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run_id.into(),
        parent_run_id: (run_id != "root").then(|| "root".to_owned()),
        agent_name: format!("{role}-agent"),
        role: role.into(),
    })
}

fn call_started(run: Option<&str>, call: &str, tool: &str, parent: Option<&str>) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: tool.into(),
        call_id: call.into(),
        input: None,
        run_id: run.map(str::to_owned),
        parent_call_id: parent.map(str::to_owned),
    })
}

fn call_completed(
    run: Option<&str>,
    call: &str,
    tool: &str,
    is_error: bool,
    parent: Option<&str>,
) -> Event {
    Event::new(ToolEvent::ToolCompleted {
        tool_name: tool.into(),
        call_id: call.into(),
        is_error,
        output: None,
        detail: None,
        run_id: run.map(str::to_owned),
        parent_call_id: parent.map(str::to_owned),
    })
}

/// A call that started and ended.
fn call(run: &str, id: &str, tool: &str, is_error: bool) -> Vec<Event> {
    vec![
        call_started(Some(run), id, tool, None),
        call_completed(Some(run), id, tool, is_error, None),
    ]
}

fn diagnostic(run: Option<&str>, code: &str) -> Event {
    Event::new(DiagnosticEvent {
        source: "test".into(),
        severity: DiagnosticSeverity::Warning,
        code: code.into(),
        detail: "detail".into(),
        run_id: run.map(str::to_owned),
        thread_id: None,
        call_id: None,
    })
}

fn sample(kind: SampleKind, checks: Vec<Check>) -> EvalSample {
    EvalSample {
        kind,
        checks,
        ..EvalSample::new("thread".into(), "problem".into(), Vec::new(), 1)
    }
}

fn successes(tool: &str, min: Option<u32>, max: Option<u32>) -> Check {
    Check::SuccessfulCalls {
        tool: tool.into(),
        min,
        max,
        scope: CheckScope::Tree,
        calls: CallRange::Direct,
    }
}

fn errors(tool: &str, max: u32, scope: CheckScope, calls: CallRange) -> Check {
    Check::ToolErrors {
        tool: tool.into(),
        max,
        scope,
        calls,
    }
}

fn observe(events: &[Event]) -> Observation<'_> {
    Observation {
        events,
        truncated: false,
        status: None,
        evidence_missing: false,
    }
}

fn verdicts(sample: &EvalSample, observation: &Observation<'_>) -> Vec<Verdict> {
    grade(sample, observation)
        .results
        .iter()
        .map(|result| result.verdict)
        .collect()
}

fn ended(run_id: &str) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run_id.into(),
        from: event_bus::AgentRunPhase::Running,
        to: event_bus::AgentRunPhase::Done,
        reason: None,
        waiting: None,
    })
}

/// A finished root and worker, and `calls` made in between.
fn tree_with(calls: Vec<Event>) -> Vec<Event> {
    let mut events = vec![started("root", "orchestrator"), started("w1", "worker")];
    events.extend(calls);
    events.extend([ended("w1"), ended("root")]);
    events
}

#[test]
fn success_bounds_are_inclusive_on_both_sides() {
    // Given: a thread with exactly two successful reads.
    let events = tree_with(
        [
            call("w1", "a", "read", false),
            call("w1", "b", "read", false),
        ]
        .concat(),
    );
    let exact = sample(
        SampleKind::Regression,
        vec![successes("read", Some(2), Some(2))],
    );
    let too_many = sample(SampleKind::Failure, vec![successes("read", None, Some(1))]);
    let too_few = sample(SampleKind::Failure, vec![successes("read", Some(3), None)]);

    // When/Then: the boundary passes and one step beyond either side fails.
    assert_eq!(verdicts(&exact, &observe(&events)), [Verdict::Pass]);
    assert_eq!(verdicts(&too_many, &observe(&events)), [Verdict::Fail]);
    assert_eq!(verdicts(&too_few, &observe(&events)), [Verdict::Fail]);
}

#[test]
fn error_and_diagnostic_limits_are_inclusive() {
    // Given: one failed shell call and one NoProgress diagnostic.
    let mut events = tree_with(call("w1", "a", "shell", true));
    events.push(diagnostic(Some("w1"), diagnostic_codes::NO_PROGRESS));
    let limits = |max| {
        vec![
            errors("shell", max, CheckScope::Tree, CallRange::Direct),
            Check::Diagnostics {
                code: diagnostic_codes::NO_PROGRESS.into(),
                max,
                scope: CheckScope::Tree,
            },
        ]
    };
    let at_limit = sample(SampleKind::Regression, limits(1));
    let over_limit = sample(SampleKind::Failure, limits(0));

    // When/Then: at the limit passes, one over fails, with the occurrence as evidence.
    assert_eq!(
        verdicts(&at_limit, &observe(&events)),
        [Verdict::Pass, Verdict::Pass]
    );
    let grading = grade(&over_limit, &observe(&events));
    assert_eq!(
        grading
            .results
            .iter()
            .map(|result| result.verdict)
            .collect::<Vec<_>>(),
        [Verdict::Fail, Verdict::Fail]
    );
    assert_eq!(grading.results[0].evidence[0].call_id.as_deref(), Some("a"));
    assert_eq!(grading.results[1].evidence[0].run_id.as_deref(), Some("w1"));
}

#[test]
fn a_successful_call_needs_its_terminal_event() {
    // Given: a read that started and never ended, and a check for one success.
    let events = tree_with(vec![call_started(Some("w1"), "a", "read", None)]);
    let check = sample(
        SampleKind::Regression,
        vec![successes("read", Some(1), None)],
    );

    // When: it is graded.
    let grading = grade(&check, &observe(&events));

    // Then: the start does not count as a success and the gap is named.
    assert_eq!(grading.results[0].verdict, Verdict::Indeterminate);
    assert_eq!(grading.results[0].observed, 0);
    assert!(grading.results[0].reason.contains("no terminal event"));
    assert!(!grading.validity.valid);
}

#[test]
fn calls_inside_run_js_count_only_when_the_check_includes_children() {
    // Given: a run_js call whose child read succeeded, and a direct read.
    let mut events = tree_with(vec![
        call_started(Some("w1"), "js", "run_js", None),
        call_started(Some("w1"), "child", "read", Some("js")),
        call_completed(Some("w1"), "child", "read", false, Some("js")),
        call_completed(Some("w1"), "js", "run_js", false, None),
    ]);
    events.extend(call("w1", "direct", "read", false));
    let observed = |tool: &str, calls| {
        let check = sample(
            SampleKind::Failure,
            vec![Check::SuccessfulCalls {
                tool: tool.into(),
                min: None,
                max: Some(0),
                scope: CheckScope::Tree,
                calls,
            }],
        );
        grade(&check, &observe(&events)).results[0].observed
    };

    // When/Then: children are counted at their own tool, once, and never also under the parent.
    assert_eq!(observed("read", CallRange::Direct), 1);
    assert_eq!(observed("read", CallRange::IncludingChildren), 2);
    assert_eq!(observed("run_js", CallRange::IncludingChildren), 1);
}

#[test]
fn a_duplicated_event_is_counted_once() {
    // Given: the same call and the same diagnostic recorded twice.
    let completed = call_completed(Some("w1"), "a", "shell", true, None);
    let repeated = diagnostic(Some("w1"), diagnostic_codes::NO_PROGRESS);
    let mut events = tree_with(vec![
        call_started(Some("w1"), "a", "shell", None),
        call_started(Some("w1"), "a", "shell", None),
        completed.clone(),
        completed,
    ]);
    events.extend([repeated.clone(), repeated]);
    let check = sample(
        SampleKind::Failure,
        vec![
            errors("shell", 0, CheckScope::Tree, CallRange::Direct),
            Check::Diagnostics {
                code: diagnostic_codes::NO_PROGRESS.into(),
                max: 0,
                scope: CheckScope::Tree,
            },
        ],
    );

    // When/Then: each counts once.
    let grading = grade(&check, &observe(&events));
    assert_eq!(grading.results[0].observed, 1);
    assert_eq!(grading.results[1].observed, 1);
}

#[test]
fn a_role_scope_counts_only_those_roles_and_needs_the_role_to_have_run() {
    // Given: an explorer error, and no worker at all.
    let mut events = vec![started("root", "orchestrator"), started("x1", "explorer")];
    events.extend(call("x1", "a", "shell", true));
    events.extend([ended("x1"), ended("root")]);
    let worker = sample(
        SampleKind::Regression,
        vec![errors(
            "shell",
            0,
            CheckScope::Roles(vec!["Worker".into()]),
            CallRange::Direct,
        )],
    );
    let explorer = sample(
        SampleKind::Failure,
        vec![errors(
            "shell",
            0,
            CheckScope::Roles(vec!["roles.explorer".into()]),
            CallRange::Direct,
        )],
    );

    // When/Then: no pass is given for the worker; the explorer's error fails its scope.
    let grading = grade(&worker, &observe(&events));
    assert_eq!(grading.results[0].verdict, Verdict::Indeterminate);
    assert!(grading.results[0].reason.contains("ever ran"));
    assert_eq!(verdicts(&explorer, &observe(&events)), [Verdict::Fail]);
}

#[test]
fn a_call_whose_run_has_no_role_cannot_be_placed_in_a_role_scope() {
    // Given: a legacy event with no run id.
    let mut events = tree_with(Vec::new());
    events.push(call_started(None, "a", "shell", None));
    events.push(call_completed(None, "a", "shell", true, None));
    let in_role = sample(
        SampleKind::Regression,
        vec![errors(
            "shell",
            0,
            CheckScope::Roles(vec!["worker".into()]),
            CallRange::Direct,
        )],
    );
    let tree = sample(
        SampleKind::Failure,
        vec![errors("shell", 0, CheckScope::Tree, CallRange::Direct)],
    );

    // When/Then: the role scope cannot decide; the tree scope still sees the error.
    assert_eq!(
        verdicts(&in_role, &observe(&events)),
        [Verdict::Indeterminate]
    );
    assert_eq!(verdicts(&tree, &observe(&events)), [Verdict::Fail]);
}

#[test]
fn an_incomplete_record_never_passes_by_absence_but_can_still_fail() {
    // Given: a clean record and a failing one, both read with a limit that cut them.
    let clean = tree_with(call("w1", "a", "read", false));
    let broken = tree_with(call("w1", "a", "shell", true));
    let max_errors = sample(
        SampleKind::Regression,
        vec![errors("shell", 0, CheckScope::Tree, CallRange::Direct)],
    );
    let truncated = |events| Observation {
        events,
        truncated: true,
        status: None,
        evidence_missing: false,
    };

    // When/Then: zero errors is indeterminate on a cut record; an observed error still fails.
    assert_eq!(
        verdicts(&max_errors, &truncated(&clean)),
        [Verdict::Indeterminate]
    );
    assert_eq!(verdicts(&max_errors, &truncated(&broken)), [Verdict::Fail]);
}

#[test]
fn passing_checks_on_an_unfinished_replay_are_not_a_quality_pass() {
    // Given: a replay that stopped early after a successful read.
    let events = tree_with(call("w1", "a", "read", false));
    let min_only = sample(
        SampleKind::Regression,
        vec![successes("read", Some(1), None)],
    );
    let stopped = Observation {
        events: &events,
        truncated: false,
        status: Some(Status::Incomplete),
        evidence_missing: false,
    };
    let finished = Observation {
        status: Some(Status::Completed),
        ..stopped
    };

    // When: the same check is graded on both.
    let early = grade(&min_only, &stopped);
    let done = grade(&min_only, &finished);

    // Then: the check passes in both, but only the finished replay has a quality pass.
    assert_eq!(early.verdict(), Some(Verdict::Pass));
    assert_eq!(early.quality(), Quality::ReplayInvalid);
    assert_eq!(done.quality(), Quality::Pass);
    // And: an upper bound is not concluded from an unfinished replay.
    let max_only = sample(
        SampleKind::Regression,
        vec![successes("read", None, Some(3))],
    );
    assert_eq!(verdicts(&max_only, &stopped), [Verdict::Indeterminate]);
}

#[test]
fn a_sample_without_checks_has_no_machine_judgment() {
    let events = tree_with(Vec::new());

    let grading = grade(&sample(SampleKind::Failure, Vec::new()), &observe(&events));

    assert_eq!(grading.verdict(), None);
    assert_eq!(grading.quality(), Quality::NoMachineJudgment);
}

#[test]
fn checks_of_another_grading_version_are_not_graded() {
    let events = tree_with(call("w1", "a", "read", false));
    let foreign = EvalSample {
        check_version: CHECK_VERSION + 1,
        ..sample(
            SampleKind::Regression,
            vec![successes("read", Some(1), None)],
        )
    };

    assert_eq!(
        verdicts(&foreign, &observe(&events)),
        [Verdict::Indeterminate]
    );
}

#[test]
fn evidence_is_bounded() {
    let calls: Vec<Event> = (0..20)
        .flat_map(|index| call("w1", &format!("c{index}"), "shell", true))
        .collect();
    let events = tree_with(calls);
    let check = sample(
        SampleKind::Failure,
        vec![errors("shell", 0, CheckScope::Tree, CallRange::Direct)],
    );

    let result = &grade(&check, &observe(&events)).results[0];

    assert_eq!(result.observed, 20);
    assert_eq!(result.evidence.len(), EVIDENCE_MAX);
    assert_eq!(result.evidence_omitted, 20 - EVIDENCE_MAX as u32);
}

#[test]
fn a_failure_sample_is_accepted_only_when_a_check_fails_on_the_recording() {
    // Given: a recording with one shell error.
    let events = tree_with(call("w1", "a", "shell", true));
    let detects = sample(
        SampleKind::Failure,
        vec![errors("shell", 0, CheckScope::Tree, CallRange::Direct)],
    );
    let misses = sample(
        SampleKind::Failure,
        vec![errors("read", 0, CheckScope::Tree, CallRange::Direct)],
    );
    let unchecked = sample(SampleKind::Failure, Vec::new());

    // When/Then: only checks that catch it are accepted; no checks is allowed.
    assert!(accept(&detects, &grade(&detects, &observe(&events))).is_ok());
    let refusal = accept(&misses, &grade(&misses, &observe(&events))).unwrap_err();
    assert!(refusal.contains("cannot detect the failure"), "{refusal}");
    assert!(accept(&unchecked, &grade(&unchecked, &observe(&events))).is_ok());
}

#[test]
fn a_regression_sample_needs_every_check_to_pass_on_a_complete_recording() {
    // Given: a complete recording and one that is cut.
    let events = tree_with(call("w1", "a", "read", false));
    let passing = sample(
        SampleKind::Regression,
        vec![successes("read", Some(1), None)],
    );
    let failing = sample(
        SampleKind::Regression,
        vec![successes("read", Some(2), None)],
    );
    let cut = Observation {
        events: &events,
        truncated: true,
        status: None,
        evidence_missing: false,
    };

    // When/Then: it is accepted only when everything passes on the whole record.
    assert!(accept(&passing, &grade(&passing, &observe(&events))).is_ok());
    assert!(accept(&failing, &grade(&failing, &observe(&events))).is_err());
    let refusal = accept(&passing, &grade(&passing, &cut)).unwrap_err();
    assert!(refusal.contains("not a complete account"), "{refusal}");
}

fn database_with(events: &[Event]) -> (tempfile::TempDir, storage::Database) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evorch.db");
    crate::test_support::write_run_db(&path, events, Vec::new());
    let database = storage::Database::open_read_only(&path).unwrap();
    (dir, database)
}

#[test]
fn source_references_must_exist_and_belong_to_the_thread() {
    // Given: a database with a run of this thread and an event of another run.
    let (_dir, database) =
        database_with(&[started("root", "orchestrator"), started("other", "worker")]);
    let stored = database.events_all_ordered().unwrap();
    let id_of = |run: &str| {
        stored
            .iter()
            .find(|stored| stored.event.primary_run_id() == Some(run))
            .map(|stored| stored.id)
            .unwrap()
    };
    let runs = vec!["root".to_owned()];
    let verify = |reference| verify_sources(&database, &runs, &[reference]);

    // When/Then: the thread's run and event are accepted; strangers are refused.
    assert!(
        verify(SourceRef::Run {
            run_id: "root".into()
        })
        .is_ok()
    );
    assert!(
        verify(SourceRef::Run {
            run_id: "other".into()
        })
        .is_err()
    );
    assert!(
        verify(SourceRef::Event {
            event_id: id_of("root")
        })
        .is_ok()
    );
    assert!(
        verify(SourceRef::Event {
            event_id: id_of("other")
        })
        .is_err()
    );
    assert!(verify(SourceRef::Event { event_id: 9_999 }).is_err());
    assert!(
        verify(SourceRef::Improvement {
            candidate_id: "missing".into()
        })
        .is_err()
    );
}

#[test]
fn missing_evidence_never_passes_an_absence_check() {
    // Given: no events because the recorded database was not there.
    let missing = Observation {
        events: &[],
        truncated: false,
        status: Some(Status::Completed),
        evidence_missing: true,
    };
    let checks = sample(
        SampleKind::Regression,
        vec![
            errors("shell", 0, CheckScope::Tree, CallRange::Direct),
            successes("read", None, Some(0)),
            Check::Diagnostics {
                code: diagnostic_codes::NO_PROGRESS.into(),
                max: 0,
                scope: CheckScope::Tree,
            },
        ],
    );

    // When: the checks are graded.
    let grading = grade(&checks, &missing);

    // Then: nothing is concluded, and the replay is not a valid record.
    assert_eq!(
        grading
            .results
            .iter()
            .map(|result| result.verdict)
            .collect::<Vec<_>>(),
        [Verdict::Indeterminate; 3]
    );
    assert!(grading.results[0].reason.contains("evidence is missing"));
    assert!(!grading.validity.valid);
    assert_ne!(grading.quality(), Quality::Pass);
}

#[test]
fn an_empty_record_or_one_with_only_run_starts_is_not_a_valid_passing_record() {
    let absence = sample(
        SampleKind::Regression,
        vec![errors("shell", 0, CheckScope::Tree, CallRange::Direct)],
    );
    fn finished(events: &[Event]) -> Observation<'_> {
        Observation {
            events,
            truncated: false,
            status: Some(Status::Completed),
            evidence_missing: false,
        }
    }

    // Given: a database that exists but holds no event.
    let empty = grade(&absence, &finished(&[]));
    // Given: a record with the start of the runs and nothing after it.
    let starts = [started("root", "orchestrator"), started("w1", "worker")];
    let only_starts = grade(&absence, &finished(&starts));

    // Then: both are indeterminate and invalid, never a quality pass.
    for grading in [&empty, &only_starts] {
        assert_eq!(
            grading.results[0].verdict,
            Verdict::Indeterminate,
            "{grading:?}"
        );
        assert!(!grading.validity.valid, "{grading:?}");
        assert_ne!(grading.quality(), Quality::Pass);
    }
    assert!(empty.validity.issues[0].contains("no run start"));
    assert!(only_starts.validity.issues[0].contains("no recorded end"));
}
