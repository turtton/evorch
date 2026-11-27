//! Tool rows, reasoning, notices and errors as clients read and receive them.
use super::*;
use event_bus::{AgentRunPhase, LifecycleEvent, ProviderEvent, ProviderFailureKind, ToolEvent};
use workbench_protocol::views::{ToolCallStatus, ToolCallView};

fn save(f: &Fixture, kind: impl Into<event_bus::EventKind>) {
    f.store
        .handle()
        .append_event(None, &Event::new(kind))
        .unwrap();
}
fn started(call: &str, name: &str, input: serde_json::Value) -> ToolEvent {
    ToolEvent::ToolStarted {
        tool_name: name.into(),
        call_id: call.into(),
        input: Some(input),
        run_id: Some("run-1".into()),
        parent_call_id: None,
    }
}
fn completed(call: &str, name: &str, output: Option<String>, is_error: bool) -> ToolEvent {
    ToolEvent::ToolCompleted {
        tool_name: name.into(),
        call_id: call.into(),
        is_error,
        output,
        detail: None,
        run_id: Some("run-1".into()),
        parent_call_id: None,
    }
}
fn read_all(f: &Fixture, direction: HistoryDirection, indexed: bool) -> Vec<CommittedMessage> {
    let mut items = Vec::new();
    let mut cursor = None;
    loop {
        let Query::Conversation(ConversationQuery::ReadCommittedHistory(mut q)) =
            history(PageRequest { cursor, limit: 3 })
        else {
            panic!()
        };
        q.direction = direction;
        let QueryReply::Conversation(ConversationReply::CommittedHistory(page)) = f.query(
            Query::Conversation(ConversationQuery::ReadCommittedHistory(q)),
        ) else {
            panic!()
        };
        assert_eq!(
            f.host.projection().history_route() == Some(HistoryRoute::Indexed),
            indexed
        );
        if direction == HistoryDirection::Backward {
            items.splice(0..0, page.items);
        } else {
            items.extend(page.items);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            return items;
        }
    }
}
fn body_text(f: &Fixture, content: &MessageContent) -> String {
    let MessageContent::Body(reference) = content else {
        panic!("expected a deferred body")
    };
    let mut text = String::new();
    let mut offset = ByteOffset(0);
    loop {
        let QueryReply::Projection(ProjectionReply::Body(chunk)) =
            f.query(Query::Projection(ProjectionQuery::ReadBody(ReadBody {
                body_id: reference.body_id.clone(),
                offset,
                max_bytes: 65536,
            })))
        else {
            panic!()
        };
        text.push_str(&chunk.text);
        match chunk.next_offset {
            Some(next) => offset = next,
            None => return text,
        }
    }
}
fn inline(text: &str) -> MessageContent {
    MessageContent::Inline { text: text.into() }
}
fn tool(message: &CommittedMessage) -> &ToolCallView {
    assert_eq!(message.role, MessageRole::Tool);
    message.tool.as_ref().unwrap()
}

fn seed_every_kind(f: &Fixture) {
    save(
        f,
        started("call-1", "shell", serde_json::json!({"command": "ls -l"})),
    );
    save(
        f,
        MessageEvent::ReasoningCompleted {
            run_id: "run-1".into(),
            text: "look first".into(),
        },
    );
    save(
        f,
        MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: "listing".into(),
        },
    );
    save(f, completed("call-1", "shell", Some("a\nb".into()), false));
    save(
        f,
        started(
            "call-2",
            "grep",
            serde_json::json!({"pattern": "x", "path": "src"}),
        ),
    );
    save(
        f,
        ToolEvent::ToolCompleted {
            tool_name: "grep".into(),
            call_id: "call-2".into(),
            is_error: true,
            output: None,
            detail: Some(serde_json::json!({"request_id": "r-9"})),
            run_id: Some("run-1".into()),
            parent_call_id: Some("call-1".into()),
        },
    );
    save(
        f,
        ProviderEvent::RequestFailed {
            request_id: "r".into(),
            provider: "anthropic".into(),
            profile: None,
            protocol: "p".into(),
            model: "m".into(),
            streaming: true,
            duration_ms: 1,
            failure: ProviderFailureKind::Timeout,
            run_id: Some("run-1".into()),
            purpose: None,
            detail: None,
        },
    );
    save(
        f,
        LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Error,
            reason: Some("gave up".into()),
            waiting: None,
        },
    );
}

