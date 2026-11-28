//! Tool rows, reasoning, notices and errors as clients read and receive them.
use super::*;
use event_bus::{AgentRunPhase, LifecycleEvent, ProviderEvent, ProviderFailureKind, ToolEvent};
use workbench_protocol::views::{ToolCallStatus, ToolCallView};

fn artifact(
    id: u8,
    title: &str,
    caption: Option<&str>,
    media_type: &str,
) -> event_bus::PresentedArtifact {
    event_bus::PresentedArtifact {
        artifact_id: format!("artifact-{id:032x}"),
        title: title.into(),
        caption: caption.map(Into::into),
        media_type: media_type.into(),
        path: "/store/blobs/never-published.png".into(),
        byte_len: 3,
        sha256: format!("{id:064x}"),
    }
}
fn presented(
    title: Option<&str>,
    caption: Option<&str>,
    artifacts: Vec<event_bus::PresentedArtifact>,
) -> ToolEvent {
    ToolEvent::ArtifactsPresented {
        run_id: "run-1".into(),
        presentation: event_bus::ArtifactPresentation {
            presentation_id: "presentation-1".into(),
            title: title.map(Into::into),
            caption: caption.map(Into::into),
            artifacts,
        },
    }
}
fn artifacts_row(
    message: &CommittedMessage,
) -> &workbench_protocol::namespaces::artifacts::ArtifactPresentationView {
    assert_eq!(message.role, MessageRole::Artifacts);
    assert!(message.tool.is_none());
    message.presentation.as_ref().unwrap()
}

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

// Longer than the 2048 bytes from which a stored string is a body token, and within the 1000
// characters `present` accepts.
fn long_japanese(ch: char) -> String {
    ch.to_string().repeat(1000)
}
// Short, but a string that starts with the escape character is stored as a body as well.
const ESCAPED_TITLE: &str = "\u{e000}evorch-body:v1:looks like a token";
fn seeded_artifacts() -> Vec<event_bus::PresentedArtifact> {
    vec![
        artifact(2, "Chart\n1", Some(&long_japanese('い')), "image/png"),
        artifact(1, "\u{1}", Some("line one\nline two\u{7}"), "text/html"),
        artifact(2, "Chart again", None, "image/png"),
    ]
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
    save(f, started("call-p", "present", serde_json::json!({})));
    save(
        f,
        presented(
            Some(ESCAPED_TITLE),
            Some(&long_japanese('あ')),
            seeded_artifacts(),
        ),
    );
    save(
        f,
        completed("call-p", "present", Some("shown".into()), false),
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
            MessageRole::Artifacts,
            MessageRole::Tool,
            MessageRole::Error,
            MessageRole::Error,
        ]
    );
    let card = artifacts_row(&forward[4]);
    assert_eq!(card.title.as_deref(), Some(ESCAPED_TITLE));
    assert_eq!(card.caption.as_deref(), Some(long_japanese('あ').as_str()));
    let shown: Vec<_> = card
        .artifacts
        .iter()
        .map(|a| {
            (
                a.artifact_id.clone(),
                a.title.as_str(),
                a.caption.clone(),
                a.media_type.as_str(),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            (
                format!("artifact-{:032x}", 2),
                "Chart1",
                Some(long_japanese('い')),
                "image/png"
            ),
            (
                format!("artifact-{:032x}", 1),
                "Artifact",
                Some("line one\nline two".to_owned()),
                "text/html"
            ),
            (
                format!("artifact-{:032x}", 2),
                "Chart again",
                None,
                "image/png"
            ),
        ]
    );
    assert_eq!(card.artifacts[0].digest, format!("{:064x}", 2));
    assert_eq!(forward[4].content, inline(""));
    assert!(
        !serde_json::to_string(&forward)
            .unwrap()
            .contains("never-published")
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

fn thread_history(thread: &str, limit: u32) -> Query {
    let Query::Conversation(ConversationQuery::ReadCommittedHistory(mut q)) =
        history(PageRequest {
            cursor: None,
            limit,
        })
    else {
        panic!()
    };
    q.thread = ThreadId::new(thread).unwrap();
    Query::Conversation(ConversationQuery::ReadCommittedHistory(q))
}

// Given: a saved presentation whose text is held as stored bodies, read page by page
// Then: reading resolves exactly the bodies the card shows (not the large text of other
//       rows) and keeps the same position on the indexed and the legacy route
#[test]
fn reading_a_card_resolves_only_its_own_bodies() {
    let f = current_thread_fixture();
    save(
        &f,
        MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: "x".repeat(20 * 1024),
        },
    );
    save(
        &f,
        presented(
            Some(ESCAPED_TITLE),
            Some("short"),
            vec![artifact(1, "A", Some(&long_japanese('う')), "image/png")],
        ),
    );
    save(
        &f,
        MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: "after".into(),
        },
    );
    let meter = read_meter(&f);
    let indexed = read_all(&f, HistoryDirection::Forward, true);
    assert_eq!(
        meter.counts().calls,
        2,
        "the title and the artifact's caption"
    );
    dirty(&f);
    let before = meter.counts().calls;
    let legacy = read_all(&f, HistoryDirection::Backward, false);
    assert_eq!(meter.counts().calls - before, 2);
    let place = |rows: &[CommittedMessage]| -> Vec<_> {
        rows.iter()
            .map(|m| (m.index, m.role, m.presentation.clone()))
            .collect()
    };
    assert_eq!(place(&legacy), place(&indexed));
    let roles: Vec<_> = indexed.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            MessageRole::Assistant,
            MessageRole::Artifacts,
            MessageRole::Assistant
        ]
    );
    assert_eq!(artifacts_row(&indexed[1]).caption.as_deref(), Some("short"));
}

