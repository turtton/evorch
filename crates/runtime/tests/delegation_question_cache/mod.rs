//! Ordinary delegation questions append after tool batches and preserve the
//! independent wire prefix AND the mock cache computed from actual request input.
use super::*;

fn call(id: &str, name: &str, input: Value) -> ScriptedResponse {
    ScriptedResponse::tool_call(id, MODEL, 0, id, name, [input.to_string()])
}
fn history(body: &Value) -> String {
    body.get("messages")
        .or_else(|| body.get("input"))
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn question_tool_idle_and_terminal_wakes_preserve_wire_prefix_through_answer_and_followup() {
    for wire in [Wire::ChatCompletions, Wire::Responses, Wire::Codex] {
        for state in ["tool", "idle", "terminal"] {
            let read = Arc::new(GatedRead {
                started: Notify::new(),
                release: Notify::new(),
                child_started: Notify::new(),
            });
            let mut h = harness_with_wire(
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
                db_path: h._directory.path().join("question-wake.db"),
                ..Default::default()
            };
            let storage = storage::Storage::open(config.clone()).unwrap();
            h.runtime = h
                .runtime
                .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
            let goal = "Coordinate ordinary delegation question cache delivery";
            let parent = h.runtime.delegate_background(
                Role::Orchestrator,
                goal.into(),
                RunConfig {
                    interactive: state != "terminal",
                    keep_alive: state != "terminal",
                    ..Default::default()
                },
            );
            read.started.notified().await;
            h.mock.enqueue(vec![read_response(1)]);
            let child = h
                .runtime
                .delegate_background_as_child(
                    parent,
                    Role::Worker,
                    "Child stays inside its tool",
                    RunConfig::default(),
                )
                .unwrap();
            read.child_started.notified().await;
            let mut events = Vec::new();
            if state != "tool" {
                h.mock.enqueue(vec![text_response("parent settled")]);
                read.release.notify_one();
                let phase = if state == "terminal" {
                    AgentRunPhase::Done
                } else {
                    AgentRunPhase::Waiting
                };
                events.extend(through_phase(&mut h.receiver, parent, phase).await);
                if state == "terminal" {
                    h.runtime.wait(parent).await.unwrap();
                }
            }
            // Freeze the live model inside a tool (or at an observed turn end), so
            // the notification cannot race the scripts intended for its parent.
            let question = h
                .runtime
                .request_user_question(child, "Choose the bounded task scope".into(), vec![], true)
                .unwrap();
            h.mock.enqueue(vec![
                call(
                    "list-child-questions",
                    "subagent_questions",
                    json!({"run_id":child.to_string()}),
                ),
                call(
                    "answer-child-question",
                    "answer_subagent_question",
                    json!({"question_id":question.id,"answer":"Use the bounded scope"}),
                ),
                text_response("question handled"),
            ]);
            if state == "tool" {
                read.release.notify_one();
            }
            if state == "terminal" {
                loop {
                    let event = h.receiver.recv().await.unwrap();
                    let wake = matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::RootWakeRequested { run_id }) if run_id == &parent.to_string());
                    events.push(event);
                    if wake {
                        break;
                    }
                }
                assert!(
                    h.runtime
                        .prepare_held_wake(parent, &RunConfig::default())
                        .unwrap()
                );
                h.runtime.wake_chat(parent, RunConfig::default()).unwrap();
            }
            // In the idle case the scripts above were enqueued synchronously,
            // before the idle agent can issue its next model request.
            // A restored completed turn first publishes Waiting before consuming
            // its mailbox. Await the durable answer event, not that initial phase.
            loop {
                let event = h.receiver.recv().await.unwrap();
                let answered = matches!(&event.kind, EventKind::Tool(event_bus::ToolEvent::UserQuestionUpdated { question: updated }) if updated.id == question.id && updated.answer.is_some());
                if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    run_id, to, ..
                }) = &event.kind
                    && run_id == &parent.to_string()
                {
                    assert!(
                        !matches!(
                            to,
                            AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                        ),
                        "unexpected terminal before answer: {event:?}"
                    );
                }
                events.push(event);
                if answered {
                    break;
                }
            }
            events.extend(through_phase(&mut h.receiver, parent, AgentRunPhase::Waiting).await);
            assert_eq!(
                h.runtime.user_answers(child).unwrap()[0].answer.as_deref(),
                Some("Use the bounded scope"),
                "state={state} wire={} remaining_scripts={} requests={}",
                wire.path(),
                h.mock.remaining_scripts(),
                h.mock.recorded_requests().len()
            );
            h.mock
                .enqueue(vec![text_response("ordinary followup complete")]);
            h.runtime
                .send_message(parent, "A normal later user turn".into())
                .unwrap();
            events.extend(through_phase(&mut h.receiver, parent, AgentRunPhase::Waiting).await);
            let requests: Vec<_> = h
                .mock
                .recorded_requests()
                .into_iter()
                .filter(|r| r.path == wire.path())
                .map(|r| r.body)
                .filter(|b| b.to_string().contains(goal))
                .collect();
            assert_eq!(
                requests.len(),
                if state == "tool" { 5 } else { 6 },
                "{state}"
            );
            assert_eq!(h.mock.remaining_scripts(), 0);
            let last = history(requests.last().unwrap());
            assert_eq!(
                last.matches("[subagent-question").count(),
                1,
                "{state}: {last}"
            );
            assert!(last.contains("A normal later user turn"));
            if state == "tool" {
                let input = requests[1]
                    .get("messages")
                    .or_else(|| requests[1].get("input"))
                    .unwrap()
                    .as_array()
                    .unwrap();
                let notice = input
                    .iter()
                    .position(|m| m.to_string().contains("[subagent-question"))
                    .unwrap();
                let result = input
                    .iter()
                    .position(|m| {
                        m.to_string().contains("call-0")
                            && (m["role"] == "tool" || m["type"] == "function_call_output")
                    })
                    .unwrap();
                assert!(
                    result < notice,
                    "notice must follow ToolResult, never split the tool batch"
                );
            }
            for pair in requests.windows(2) {
                assert_append_only(wire.protocol(), &pair[0], &pair[1])
                    .unwrap_or_else(|e| panic!("{state} question wake changed sent prefix: {e}"));
            }
            let usage: Vec<_> = events
                .iter()
                .filter_map(|e| match &e.kind {
                    EventKind::Provider(ProviderEvent::RequestCompleted {
                        run_id: Some(id),
                        input_tokens,
                        cache_read_tokens,
                        ..
                    }) if id == &parent.to_string() => Some((*input_tokens, *cache_read_tokens)),
                    _ => None,
                })
                .collect();
            assert_eq!(usage.len(), requests.len());
            for pair in usage.windows(2) {
                assert!(
                    pair[1].1 >= pair[0].0,
                    "{state}: input-derived cache was lost: {pair:?}"
                );
            }
            assert!(!events.iter().any(|e| matches!(
                e.kind,
                EventKind::Compaction(CompactionEvent::Compacted { .. })
            )));
            h.runtime.cancel_subtree(parent).unwrap();
            h.runtime.wait(child).await.unwrap();
            h.runtime.wait(parent).await.unwrap();
        }
    }
}
