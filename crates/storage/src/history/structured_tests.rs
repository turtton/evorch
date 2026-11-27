//! Tool rows, reasoning, notices and errors in the events-only index.
use super::read::*;
use super::rows::{self, ToolInput};
use super::{Reducer, Role};
use crate::{
    Database, HardLimits,
    repo::event::{EventAccounting, append_event},
};
use event_bus::{
    AgentRunPhase, DiagnosticEvent, DiagnosticSeverity, Event, EventKind, LifecycleEvent,
    MessageEvent, ProviderEvent, ProviderFailureKind, ToolEvent,
};
use serde_json::{Value, json};

fn scope(db: &Database, runs: &[&str]) {
    db.conn
        .execute(
            "INSERT INTO threads(thread_id,root_run_id,created_at_ns,updated_at_ns,source)
             VALUES ('thread',?1,0,0,'runtime')",
            [runs[0]],
        )
        .unwrap();
    for run in runs {
        db.conn
            .execute(
                "INSERT INTO thread_runs VALUES (?1,'thread',?2,0)",
                [run, &runs[0]],
            )
            .unwrap();
    }
}
fn append(db: &Database, kind: impl Into<EventKind>) -> i64 {
    append_event(
        &db.conn,
        None,
        &Event::new(kind),
        &HardLimits::default(),
        &mut EventAccounting::default(),
    )
    .unwrap()
    .id
}
fn started(run: &str, call: &str, name: &str, input: Option<Value>) -> ToolEvent {
    ToolEvent::ToolStarted {
        tool_name: name.into(),
        call_id: call.into(),
        input,
        run_id: Some(run.into()),
        parent_call_id: None,
    }
}
fn completed(run: &str, call: &str, name: &str, output: Option<&str>, is_error: bool) -> ToolEvent {
    ToolEvent::ToolCompleted {
        tool_name: name.into(),
        call_id: call.into(),
        is_error,
        output: output.map(Into::into),
        detail: None,
        run_id: Some(run.into()),
        parent_call_id: None,
    }
}
fn count(db: &Database, sql: &str) -> i64 {
    db.conn.query_row(sql, [], |r| r.get(0)).unwrap()
}
fn every_descriptor(db: &Database, limit: usize, direction: Direction) -> Vec<Descriptor> {
    let snapshot = db.history_snapshot("thread", None).unwrap().unwrap();
    let Availability::Indexed(prepared) = snapshot.prepare(snapshot.latest, direction).unwrap()
    else {
        panic!("the thread is not indexed");
    };
    let mut items = Vec::new();
    let mut boundary = None;
    loop {
        let page = prepared
            .page(direction, boundary.as_ref(), limit, 768 * 1024)
            .unwrap();
        assert!(!page.oversized);
        if direction == Direction::Backward {
            items.splice(0..0, page.items);
        } else {
            items.extend(page.items);
        }
        boundary = page.next;
        if boundary.is_none() {
            return items;
        }
    }
}

// Given: a completion with no output, then one with an empty output
// Then: both are rows, and the failure flag comes from the event
#[test]
fn completed_tool_without_output_is_preserved() {
    let db = Database::open_in_memory().unwrap();
    scope(&db, &["run"]);
    append(&db, completed("run", "a", "shell", None, true));
    append(&db, completed("run", "b", "shell", Some(""), false));
    let items = every_descriptor(&db, 50, Direction::Forward);
    let rows: Vec<_> = items
        .iter()
        .map(|d| (d.role, d.parts.clone(), d.tool.as_ref().unwrap().is_error))
        .collect();
    assert_eq!(
        rows,
        [
            (Role::Tool, vec![], true),
            (Role::Tool, vec![(String::new(), false)], false)
        ]
    );
}

