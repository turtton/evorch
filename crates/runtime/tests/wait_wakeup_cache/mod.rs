//! A wait that ends early on an answer or on host input is an ordinary
//! append-only provider turn: the next request keeps the sent prefix and the
//! answer or input is appended once after the tool result.
use super::*;
use event_bus::ToolEvent;

fn call(id: &str, name: &str, input: Value) -> ScriptedResponse {
    ScriptedResponse::tool_call(id, MODEL, 0, id, name, [input.to_string()])
}

fn waiting_transitions(events: &[Event], run: RunId) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(&event.kind,
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Waiting, .. })
                    if run_id == &run.to_string())
        })
        .count()
}

async fn until_waiting(harness: &mut Harness, events: &mut Vec<Event>, run: RunId, count: usize) {
    while waiting_transitions(events, run) < count {
        events.push(harness.receiver.recv().await.unwrap());
    }
}

fn question_id(harness: &Harness, run: RunId, title: &str) -> String {
    harness
        .runtime
        .user_answers(run)
        .unwrap()
        .into_iter()
        .find(|question| question.title == title)
        .unwrap_or_else(|| panic!("missing question {title}"))
        .id
}

/// The conversation part of a request; tool descriptions are excluded.
fn history(request: &Value) -> String {
    request
        .get("messages")
        .or_else(|| request.get("input"))
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn answer_and_host_input_wakeups_of_waits_preserve_the_wire_prefix() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        let read = Arc::new(GatedRead {
            started: Notify::new(),
            release: Notify::new(),
            child_started: Notify::new(),
        });
        let mut harness = harness_with_wire(
            wire,
            vec![read_response(0)],
            1_000_000,
            read.clone(),
            Vec::new(),
            None,
            0.9,
            |_| {},
        );
        let config = storage::StorageConfig {
            db_path: harness._directory.path().join("wait-wakeups.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        harness.runtime = harness
            .runtime
            .with_sequential_run_ids()
            .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        let goal = "Coordinate the waiting wake-ups";
        let parent = harness.runtime.delegate_background(
            Role::Orchestrator,
            goal.into(),
            RunConfig::default(),
        );
        read.started.notified().await;
        // The child stays in a tool for good, so only the parent talks to the model.
        harness.mock.enqueue(vec![read_response(1)]);
        let child = harness
            .runtime
            .delegate_background_as_child(parent, Role::Worker, "Child work", RunConfig::default())
            .unwrap();
        read.child_started.notified().await;
        assert_eq!(
            harness.runtime.send_agent_message(
                parent,
                child,
                event_bus::AgentMessageKind::Send,
                "question for the child",
                None,
            ),
            Ok("msg-1".to_string())
        );
        harness.mock.enqueue(vec![
            call(
                "ask-1",
                "ask_user",
                json!({"title":"Which scope?","blocking":false}),
            ),
            call(
                "reply-wait",
                "wait_reply",
                json!({"message_id":"msg-1","timeout_ms":600_000}),
            ),
            call(
                "ask-2",
                "ask_user",
                json!({"title":"Which risk?","blocking":false}),
            ),
            call("child-wait", "wait", json!({"run_ids":[child.to_string()]})),
            call(
                "reply-wait-again",
                "wait_reply",
                json!({"message_id":"msg-1","timeout_ms":600_000}),
            ),
            text_response("applied everything"),
        ]);
        read.release.notify_one();
        let mut events = Vec::new();

        until_waiting(&mut harness, &mut events, parent, 1).await;
        let first = question_id(&harness, parent, "Which scope?");
        harness
            .runtime
            .answer_user_question(&first, "Use A")
            .unwrap();
        until_waiting(&mut harness, &mut events, parent, 2).await;
        let second = question_id(&harness, parent, "Which risk?");
        harness
            .runtime
            .answer_user_question(&second, "Use B")
            .unwrap();
        until_waiting(&mut harness, &mut events, parent, 3).await;
        harness
            .runtime
            .send_message(parent, "host says hi".into())
            .unwrap();
        events.extend(through_phase(&mut harness.receiver, parent, AgentRunPhase::Done).await);

        let requests = harness
            .mock
            .recorded_requests()
            .into_iter()
            .filter(|request| request.path == wire.path())
            .map(|request| request.body)
            .filter(|body| body.to_string().contains(goal))
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 7);
        assert_eq!(harness.mock.remaining_scripts(), 0);
        // The wake-up results append to the history; each answer or input shows
        // up once, right after the tool result that carried the wake-up.
        let after_first = history(&requests[3]);
        assert!(after_first.contains("answer_ready"), "{after_first}");
        assert_eq!(after_first.matches("Use A").count(), 1);
        assert_eq!(after_first.matches("Use B").count(), 0);
        let after_second = history(&requests[5]);
        assert!(after_second.contains("answer_ready"));
        assert_eq!(after_second.matches("Use A").count(), 1);
        assert_eq!(after_second.matches("Use B").count(), 1);
        let last = history(&requests[6]);
        assert!(last.contains("user_input_ready"));
        assert_eq!(last.matches("host says hi").count(), 1);
        assert_eq!(last.matches("Use A").count(), 1);
        assert_eq!(last.matches("Use B").count(), 1);
        for index in 0..requests.len() {
            // A wake-up never relaxes the stable instructions or tool schemas.
            assert_eq!(requests[index]["tools"], requests[0]["tools"], "{index}");
        }
        // Independent wire prefix comparison AND input-derived cache usage. The
        // child's own request is excluded: only the parent's run is traced.
        for pair in requests.windows(2) {
            assert_append_only(wire.protocol(), &pair[0], &pair[1])
                .unwrap_or_else(|error| panic!("a wake-up destroyed cached input: {error}"));
        }
        let usage = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Provider(ProviderEvent::RequestCompleted {
                    run_id: Some(run),
                    input_tokens,
                    cache_read_tokens,
                    ..
                }) if run == &parent.to_string() => Some((*input_tokens, *cache_read_tokens)),
                EventKind::Diagnostic(diagnostic) if diagnostic.code == "CacheRegression" => {
                    panic!("{diagnostic:?}")
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(usage.len(), requests.len());
        assert_eq!(usage[0].1, 0, "the first request is cold");
        for pair in usage.windows(2) {
            assert!(pair[0].0 > 0);
            assert!(
                pair[1].1 >= pair[0].0,
                "cached {} < previously processed {}",
                pair[1].1,
                pair[0].0
            );
        }

        harness.runtime.cancel(child).unwrap();
        harness.runtime.wait(child).await.unwrap();
        assert!(events.iter().all(|event| !matches!(
            &event.kind,
            EventKind::Tool(ToolEvent::ToolCompleted { is_error: true, run_id: Some(run), .. })
                if run == &parent.to_string()
        )));
    }
}