// Given: a thread with every kind of structured row
// Then: the indexed route, the legacy route and the live stream describe the same rows
#[test]
fn indexed_and_live_structured_history_agree() {
    let f = current_thread_fixture();
    let mut sub = f.subscribe();
    seed_every_kind(&f);
    let forward = read_all(&f, HistoryDirection::Forward, true);
    let live: Vec<_> = (0..forward.len())
        .map(|_| next_message(&f, &mut sub))
        .collect();
    assert_eq!(read_all(&f, HistoryDirection::Backward, true), forward);
    dirty(&f);
    assert_eq!(read_all(&f, HistoryDirection::Forward, false), forward);
    assert_eq!(read_all(&f, HistoryDirection::Backward, false), forward);
    let json = |rows: &[CommittedMessage]| serde_json::to_value(rows).unwrap();
    assert_eq!(json(&live), json(&forward));

    let roles: Vec<_> = forward.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            MessageRole::Reasoning,
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::Tool,
            MessageRole::Error,
            MessageRole::Error,
        ]
    );
    let first = tool(&forward[2]);
    assert_eq!(first.tool_name, "shell");
    assert_eq!(first.input_summary.as_deref(), Some("ls -l"));
    assert_eq!(first.input, Some(inline("command: ls -l")));
    assert_eq!(first.status, ToolCallStatus::Succeeded);
    assert_eq!(forward[2].content, inline("a\nb"));
    let second = tool(&forward[3]);
    assert_eq!(second.status, ToolCallStatus::Failed);
    assert_eq!(second.parent_call_id.as_deref(), Some("call-1"));
    assert_eq!(second.input_summary.as_deref(), Some("\"x\" in src"));
    assert_eq!(forward[3].content, inline(""));
    assert!(second.detail.is_some());
    assert_eq!(forward[0].content, inline("look first"));
    assert!(forward.iter().all(|m| m.validate().is_ok()));
}

// Given: a large script and a secret in an argument of a completed call, with a page of
//        one row
// Then: reading pages resolves no blob, the script comes back as written and the secret
//       is masked
#[test]
fn run_js_input_preserves_source_and_redacts_secrets() {
    let f = current_thread_fixture();
    let secret = format!("sk-{}", "a".repeat(32));
    let code = format!("// {secret}\n{}", "console.log(1);\n".repeat(500));
    save(
        &f,
        started(
            "js",
            "run_js",
            serde_json::json!({"code": code, "timeout_ms": 1000}),
        ),
    );
    save(&f, completed("js", "run_js", Some("ok".into()), false));
    let meter = read_meter(&f);
    let rows = read_all(&f, HistoryDirection::Forward, true);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        meter.counts(),
        storage::test_support::BodyReadCounts::default(),
        "a page must not resolve large inputs"
    );
    let input = tool(&rows[0]).input.clone().unwrap();
    let shown = body_text(&f, &input);
    assert!(!shown.contains(&secret));
    assert!(shown.contains("[REDACTED:openai-style-key]"));
    assert!(shown.ends_with("console.log(1);\n"));
    assert!(
        shown.starts_with("// ") && !shown.contains("timeout_ms"),
        "the script is shown as written, not as an argument list: {shown:.40}"
    );
    assert_eq!(shown.matches("console.log(1);").count(), 500);
    assert!(
        tool(&rows[0])
            .input_summary
            .as_deref()
            .is_some_and(|s| s.starts_with("code ("))
    );
}