// Given: a presentation that cannot be shown whole: too many artifacts, an artifact id of the
//        wrong shape, and a text body that is gone
// Then: reading the history fails with the matching error and never returns the row without
//       an item, on both routes, and the stream asks clients to read again
#[test]
fn an_unrepresentable_presentation_fails_the_read_and_resyncs_the_stream() {
    let many = || {
        (0..9)
            .map(|i| artifact(i, "A", None, "image/png"))
            .collect()
    };
    let misshapen = || {
        let mut bad = artifact(1, "A", None, "image/png");
        bad.artifact_id = "artifact-1".into();
        vec![bad]
    };
    let gone = || vec![artifact(1, "A", Some(&long_japanese('え')), "image/png")];
    let cases: [(Vec<event_bus::PresentedArtifact>, ErrorCode); 3] = [
        (many(), ErrorCode::LimitExceeded),
        (misshapen(), ErrorCode::StorageUnavailable),
        (gone(), ErrorCode::StorageUnavailable),
    ];
    for (index, (artifacts, code)) in cases.into_iter().enumerate() {
        let f = current_thread_fixture();
        let mut sub = f.subscribe();
        save(&f, presented(None, None, artifacts));
        if index == 2 {
            let db = rusqlite::Connection::open(f.dir.path().join("host.db")).unwrap();
            db.pragma_update(None, "foreign_keys", false).unwrap();
            assert_eq!(db.execute("DELETE FROM content_blobs", []).unwrap(), 1);
        }
        for dirty_first in [false, true] {
            if dirty_first {
                dirty(&f);
            }
            assert_eq!(
                query_error(&f, thread_history("thread-1", 10)),
                code,
                "case {index}"
            );
        }
        f.rt.block_on(async {
            loop {
                match sub.recv().await.expect("open subscription").event {
                    UiEvent::Projection(ProjectionEvent::ResyncRequired { .. }) => break,
                    UiEvent::Conversation(ConversationEvent::MessageCommitted { message }) => {
                        panic!("an unrepresentable presentation was published: {message:?}")
                    }
                    _ => {}
                }
            }
        });
    }
}