// Given: two runs of one thread that reuse a call id, and a repeated start of one call
// Then: each completion takes the input of the latest start of its own run and call
#[test]
fn tool_metadata_is_scoped_by_run_and_call() {
    let db = Database::open_in_memory().unwrap();
    scope(&db, &["run-a", "run-b"]);
    append(
        &db,
        started("run-a", "c", "shell", Some(json!({"command": "ls"}))),
    );
    append(
        &db,
        started("run-b", "c", "shell", Some(json!({"command": "pwd"}))),
    );
    append(
        &db,
        started("run-a", "c", "shell", Some(json!({"command": "ls -l"}))),
    );
    append(&db, completed("run-b", "c", "shell", Some("/w"), false));
    append(&db, completed("run-a", "c", "shell", Some("x"), false));
    // The start was consumed: a second completion has no input.
    append(&db, completed("run-a", "c", "shell", Some("y"), false));
    let subjects: Vec<_> = every_descriptor(&db, 50, Direction::Forward)
        .into_iter()
        .map(|d| {
            let tool = d.tool.unwrap();
            (
                d.run,
                tool.input.map(|input| input.summary.unwrap_or_default()),
            )
        })
        .collect();
    assert_eq!(
        subjects,
        [
            ("run-b".to_owned(), Some("pwd".to_owned())),
            ("run-a".to_owned(), Some("ls -l".to_owned())),
            ("run-a".to_owned(), None),
        ]
    );
    assert_eq!(count(&db, "SELECT COUNT(*) FROM history_tool_starts"), 0);
}

// Given: a start long before its completion, and pages one row wide in both directions
// Then: the completion row restores its input from the start's source event
#[test]
fn tool_sources_before_page_boundary_are_restored() {
    let db = Database::open_in_memory().unwrap();
    scope(&db, &["run"]);
    append(
        &db,
        started(
            "run",
            "c",
            "run_js",
            Some(json!({"code": "console.log(1)\nconsole.log(2)"})),
        ),
    );
    for i in 0..5 {
        append(
            &db,
            MessageEvent::MessageCompleted {
                run_id: "run".into(),
                text: format!("between {i}"),
            },
        );
    }
    append(&db, completed("run", "c", "run_js", Some("2"), false));
    for direction in [Direction::Forward, Direction::Backward] {
        let items = every_descriptor(&db, 1, direction);
        assert_eq!(items.len(), 6);
        let tool = items[5].tool.as_ref().unwrap();
        let input = tool.input.as_ref().unwrap();
        assert_eq!(input.summary.as_deref(), Some("console.log(1)"));
        assert_eq!(
            input.parts,
            vec![("console.log(1)\nconsole.log(2)".to_owned(), false)],
            "a script is kept as written"
        );
        assert!(items[..5].iter().all(|d| d.tool.is_none()));
    }
}