// Given: a small script run with a timeout argument
// Then: the input is the script exactly (the secret masked), whatever else the call sets
#[test]
fn run_js_input_is_the_script_even_with_other_arguments() {
    let f = current_thread_fixture();
    let secret = format!("sk-{}", "b".repeat(32));
    let code = format!("// {secret}\nreturn 1;\n");
    save(
        &f,
        started(
            "js",
            "run_js",
            serde_json::json!({"timeout_ms": 1000, "code": code}),
        ),
    );
    save(&f, completed("js", "run_js", Some("1".into()), false));
    for (indexed, dirty_first) in [(true, false), (false, true)] {
        if dirty_first {
            dirty(&f);
        }
        let rows = read_all(&f, HistoryDirection::Forward, indexed);
        let input = tool(&rows[0]).input.clone().unwrap();
        let MessageContent::Inline { text } = &input else {
            panic!("a small script stays inline")
        };
        assert!(text.ends_with("\nreturn 1;\n") && text.starts_with("// "));
        assert!(!text.contains("timeout_ms") && !text.contains(&secret));
    }
}

// Given: nested edit strings with JSON-sensitive characters around the tokenization
//       threshold, and a live input large enough to require a Body itself
// Then: live and actual committed inputs read back identically on both routes,
//       while neither observation nor paging resolves a stored blob
#[test]
fn nested_input_strings_match_live_before_and_after_tokenization() {
    use workbench_protocol::namespaces::ExecutionEvent;
    for bytes in [2047, 2048, 2049, 20 * 1024] {
        let f = current_thread_fixture();
        let special = "α\n\"quoted\"\\path\t\r\u{0001}";
        let first = format!("{special}{}", "x".repeat(bytes - special.len()));
        let second = first.replace("quoted", "second");
        assert_eq!(first.len(), bytes);
        let meter = read_meter(&f);
        let before = meter.counts();
        let mut sub = f.subscribe();
        let start = Event::new(started(
            "edit",
            "edit",
            serde_json::json!({
                "path": "src/lib.rs",
                "edits": [
                    {"old_string": "a\n\"before\"", "new_string": first},
                    {"old_string": "b\\before", "new_string": second},
                ],
            }),
        ));
        f.store.handle().append_event(None, &start).unwrap();
        f.runtime.event_bus().emit(start);
        let live = f.rt.block_on(async {
            loop {
                let envelope = sub.recv().await.unwrap();
                if let UiEvent::Execution(ExecutionEvent::ToolStarted { tool }) = envelope.event {
                    break tool;
                }
            }
        });
        assert_eq!(meter.counts(), before, "observing a start resolves no blob");
        let read = |input: &MessageContent| match input {
            MessageContent::Inline { text } => text.clone(),
            MessageContent::Body(_) => body_text(&f, input),
        };
        let live_input = live.input.as_ref().unwrap();
        assert_eq!(
            matches!(live_input, MessageContent::Body(_)),
            bytes == 20 * 1024
        );
        let shown_live = read(live_input);
        save(&f, completed("edit", "edit", Some("ok".into()), false));
        for (indexed, dirty_first) in [(true, false), (false, true)] {
            if dirty_first {
                dirty(&f);
            }
            let before = meter.counts();
            let rows = read_all(&f, HistoryDirection::Forward, indexed);
            assert_eq!(meter.counts(), before, "paging resolves no nested blob");
            assert_eq!(rows.len(), 1);
            let input = tool(&rows[0]).input.as_ref().unwrap();
            assert_eq!(matches!(input, MessageContent::Body(_)), bytes >= 2048);
            let shown = read(input);
            assert_eq!(shown, shown_live, "bytes={bytes}, indexed={indexed}");
            if bytes >= 2048 {
                assert!(shown.contains(&first) && shown.contains(&second));
            }
        }
    }
}

fn long_id(fill: char, len: usize) -> String {
    std::iter::repeat_n(fill, len).collect()
}