// Given: a presentation that is not stored yet, then a stored message
// Then: only the stored rows reach a subscriber, and the card, once stored, arrives at the
//       position it was saved at
#[test]
fn a_card_is_published_only_after_it_is_stored() {
    let f = current_thread_fixture();
    let mut sub = f.subscribe();
    f.runtime.event_bus().emit(Event::new(presented(
        Some("unsaved"),
        None,
        vec![artifact(1, "A", None, "image/png")],
    )));
    let says = |text: &str| {
        let event = Event::new(MessageEvent::MessageCompleted {
            run_id: "run-1".into(),
            text: text.into(),
        });
        f.store.handle().append_event(None, &event).unwrap();
        f.runtime.event_bus().emit(event);
    };
    says("before");
    let first = next_message(&f, &mut sub);
    assert_eq!(
        (first.role, first.index),
        (MessageRole::Assistant, Sequence(0))
    );
    let card = Event::new(presented(
        Some("saved"),
        None,
        vec![artifact(1, "A", None, "image/png")],
    ));
    f.store.handle().append_event(None, &card).unwrap();
    f.runtime.event_bus().emit(card);
    says("after");
    let second = next_message(&f, &mut sub);
    assert_eq!(
        (second.role, second.index),
        (MessageRole::Artifacts, Sequence(1))
    );
    assert_eq!(artifacts_row(&second).title.as_deref(), Some("saved"));
    let third = next_message(&f, &mut sub);
    assert_eq!(
        (third.role, third.index),
        (MessageRole::Assistant, Sequence(2))
    );
    let stored = read_all(&f, HistoryDirection::Forward, true);
    assert_eq!(stored, [first, second, third]);
}

// Given: a conversation that belongs to no project
// Then: its card is a row like any other
#[test]
fn a_card_of_a_conversation_without_a_project_is_readable() {
    let f = current_thread_fixture();
    f.store
        .handle()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "loose".into(),
            project_id: None,
            project_root: None,
            title: Some("Loose".into()),
            root_run_id: Some("run-loose".into()),
            ..Default::default()
        }])
        .unwrap();
    save(
        &f,
        ToolEvent::ArtifactsPresented {
            run_id: "run-loose".into(),
            presentation: event_bus::ArtifactPresentation {
                presentation_id: "presentation-1".into(),
                title: None,
                caption: None,
                artifacts: vec![artifact(1, "A", None, "image/png")],
            },
        },
    );
    let QueryReply::Conversation(ConversationReply::CommittedHistory(page)) =
        f.query(thread_history("loose", 10))
    else {
        panic!()
    };
    assert_eq!(page.items.len(), 1);
    assert_eq!(artifacts_row(&page.items[0]).artifacts.len(), 1);
    assert!(page.items[0].validate().is_ok());
}

// Given: presentations in a history that exists before the host starts
// Then: a host opened on it restores each card at its saved position
#[test]
fn cards_are_restored_when_the_core_is_reopened() {
    let f = Fixture::with_history(|store, _| {
        let save = |kind: ToolEvent| {
            store
                .handle()
                .append_event(None, &Event::new(kind))
                .unwrap();
        };
        let says = |text: &str| {
            store
                .handle()
                .append_event(
                    None,
                    &Event::new(MessageEvent::MessageCompleted {
                        run_id: "run-1".into(),
                        text: text.into(),
                    }),
                )
                .unwrap();
        };
        says("one");
        save(presented(
            None,
            Some(&long_japanese('お')),
            vec![artifact(1, "A", None, "text/html")],
        ));
        says("two");
        save(presented(None, None, vec![]));
    });
    let rows = read_all(&f, HistoryDirection::Forward, true);
    let shape: Vec<_> = rows.iter().map(|m| (m.index.0, m.role)).collect();
    assert_eq!(
        shape,
        [
            (0, MessageRole::Assistant),
            (1, MessageRole::Artifacts),
            (2, MessageRole::Assistant),
            (3, MessageRole::Artifacts),
        ]
    );
    assert_eq!(
        artifacts_row(&rows[1]).caption.as_deref(),
        Some(long_japanese('お').as_str())
    );
    assert!(artifacts_row(&rows[3]).artifacts.is_empty());
    dirty(&f);
    assert_eq!(read_all(&f, HistoryDirection::Forward, false), rows);
}
