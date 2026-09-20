mod support;

mod chat_flow_contract {
    use super::support;
    use std::sync::Arc;

    use event_bus::{
        AgentRunPhase, Event, EventBus, EventKind, EventReceiver, LifecycleEvent, MessageEvent,
    };
    use providers::{ContentBlock, FinishReason, Message, Role, ToolResultContent};
    use runtime::{AgentRuntime, RunConfig, RunId, RunStore, RuntimeError};
    use sandbox::DirectSandbox;
    use serde_json::json;
    use storage::{Database, Storage, StorageConfig};
    use support::{ScriptedModel, text_response, tool_response};
    use tokio::time::{Duration, timeout};
    use tools::ToolExecutor;

    fn runtime_with(model: Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>) {
        let bus = Arc::new(EventBus::new(128));
        let executor = Arc::new(ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
        ));
        (AgentRuntime::new(Arc::clone(&bus), executor, model), bus)
    }

    fn chat(runtime: &AgentRuntime, prompt: &str, config: RunConfig) -> RunId {
        runtime
            .delegate_chat(
                "contract-thread",
                agents::Role::Worker,
                prompt.into(),
                config,
            )
            .expect("worker chat starts or restores")
    }

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    async fn until_phase(
        receiver: &mut EventReceiver,
        run: RunId,
        phase: AgentRunPhase,
    ) -> Vec<Event> {
        timeout(Duration::from_secs(5), async {
            let mut events = Vec::new();
            loop {
                let event = receiver.recv().await.expect("event receiver stays open");
                let reached = matches!(
                    &event.kind,
                    EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
                        if run_id == &run.to_string() && *to == phase
                );
                events.push(event);
                if reached {
                    return events;
                }
            }
        })
        .await
        .expect("requested chat phase arrives")
    }

    fn assert_answer(events: &[Event], run: RunId, answer: &str) {
        assert!(
            events.iter().any(|event| matches!(
                &event.kind,
                EventKind::Message(MessageEvent::MessageDelta { run_id: Some(id), delta })
                    if id == &run.to_string() && delta == answer
            )),
            "answer {answer:?} must be delivered for {run}: {events:?}"
        );
    }

    #[tokio::test]
    async fn worker_chat_runs_tool_call_then_produces_answer() {
        // Given: a real read tool and a model that requests it before answering.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("evidence.txt");
        std::fs::write(&path, "tool evidence").unwrap();
        let model = Arc::new(ScriptedModel::new([
            Ok(tool_response(
                "read-evidence",
                "read",
                json!({ "path": path }),
            )),
            Ok(text_response("evidence received", FinishReason::Stop)),
        ]));
        let (runtime, bus) = runtime_with(model.clone());
        let mut receiver = bus.subscribe();

        // When: the worker chat executes its tool round and final answer.
        let run = chat(&runtime, "read evidence", RunConfig::default());
        let events = until_phase(&mut receiver, run, AgentRunPhase::Done).await;

        // Then: actual file contents reach the next model request, then the user gets an answer.
        assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
        assert_eq!(
            runtime.inspect_agent(run).unwrap().role_name,
            agents::Role::Worker.name()
        );
        let observed = model.observed().await;
        assert_eq!(observed.len(), 2);
        assert!(
            observed[1].iter().any(
                |message| message.content.contains(&ContentBlock::ToolResult {
                    tool_call_id: "read-evidence".into(),
                    content: vec![ToolResultContent::Text {
                        text: "tool evidence".into()
                    }],
                    is_error: false,
                })
            ),
            "successful real tool result missing: {:?}",
            observed[1]
        );
        assert_answer(&events, run, "evidence received");
        assert_eq!(
            runtime.run_result(run),
            Ok(Some("evidence received".into()))
        );
    }

    #[tokio::test]
    async fn follow_up_while_waiting_lands_in_same_run() {
        // Given: an interactive worker has answered once and is waiting for input.
        let first_answer = text_response("first answer", FinishReason::Stop);
        let model = Arc::new(ScriptedModel::new([
            Ok(first_answer.clone()),
            Ok(text_response("second answer", FinishReason::Stop)),
        ]));
        let (runtime, bus) = runtime_with(model.clone());
        let mut receiver = bus.subscribe();
        let run = chat(
            &runtime,
            "first question",
            RunConfig {
                interactive: true,
                ..RunConfig::default()
            },
        );
        let first = until_phase(&mut receiver, run, AgentRunPhase::Waiting).await;
        assert_answer(&first, run, "first answer");
        assert_eq!(
            runtime.inspect_agent(run).unwrap().phase,
            AgentRunPhase::Waiting
        );

        // When: a follow-up is delivered through the public interactive inbox.
        runtime.send_message(run, "second question".into()).unwrap();
        let second = until_phase(&mut receiver, run, AgentRunPhase::Done).await;

        // Then: this same run resumes, preserving both sides of its prior conversation.
        assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
        assert_eq!(runtime.list_agents().len(), 1);
        assert!(second.iter().any(|event| matches!(
            &event.kind,
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id, from: AgentRunPhase::Waiting, to: AgentRunPhase::Running, ..
            }) if run_id == &run.to_string()
        )));
        assert_eq!(
            model.observed().await,
            vec![
                vec![user("first question")],
                vec![
                    user("first question"),
                    first_answer.message,
                    user("second question")
                ],
            ]
        );
        assert_answer(&second, run, "second answer");
        assert_eq!(runtime.run_result(run), Ok(Some("second answer".into())));
    }

    #[tokio::test]
    async fn post_error_continue_reuses_same_run_preserving_context() {
        // Given: a persisted interactive chat with one successful turn before a model error.
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("chat.sqlite3"),
            ..StorageConfig::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let database = Database::open(&config).unwrap();
        let first_answer = text_response("remembered answer", FinishReason::Stop);
        let model = Arc::new(ScriptedModel::new([
            Ok(first_answer.clone()),
            Err(RuntimeError::Model {
                reason: "scripted provider failure".into(),
            }),
            Ok(text_response("recovered answer", FinishReason::Stop)),
        ]));
        let (runtime, bus) = runtime_with(model.clone());
        let runtime = runtime.with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        let mut receiver = bus.subscribe();
        let failed = chat(
            &runtime,
            "remember this",
            RunConfig {
                interactive: true,
                ..RunConfig::default()
            },
        );
        let first = until_phase(&mut receiver, failed, AgentRunPhase::Waiting).await;
        assert_answer(&first, failed, "remembered answer");
        runtime
            .send_message(failed, "failing question".into())
            .unwrap();
        let error = until_phase(&mut receiver, failed, AgentRunPhase::Error).await;
        assert_eq!(runtime.wait(failed).await, Ok(AgentRunPhase::Error));
        assert!(error.iter().any(|event| matches!(
            &event.kind,
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id, to: AgentRunPhase::Error, reason: Some(reason), ..
            }) if run_id == &failed.to_string() && reason.contains("scripted provider failure")
        )));
        let saved = database.run_context(&failed.to_string()).unwrap().unwrap();
        assert!(
            saved.restorable,
            "model errors must retain a restorable snapshot"
        );
        let prior = vec![
            user("remember this"),
            first_answer.message,
            user("failing question"),
        ];
        assert_eq!(
            serde_json::from_str::<Vec<Message>>(&saved.messages_json).unwrap(),
            prior
        );

        // When: the same logical thread continues through delegate_chat after the error.
        let continued = chat(&runtime, "continue please", RunConfig::default());
        let events = until_phase(&mut receiver, continued, AgentRunPhase::Done).await;

        // Then: plain chat allocates a NEW RunId, restoring the terminal record's history.
        assert_ne!(continued, failed);
        assert_eq!(runtime.wait(continued).await, Ok(AgentRunPhase::Done));
        let observed = model.observed().await;
        assert_eq!(observed.len(), 3);
        let mut restored = prior;
        restored.push(user("continue please"));
        assert_eq!(observed[2], restored);
        assert_answer(&events, continued, "recovered answer");
        assert_eq!(
            runtime.run_result(continued),
            Ok(Some("recovered answer".into()))
        );
    }
}
