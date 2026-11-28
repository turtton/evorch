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

fn artifact(id: u8, title: &str, caption: Option<&str>) -> event_bus::PresentedArtifact {
    event_bus::PresentedArtifact {
        artifact_id: format!("artifact-{id:032x}"),
        title: title.into(),
        caption: caption.map(Into::into),
        media_type: "image/png".into(),
        path: "/store/blobs/never-published.png".into(),
        byte_len: 3,
        sha256: format!("{id:064x}"),
    }
}
fn presented(
    run: &str,
    title: Option<&str>,
    caption: Option<&str>,
    artifacts: Vec<event_bus::PresentedArtifact>,
) -> ToolEvent {
    ToolEvent::ArtifactsPresented {
        run_id: run.into(),
        presentation: event_bus::ArtifactPresentation {
            presentation_id: "presentation-1".into(),
            title: title.map(Into::into),
            caption: caption.map(Into::into),
            artifacts,
        },
    }
}
fn said(run: &str, text: &str) -> MessageEvent {
    MessageEvent::MessageCompleted {
        run_id: run.into(),
        text: text.into(),
    }
}
fn rebuild(db: &Database, run: &str) {
    db.conn
        .execute(
            "UPDATE history_run_state SET phase=0 WHERE run_id=?1",
            [run],
        )
        .unwrap();
    struct FrozenClock;
    impl super::build::Clock for FrozenClock {
        fn now_ns(&self) -> i64 {
            0
        }
    }
    while !db
        .history_build_step(run, super::build::BuildBudget::default(), &FrozenClock)
        .unwrap()
        .ready
    {}
}

// Given: a message, a presentation, a message, the same presentation again and an empty one
// Then: each presentation is its own row at its saved position, in the order its artifacts
//       were listed, whichever way the history is read or rebuilt; the stored path is not kept
#[test]
fn presentations_are_rows_at_their_saved_position() {
    let db = Database::open_in_memory().unwrap();
    scope(&db, &["run"]);
    let pair = || vec![artifact(2, "B", None), artifact(1, "A", Some("first"))];
    append(&db, said("run", "before"));
    append(&db, presented("run", Some("Compare"), Some("two"), pair()));
    append(&db, said("run", "after"));
    append(&db, presented("run", None, None, pair()));
    append(&db, presented("run", None, None, vec![]));
    let items = every_descriptor(&db, 1, Direction::Forward);
    let shape: Vec<_> = items
        .iter()
        .map(|d| (d.index, d.role, d.parts.clone(), d.tool.is_none()))
        .collect();
    assert_eq!(
        shape,
        [
            (0, Role::Assistant, vec![("before".into(), false)], true),
            (1, Role::Artifacts, vec![], true),
            (2, Role::Assistant, vec![("after".into(), false)], true),
            (3, Role::Artifacts, vec![], true),
            (4, Role::Artifacts, vec![], true),
        ]
    );
    let first = items[1].presentation.as_ref().unwrap();
    assert_eq!(first.title, Some(("Compare".into(), false)));
    assert_eq!(first.caption, Some(("two".into(), false)));
    let ids: Vec<_> = first
        .artifacts
        .iter()
        .map(|a| a.artifact_id.clone())
        .collect();
    assert_eq!(
        ids,
        [
            format!("artifact-{:032x}", 2),
            format!("artifact-{:032x}", 1)
        ]
    );
    assert_eq!(first.artifacts[1].digest, format!("{:064x}", 1));
    assert_eq!(first.artifacts[1].caption, Some(("first".into(), false)));
    assert_eq!(items[3].presentation.as_ref().unwrap().artifacts.len(), 2);
    assert!(items[4].presentation.as_ref().unwrap().artifacts.is_empty());
    assert!(items[0].presentation.is_none());
    assert!(
        !serde_json::to_string(&items)
            .unwrap()
            .contains("never-published")
    );
    assert_eq!(items, every_descriptor(&db, 50, Direction::Backward));
    rebuild(&db, "run");
    assert_eq!(items, every_descriptor(&db, 2, Direction::Forward));
}

// Given: strings a stored event holds as body tokens, a long caption and text that only
//        starts with the escape character
// Then: the row marks exactly those strings as tokens and reading a page reads no body
#[cfg(feature = "test-support")]
#[test]
fn tokenized_presentation_strings_stay_references() {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::StorageConfig {
        db_path: dir.path().join("presentation.db"),
        ..Default::default()
    };
    let db = Database::open(&config).unwrap();
    scope(&db, &["run"]);
    let long = "あ".repeat(1000);
    assert!(long.len() > 2048);
    let escaped = "\u{e000}evorch-body:v1:not a token";
    append(
        &db,
        presented(
            "run",
            Some(escaped),
            Some(&long),
            vec![artifact(1, "plain", Some(&long))],
        ),
    );
    let bodies = crate::test_support::meter_full_body_reads(&db);
    let items = every_descriptor(&db, 5, Direction::Forward);
    assert_eq!(bodies.counts(), Default::default());
    let row = items[0].presentation.as_ref().unwrap();
    let token = |text: &Option<(String, bool)>| {
        let (text, deferred) = text.as_ref().unwrap();
        assert!(deferred, "{text}");
        crate::BodyRef::from_token(text).expect("a well-formed token")
    };
    assert_eq!(token(&row.title).byte_len, escaped.len() as u64);
    assert_eq!(token(&row.caption).byte_len, long.len() as u64);
    assert_eq!(token(&row.artifacts[0].caption).byte_len, long.len() as u64);
    assert_eq!(row.artifacts[0].title, ("plain".into(), false));
}

// Given: an event held inline whose text starts with the escape character
// Then: it is not a token
#[test]
fn an_inline_event_has_no_tokens() {
    let kind = EventKind::Tool(presented(
        "run",
        Some("\u{e000}evorch-body:v1:text"),
        None,
        vec![],
    ));
    let row = rows::committed(&kind, false, None).unwrap();
    assert_eq!(row.role, Role::Artifacts);
    assert_eq!(
        row.presentation.unwrap().title,
        Some(("\u{e000}evorch-body:v1:text".into(), false))
    );
}

// Given: presentations with none, one and the most artifacts
// Then: the page budget grows with their number and exceeds the text a view may hold
#[test]
fn the_budget_of_a_presentation_covers_its_largest_view() {
    let row = |n: usize| rows::PresentationRow {
        title: None,
        caption: None,
        artifacts: (0..n)
            .map(|_| rows::PresentedRow {
                artifact_id: String::new(),
                digest: String::new(),
                title: (String::new(), false),
                caption: None,
                media_type: (String::new(), false),
            })
            .collect(),
    };
    let widest = rows::PRESENTATION_TITLE_BYTES * 2
        + rows::PRESENTATION_CAPTION_BYTES * 2
        + rows::PRESENTATION_MEDIA_TYPE_BYTES * 2
        + 105;
    let (none, one, eight) = (
        rows::presentation_public_bound(&row(0)),
        rows::presentation_public_bound(&row(1)),
        rows::presentation_public_bound(&row(8)),
    );
    assert!(none >= (rows::PRESENTATION_TITLE_BYTES + rows::PRESENTATION_CAPTION_BYTES) * 2);
    assert!(one - none >= widest);
    assert_eq!(eight - none, 8 * (one - none));
}