// Given: reasoning, a provider failure, a switch, a pause and run state changes
// Then: exactly the listed ones are rows, in order, with the expected roles
#[test]
fn notices_errors_and_reasoning_are_rows_of_a_closed_set() {
    let db = Database::open_in_memory().unwrap();
    scope(&db, &["run"]);
    let request_failed = |detail: Option<&str>| ProviderEvent::RequestFailed {
        request_id: "r".into(),
        provider: "anthropic".into(),
        profile: None,
        protocol: "p".into(),
        model: "m".into(),
        streaming: true,
        duration_ms: 1,
        failure: ProviderFailureKind::Http { status: 529 },
        run_id: Some("run".into()),
        purpose: None,
        detail: detail.map(Into::into),
    };
    let state = |from, to, reason: Option<&str>| LifecycleEvent::AgentRunStateChanged {
        run_id: "run".into(),
        from,
        to,
        reason: reason.map(Into::into),
        waiting: None,
    };
    append(
        &db,
        MessageEvent::ReasoningCompleted {
            run_id: "run".into(),
            text: "thinking".into(),
        },
    );
    append(
        &db,
        MessageEvent::ReasoningCompleted {
            run_id: "run".into(),
            text: String::new(),
        },
    );
    append(&db, request_failed(Some("overloaded")));
    append(
        &db,
        ProviderEvent::FallbackTriggered {
            from_provider: "a".into(),
            from_model: Some("m1".into()),
            to_provider: "b".into(),
            to_model: "m2".into(),
            logical_model: "l".into(),
            session_id: "run".into(),
            failure: ProviderFailureKind::RateLimited,
            request_id: None,
        },
    );
    append(
        &db,
        ProviderEvent::CooldownStarted {
            profile: "a".into(),
            model: None,
            failure: ProviderFailureKind::Quota,
            cooldown_ms: Some(1500),
            run_id: Some("run".into()),
        },
    );
    // None of these is a row.
    append(
        &db,
        state(AgentRunPhase::Pending, AgentRunPhase::Running, None),
    );
    append(
        &db,
        state(AgentRunPhase::Running, AgentRunPhase::Done, None),
    );
    append(
        &db,
        DiagnosticEvent {
            source: "x".into(),
            severity: DiagnosticSeverity::Error,
            code: "Y".into(),
            detail: "z".into(),
            run_id: Some("run".into()),
            thread_id: None,
            call_id: None,
        },
    );
    // These are.
    append(
        &db,
        state(
            AgentRunPhase::Running,
            AgentRunPhase::Stopped,
            Some("by user"),
        ),
    );
    append(
        &db,
        state(AgentRunPhase::Running, AgentRunPhase::Error, Some("boom")),
    );
    let rows: Vec<_> = every_descriptor(&db, 50, Direction::Forward)
        .into_iter()
        .map(|d| (d.index, d.role, d.parts[0].0.clone(), d.tool.is_none()))
        .collect();
    assert_eq!(
        rows,
        [
            (0, Role::Reasoning, "thinking".into(), true),
            (
                1,
                Role::Error,
                "Request to anthropic (m) failed: HTTP 529: overloaded".into(),
                true
            ),
            (
                2,
                Role::Notice,
                "Switched from a/m1 to b/m2 after rate limited".into(),
                true
            ),
            (
                3,
                Role::Notice,
                "Paused a after quota exceeded for 2s".into(),
                true
            ),
            (4, Role::Notice, "Run stopped: by user".into(), true),
            (5, Role::Error, "Run failed: boom".into(), true),
        ]
    );
}

// Given: arguments of the common tools
// Then: the summary names the subject on one line, and a long value is never inlined
#[test]
fn input_summaries_name_the_subject_of_a_call() {
    let summary = |tool: &str, input: Value| {
        let (_, _, input) = rows::tool_start(
            &EventKind::Tool(started("run", "c", tool, Some(input))),
            false,
        )
        .unwrap();
        input.unwrap().summary
    };
    assert_eq!(
        summary("shell", json!({"command": "cargo test\n--lib"})).as_deref(),
        Some("cargo test")
    );
    assert_eq!(
        summary("shell", json!({"action": "poll", "job_id": "job-3"})).as_deref(),
        Some("poll job-3")
    );
    assert_eq!(
        summary("grep", json!({"pattern": "foo", "path": "src"})).as_deref(),
        Some("\"foo\" in src")
    );
    assert_eq!(
        summary("read", json!({"path": "a/b.rs", "offset": 3})).as_deref(),
        Some("a/b.rs")
    );
    assert_eq!(
        summary("web_fetch", json!({"url": "https://example.com"})).as_deref(),
        Some("https://example.com")
    );
    assert_eq!(
        summary("mystery", json!({"n": 2, "flag": true, "deep": {"x": 1}})).as_deref(),
        Some("flag=true, n=2")
    );
    assert_eq!(summary("mystery", json!({"deep": {"x": 1}})), None);
}