// Given: tool call ids at the protocol limit, and two ids that only differ after it
// Then: the limit's ids come back whole with their parent link; an id past it is refused
//       with a structured error and never shortened into another call's id
#[test]
fn tool_call_ids_are_kept_whole_or_refused() {
    let limit = workbench_protocol::limits::MAX_NAME_BYTES;
    let (call, parent) = (long_id('a', limit), long_id('p', limit));
    let f = current_thread_fixture();
    save(
        &f,
        started(&call, "shell", serde_json::json!({"command": "ls"})),
    );
    save(
        &f,
        ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: call.clone(),
            is_error: false,
            output: Some("x".into()),
            detail: None,
            run_id: Some("run-1".into()),
            parent_call_id: Some(parent.clone()),
        },
    );
    for (indexed, dirty_first) in [(true, false), (false, true)] {
        if dirty_first {
            dirty(&f);
        }
        let rows = read_all(&f, HistoryDirection::Forward, indexed);
        let view = tool(&rows[0]);
        assert_eq!(view.call_id, call);
        assert_eq!(view.parent_call_id.as_deref(), Some(parent.as_str()));
    }

    let (one, two) = (
        format!("{}1", long_id('c', limit)),
        format!("{}2", long_id('c', limit)),
    );
    for id in [&one, &two] {
        save(&f, completed(id, "shell", Some("y".into()), false));
    }
    assert_eq!(
        query_error(
            &f,
            history(PageRequest {
                cursor: None,
                limit: 10
            })
        ),
        ErrorCode::LimitExceeded
    );
}

// Given: a subscriber and a tool call whose id the protocol cannot carry
// Then: the stream does not skip the row or shorten its id: it asks clients to read again
#[test]
fn a_tool_call_id_past_the_limit_resyncs_the_stream() {
    let f = current_thread_fixture();
    let mut sub = f.subscribe();
    let id = long_id('z', workbench_protocol::limits::MAX_NAME_BYTES + 1);
    save(&f, completed(&id, "shell", Some("y".into()), false));
    f.rt.block_on(async {
        loop {
            match sub.recv().await.expect("open subscription").event {
                UiEvent::Projection(ProjectionEvent::ResyncRequired { .. }) => break,
                UiEvent::Conversation(ConversationEvent::MessageCommitted { message }) => {
                    panic!("a row with an unrepresentable id was published: {message:?}")
                }
                _ => {}
            }
        }
    });
}

// Wall clock stamps are explicit: the test does not depend on time spent executing it.
#[test]
fn durations_match_in_indexed_live_and_legacy_history_for_all_tool_names() {
    use std::time::{Duration, UNIX_EPOCH};
    let f = current_thread_fixture();
    let mut sub = f.subscribe();
    let at = |kind: ToolEvent, millis| {
        let mut event = Event::new(kind);
        event.meta.wall_clock = UNIX_EPOCH + Duration::from_millis(millis);
        f.store.handle().append_event(None, &event).unwrap();
    };
    for (call, name, input, start, end) in [
        (
            "shell",
            "shell",
            Some(serde_json::json!({"command":"true"})),
            Some(1000),
            2240,
        ),
        (
            "poll",
            "shell",
            Some(serde_json::json!({"action":"poll","job_id":"job-1"})),
            Some(3000),
            3100,
        ),
        ("wait", "wait", None, Some(4000), 4999),
        (
            "js",
            "run_js",
            Some(serde_json::json!({"code":"1"})),
            Some(6000),
            5000,
        ),
        ("missing", "delegate", None, None, 7000),
    ] {
        if let Some(start) = start {
            let mut kind = started(call, name, serde_json::Value::Null);
            if let ToolEvent::ToolStarted { input: value, .. } = &mut kind {
                *value = input;
            }
            at(kind, start);
        }
        at(completed(call, name, None, false), end);
    }
    let indexed = read_all(&f, HistoryDirection::Forward, true);
    assert_eq!(
        indexed
            .iter()
            .map(|m| tool(m).duration_ms)
            .collect::<Vec<_>>(),
        [Some(1240), Some(100), Some(999), None, None]
    );
    let live: Vec<_> = (0..indexed.len())
        .map(|_| next_message(&f, &mut sub))
        .collect();
    assert_eq!(live, indexed);
    // Invalidate via the existing trigger without changing a timestamp: `dirty`
    // advances the first start by 1ns, which correctly changes a floored duration.
    let db = rusqlite::Connection::open(f.dir.path().join("host.db")).unwrap();
    db.execute("UPDATE events SET wall_clock_ns=wall_clock_ns WHERE id=(SELECT MIN(id) FROM events WHERE run_id='run-1')", []).unwrap();
    assert_eq!(read_all(&f, HistoryDirection::Forward, false), indexed);
    rebuild(&f);
    assert_eq!(read_all(&f, HistoryDirection::Backward, true), indexed);
}