// Given: a tokenized start whose script and argument are long strings
// Then: the script is one deferred part, a nested string is a deferred part inside its
//       JSON, and no token reaches a summary
#[test]
fn long_arguments_stay_references() {
    let token = |len: u64| format!("\u{e000}evorch-body:v1:{:064x}:{len}", 7);
    assert!(crate::BodyRef::from_token(&token(5000)).is_some());
    let start = |tool: &str, input: Value| -> ToolInput {
        rows::tool_start(
            &EventKind::Tool(started("run", "c", tool, Some(input))),
            true,
        )
        .unwrap()
        .2
        .unwrap()
    };
    let js = start("run_js", json!({"code": token(5000)}));
    assert_eq!(js.parts, vec![(token(5000), true)]);
    assert_eq!(js.summary.as_deref(), Some("code (5000 bytes)"));
    let with_timeout = start("run_js", json!({"timeout_ms": 1000, "code": token(5000)}));
    assert_eq!(with_timeout.parts, js.parts);
    let other = start(
        "write",
        json!({"path": "f", "content": token(9000), "meta": [token(3000)]}),
    );
    assert_eq!(other.summary.as_deref(), Some("f"));
    assert!(other.parts.contains(&(token(9000), true)));
    assert!(other.parts.contains(&(token(3000), true)));
    let at = other
        .parts
        .iter()
        .position(|part| *part == (token(3000), true))
        .unwrap();
    assert!(other.parts[at - 1].0.ends_with("meta: [\""));
    assert!(other.parts[at + 1].0.starts_with("\"]"));
    assert!(
        other
            .parts
            .iter()
            .filter(|(_, deferred)| !deferred)
            .all(|(text, _)| !text.contains("evorch-body")),
        "a token only ever appears as a deferred part"
    );
}

// Given: a reducer fed reasoning
// Then: empty reasoning leaves no row and never takes the place of the last reply
#[test]
fn reasoning_neither_counts_as_a_reply_nor_survives_empty() {
    let mut reducer = Reducer::default();
    let kind = |text: &str| {
        EventKind::Message(MessageEvent::ReasoningCompleted {
            run_id: "run".into(),
            text: text.into(),
        })
    };
    assert!(reducer.observe(&kind(""), false, &mut None, None).is_none());
    let row = reducer
        .observe(&kind("why"), false, &mut None, None)
        .unwrap();
    assert_eq!((row.index, row.role), (0, Role::Reasoning));
    assert!(reducer.last_assistant.is_none());
}

// The persisted clocks, not process sessions or tool names, determine elapsed time.
#[test]
fn tool_duration_survives_rebuild_and_missing_or_backwards_start_is_unknown() {
    use std::time::{Duration, UNIX_EPOCH};
    let db = Database::open_in_memory().unwrap();
    scope(&db, &["run"]);
    let at = |kind: ToolEvent, millis: u64| {
        let mut event = Event::new(kind);
        event.meta.wall_clock = UNIX_EPOCH + Duration::from_millis(millis);
        append_event(
            &db.conn,
            None,
            &event,
            &HardLimits::default(),
            &mut EventAccounting::default(),
        )
        .unwrap();
    };
    at(
        started("run", "normal", "shell", Some(json!({"command":"true"}))),
        1000,
    );
    at(completed("run", "normal", "shell", None, false), 2240);
    at(started("run", "no-input", "wait", None), 3000);
    at(completed("run", "no-input", "wait", None, true), 3100);
    at(started("run", "backwards", "run_js", None), 5000);
    at(completed("run", "backwards", "run_js", None, false), 4000);
    at(completed("run", "missing", "delegate", None, false), 6000);
    let before = every_descriptor(&db, 1, Direction::Forward);
    let durations: Vec<_> = before
        .iter()
        .map(|d| d.tool.as_ref().unwrap().duration_ms)
        .collect();
    assert_eq!(durations, [Some(1240), Some(100), None, None]);
    db.conn
        .execute(
            "UPDATE history_run_state SET phase=0 WHERE run_id='run'",
            [],
        )
        .unwrap();
    struct FrozenClock;
    impl super::build::Clock for FrozenClock {
        fn now_ns(&self) -> i64 {
            0
        }
    }
    loop {
        if db
            .history_build_step("run", super::build::BuildBudget::default(), &FrozenClock)
            .unwrap()
            .ready
        {
            break;
        }
    }
    assert_eq!(before, every_descriptor(&db, 1, Direction::Backward));
    assert_eq!(
        rows::duration_ms(i64::MIN, i64::MAX),
        Some(18_446_744_073_709)
    );
    assert_eq!(rows::duration_ms(1, 0), None);
    assert_eq!(rows::duration_ms(0, 999_999), Some(0));
}
