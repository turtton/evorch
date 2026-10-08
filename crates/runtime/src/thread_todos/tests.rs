use super::*;
use crate::{AgentInvocationContext, AgentModel, AgentRunPhase, RunConfig, RuntimeError};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolResultContent, Usage};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::Notify;

#[derive(Clone)]
enum Step {
    Tools(Vec<(&'static str, Value)>),
    Stop,
    Hold(Arc<Notify>, Arc<Notify>),
    GatedTools(Arc<Notify>, Arc<Notify>, Vec<(&'static str, Value)>),
}
type Request = (String, Vec<Message>, Vec<ToolSpec>);
struct Model {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<Request>>,
}
#[async_trait::async_trait]
impl AgentModel for Model {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
    async fn complete(
        &self,
        ctx: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        specs: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.requests
            .lock()
            .unwrap()
            .push((ctx.run_id.clone(), messages.to_vec(), specs.to_vec()));
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider continuation");
        let calls = match step {
            Step::Tools(calls) => calls,
            Step::Stop => vec![],
            Step::GatedTools(started, release, calls) => {
                started.notify_one();
                release.notified().await;
                calls
            }
            Step::Hold(started, release) => {
                started.notify_one();
                release.notified().await;
                vec![]
            }
        };
        let finished = calls.is_empty();
        let content = if finished {
            vec![ContentBlock::Text {
                text: "Result".into(),
            }]
        } else {
            calls
                .into_iter()
                .enumerate()
                .map(|(index, (name, input))| ContentBlock::ToolUse {
                    id: format!("call-{}-{index}", self.requests.lock().unwrap().len()),
                    name: name.into(),
                    input,
                })
                .collect()
        };
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content,
            },
            finish_reason: if finished {
                FinishReason::Stop
            } else {
                FinishReason::ToolUse
            },
            usage: Usage::default(),
        })
    }
}
fn harness(steps: Vec<Step>) -> (AgentRuntime, Arc<Model>, event_bus::EventReceiver) {
    let model = Arc::new(Model {
        steps: Mutex::new(steps.into()),
        requests: Mutex::new(vec![]),
    });
    let bus = Arc::new(event_bus::EventBus::new(4096));
    let events = bus.subscribe();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(::tools::ToolExecutor::new(bus)),
        model.clone(),
    );
    (runtime, model, events)
}
fn config() -> RunConfig {
    RunConfig {
        conversation: true,
        ..Default::default()
    }
}
fn start(runtime: &AgentRuntime, role: Role, config: RunConfig, bound: bool) -> RunId {
    let run = runtime.reserve_run_id();
    if bound {
        runtime.bind_thread_root("thread", run).unwrap();
    }
    runtime.spawn_reserved(run, None, role, "Complete the requested work", config)
}
fn item(content: &str, status: ThreadTodoStatus) -> ThreadTodoItem {
    ThreadTodoItem {
        content: content.into(),
        status,
    }
}
fn write(items: Vec<ThreadTodoItem>) -> Step {
    Step::Tools(vec![("todo_write", json!({"items":items}))])
}
fn results(requests: &[Request]) -> Vec<(bool, String)> {
    requests
        .last()
        .unwrap()
        .1
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                is_error, content, ..
            } => Some((
                *is_error,
                content
                    .iter()
                    .map(|part| match part {
                        ToolResultContent::Text { text } => text.as_str(),
                    })
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}
fn contains_snapshot(messages: &[Message], snapshot: &ThreadTodoSnapshot) -> bool {
    let json = json!(snapshot).to_string();
    messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text.ends_with(&json)))
    })
}
fn restored(items: Vec<ThreadTodoItem>) -> ThreadTodoSnapshot {
    ThreadTodoSnapshot {
        list_id: "list-1".into(),
        thread_id: "thread".into(),
        revision: 3,
        items,
    }
}
async fn phase(events: &mut event_bus::EventReceiver, run: RunId, wanted: AgentRunPhase) {
    loop {
        if let event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
            run_id,
            to,
            reason,
            ..
        }) = events.recv().await.unwrap().kind
            && run_id == run.to_string()
        {
            if to == wanted {
                return;
            }
            assert!(
                !matches!(
                    to,
                    AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                ),
                "unexpected terminal {to:?}: {reason:?}"
            );
        }
    }
}

#[tokio::test]
async fn replacement_parallel_items_and_clear_do_not_control_lifecycle_or_goal() {
    for role in [Role::Worker, Role::Orchestrator] {
        let pending = vec![
            item("Inspect", ThreadTodoStatus::InProgress),
            item("Verify", ThreadTodoStatus::InProgress),
        ];
        let (runtime, model, mut events) = harness(vec![
            write(pending),
            write(vec![item("Verify", ThreadTodoStatus::Completed)]),
            write(vec![]),
            Step::Stop,
        ]);
        let run = start(&runtime, role, config(), true);
        assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
        let final_state = runtime.thread_todo("thread").unwrap();
        assert_eq!(final_state.revision, 3);
        assert!(final_state.items.is_empty());
        assert!(runtime.thread_goal("thread").is_none());
        let mut snapshots = vec![];
        for event in events.drain_pending_snapshot() {
            if let event_bus::EventKind::Orchestrator(OrchestratorEvent::ThreadTodoUpdated {
                snapshot,
            }) = event.kind
            {
                snapshots.push(snapshot);
            }
        }
        assert_eq!(snapshots.len(), 3);
        assert_eq!(snapshots[0].items.len(), 2);
        assert!(snapshots.iter().all(|s| s.list_id == final_state.list_id));
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(
            results(&requests)
                .iter()
                .all(|(error, text)| !error && text.len() < 256)
        );
        for pair in requests.windows(2) {
            assert_eq!(pair[0].2, pair[1].2);
            assert_eq!(pair[0].1, pair[1].1[..pair[0].1.len()]);
        }
    }
    // Unfinished procedures alone never cause additional model turns.
    let (runtime, model, _) = harness(vec![
        write(vec![item("Later", ThreadTodoStatus::Pending)]),
        Step::Stop,
    ]);
    let run = start(&runtime, Role::Worker, config(), true);
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(model.requests.lock().unwrap().len(), 2);
    assert_eq!(
        runtime.thread_todo("thread").unwrap().items[0].status,
        ThreadTodoStatus::Pending
    );
}

#[tokio::test]
async fn invalid_arguments_leave_the_last_durable_list_unchanged() {
    let invalid = vec![
        json!({"items":[{"content":" ","status":"pending"}]}),
        json!({"items":[{"content":"x".repeat(1025),"status":"pending"}]}),
        json!({"items":vec![json!({"content":"x","status":"pending"});33]}),
        json!({"items":[{"content":"x","status":"unknown"}]}),
        json!({"items":[{"content":"x","status":"pending","id":"untrusted"}]}),
        json!({"items":[],"thread_id":"other"}),
    ];
    let mut steps = vec![write(vec![item("Keep", ThreadTodoStatus::InProgress)])];
    steps.extend(
        invalid
            .into_iter()
            .map(|input| Step::Tools(vec![("todo_write", input)])),
    );
    steps.push(Step::Stop);
    let (runtime, model, _) = harness(steps);
    let run = start(&runtime, Role::Worker, config(), true);
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let snapshot = runtime.thread_todo("thread").unwrap();
    assert_eq!(snapshot.revision, 1);
    assert_eq!(
        snapshot.items,
        vec![item("Keep", ThreadTodoStatus::InProgress)]
    );
    let requests = model.requests.lock().unwrap();
    let results = results(&requests);
    assert!(!results[0].0);
    assert!(results[1..].iter().all(|result| result.0));
}

#[tokio::test]
async fn tool_exposure_and_dispatch_require_current_trusted_conversation_root() {
    let cases = vec![
        (Role::Worker, config(), false),
        (Role::Worker, RunConfig::default(), true),
        (Role::Explorer, config(), true),
        (Role::Reviewer, config(), true),
        (
            Role::Orchestrator,
            RunConfig {
                purpose: RunPurpose::LessonExtract {
                    source_run_id: RunId::new(1),
                },
                ..config()
            },
            true,
        ),
    ];
    for (role, config, bound) in cases {
        let (runtime, model, _) = harness(vec![
            write(vec![item("Forbidden", ThreadTodoStatus::Pending)]),
            Step::Stop,
        ]);
        let run = start(&runtime, role, config, bound);
        assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
        assert!(runtime.thread_todo("thread").is_none());
        let requests = model.requests.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|r| r.2.iter().all(|s| s.name != "todo_write"))
        );
        assert!(results(&requests)[0].0);
    }
    // Changing the host binding after the fixed schema was sent revokes writes.
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (runtime, model, mut events) = harness(vec![
        Step::Hold(entered.clone(), release.clone()),
        write(vec![item("Stale", ThreadTodoStatus::Pending)]),
        Step::Stop,
    ]);
    let run = start(
        &runtime,
        Role::Worker,
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..config()
        },
        true,
    );
    entered.notified().await;
    runtime
        .bind_thread_root("thread", runtime.reserve_run_id())
        .unwrap();
    release.notify_one();
    phase(&mut events, run, AgentRunPhase::Waiting).await;
    runtime.send_message(run, "Continue".into()).unwrap();
    phase(&mut events, run, AgentRunPhase::Waiting).await;
    runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Stopped);
    assert!(runtime.thread_todo("thread").is_none());
    let requests = model.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|r| r.2.iter().any(|s| s.name == "todo_write"))
    );
    assert!(results(&requests)[0].0);
}

#[tokio::test]
async fn restore_is_passive_and_empty_revision_prevents_resurrection() {
    let (runtime, model, mut events) = harness(vec![Step::Stop]);
    let earlier = restored(vec![item("Old", ThreadTodoStatus::Pending)]);
    let mut cleared = earlier.clone();
    cleared.items.clear();
    cleared.revision += 1;
    runtime.restore_thread_todo(cleared.clone()).unwrap();
    runtime.restore_thread_todo(earlier).unwrap();
    assert_eq!(runtime.thread_todo("thread"), Some(cleared.clone()));
    assert!(runtime.list_agents().is_empty());
    assert!(runtime.goal_lock().roots.is_empty());
    assert!(events.drain_pending_snapshot().is_empty());
    let run = start(&runtime, Role::Worker, config(), true);
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let requests = model.requests.lock().unwrap();
    assert!(contains_snapshot(&requests[0].1, &cleared));
    assert!(runtime.thread_goal("thread").is_none());
}

#[tokio::test]
async fn todo_only_escalation_moves_list_and_conversation_without_granting_source_followup_access()
{
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (runtime, model, mut events) = harness(vec![
        write(vec![item(
            "Delegate research",
            ThreadTodoStatus::InProgress,
        )]),
        Step::Tools(vec![(
            "escalate",
            json!({"original_request":"Research","escalation_reason":"Parallel investigation"}),
        )]),
        Step::Hold(entered.clone(), release.clone()),
        Step::Stop,
    ]);
    let source = start(
        &runtime,
        Role::Worker,
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..config()
        },
        true,
    );
    entered.notified().await;
    let child = runtime
        .list_agents()
        .into_iter()
        .find(|r| r.run_id != source)
        .unwrap()
        .run_id;
    let thread = event_bus::escalation_thread_id(&child.to_string());
    let moved = runtime.thread_todo(&thread).unwrap();
    assert_eq!(moved.revision, 2);
    assert!(runtime.thread_todo("thread").is_none());
    assert!(runtime.goal_thread(source).is_none());
    assert_eq!(runtime.goal_thread(child), Some(thread));
    assert!(runtime.write_thread_todo(source, vec![]).is_err());
    {
        let requests = model.requests.lock().unwrap();
        let child_request = requests.iter().find(|r| r.0 == child.to_string()).unwrap();
        assert!(child_request.2.iter().any(|spec| spec.name == "todo_write"));
        assert!(contains_snapshot(&child_request.1, &moved));
    }
    // A fresh source incarnation starts a distinct list even when its RunId is reused.
    runtime.bind_thread_root("thread", source).unwrap();
    let source_list = runtime
        .write_thread_todo(
            source,
            vec![item("Separate followup", ThreadTodoStatus::Pending)],
        )
        .unwrap();
    assert_ne!(source_list.list_id, moved.list_id);
    let mut obsolete = moved.clone();
    obsolete.thread_id = "thread".into();
    obsolete.revision = 1;
    runtime.restore_thread_todo(obsolete).unwrap();
    assert_eq!(runtime.thread_todo("thread"), Some(source_list));
    release.notify_one();
    phase(&mut events, child, AgentRunPhase::Waiting).await;
    runtime
        .send_message(child, "Next user turn".into())
        .unwrap();
    phase(&mut events, child, AgentRunPhase::Waiting).await;
    runtime.stop(child, crate::StopScope::SelfOnly).unwrap();
    assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Stopped);
    assert_eq!(runtime.wait(source).await.unwrap(), AgentRunPhase::Done);
}

#[tokio::test]
async fn delegated_runs_cannot_write_even_with_conversation_flag_and_binding() {
    let (runtime, model, _) = harness(vec![
        write(vec![item("Forbidden", ThreadTodoStatus::Pending)]),
        Step::Stop,
    ]);
    let run = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", run).unwrap();
    runtime.spawn_reserved(
        run,
        Some(RunId::new(999)),
        Role::Worker,
        "Child work",
        config(),
    );
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    assert!(runtime.thread_todo("thread").is_none());
    let requests = model.requests.lock().unwrap();
    assert!(!requests[0].2.iter().any(|s| s.name == "todo_write"));
    assert!(results(&requests)[0].0);
}

#[tokio::test]
async fn compaction_restores_latest_list_after_the_whole_tool_batch_including_clear() {
    for clear in [false, true] {
        let latest = if clear {
            vec![]
        } else {
            vec![item("Latest step", ThreadTodoStatus::InProgress)]
        };
        let (runtime, model, mut events) = harness(vec![
            write(vec![item("Old step", ThreadTodoStatus::Completed)]),
            Step::Stop,
            Step::Tools(vec![
                ("compact", json!({})),
                ("todo_write", json!({"items":latest})),
            ]),
            Step::Stop,
        ]);
        let runtime = runtime.with_compaction(config::CompactionConfig {
            keep_recent_tokens: 1,
            cooldown_turns: 0,
            summarizer: config::SummarizerKind::Structural,
            context_window_tokens: 1_000_000,
            ..Default::default()
        });
        let run = start(
            &runtime,
            Role::Orchestrator,
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..config()
            },
            true,
        );
        phase(&mut events, run, AgentRunPhase::Waiting).await;
        runtime
            .send_message(run, "Update the current procedure".into())
            .unwrap();
        phase(&mut events, run, AgentRunPhase::Waiting).await;
        let snapshot = runtime.thread_todo("thread").unwrap();
        assert_eq!(snapshot.items, latest);
        {
            let requests = model.requests.lock().unwrap();
            let messages = &requests.last().unwrap().1;
            let procedure=messages.iter().position(|message|message.content.iter().any(|block|matches!(block,ContentBlock::Text{text} if text.ends_with(&json!(snapshot).to_string())))).expect("authoritative snapshot after compaction");
            assert!(messages[..procedure].iter().any(|message|message.content.iter().any(|block|matches!(block,ContentBlock::Text{text} if text.starts_with("[COMPACTION CHECKPOINT")))));
            let result_positions:Vec<_>=messages.iter().enumerate().filter_map(|(index,message)|message.content.iter().any(|block|matches!(block,ContentBlock::ToolResult{tool_call_id,..} if tool_call_id.starts_with("call-3-"))).then_some(index)).collect();
            assert_eq!(result_positions.len(), 2);
            assert!(
                result_positions
                    .into_iter()
                    .all(|position| position < procedure)
            );
        }
        runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
        assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Stopped);
    }
}

#[tokio::test]
async fn restart_retries_interrupted_provider_input_before_appending_authoritative_todo() {
    let directory = tempfile::tempdir().unwrap();
    let storage_config = storage::StorageConfig {
        db_path: directory.path().join("runs.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (original, old_model, _) = harness(vec![
        write(vec![item("Keep procedure", ThreadTodoStatus::InProgress)]),
        Step::Hold(entered.clone(), release),
    ]);
    let original =
        original.with_run_store(crate::RunStore::open(&storage_config, storage.handle()).unwrap());
    let run = start(
        &original,
        Role::Worker,
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..config()
        },
        true,
    );
    entered.notified().await;
    original.stop(run, crate::StopScope::SelfOnly).unwrap();
    assert_eq!(original.wait(run).await.unwrap(), AgentRunPhase::Stopped);
    let snapshot = original.thread_todo("thread").unwrap();
    let interrupted = old_model.requests.lock().unwrap().last().unwrap().clone();
    let (runtime, model, mut events) = harness(vec![Step::Stop, Step::Stop]);
    let runtime =
        runtime.with_run_store(crate::RunStore::open(&storage_config, storage.handle()).unwrap());
    runtime.restore_thread_todo(snapshot.clone()).unwrap();
    runtime.bind_thread_root("thread", run).unwrap();
    runtime.resume_chat(run, config()).unwrap();
    phase(&mut events, run, AgentRunPhase::Waiting).await;
    assert_eq!(model.requests.lock().unwrap()[0], interrupted);
    runtime
        .send_message(run, "Next user request".into())
        .unwrap();
    phase(&mut events, run, AgentRunPhase::Waiting).await;
    {
        let requests = model.requests.lock().unwrap();
        assert!(contains_snapshot(&requests[1].1, &snapshot));
        assert_eq!(requests[0].1, requests[1].1[..requests[0].1.len()]);
    }
    runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Stopped);
}

#[tokio::test]
async fn exhausted_restored_revision_rejects_write_and_handoff_before_any_transfer() {
    let (runtime, _, mut events) = harness(vec![]);
    let source = runtime.reserve_run_id();
    let target = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", source).unwrap();
    let mut snapshot = restored(vec![item("Keep", ThreadTodoStatus::InProgress)]);
    snapshot.revision = u64::MAX;
    runtime.restore_thread_todo(snapshot.clone()).unwrap();
    assert!(runtime.write_thread_todo(source, vec![]).is_err());
    let transferred = std::sync::atomic::AtomicBool::new(false);
    assert!(
        runtime
            .handoff_thread_goal(source, target, None, |_| {
                transferred.store(true, std::sync::atomic::Ordering::SeqCst);
            })
            .is_err()
    );
    assert!(!transferred.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(runtime.thread_todo("thread"), Some(snapshot));
    assert_eq!(runtime.goal_thread(source), Some("thread".into()));
    assert!(runtime.goal_thread(target).is_none());
    assert!(events.drain_pending_snapshot().is_empty());
}

#[tokio::test]
async fn revoked_owner_cannot_publish_a_procedure_update() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owners.db");
    let mut registry = crate::ownership::Registry::open(&path).unwrap();
    let owner = crate::ownership::ThreadOwner::new(
        "thread".into(),
        crate::ownership::Lease {
            owner_id: "original".into(),
            generation: 1,
            expires_at: 1,
        },
    );
    registry.start(&owner).unwrap();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (runtime, _, mut events) = harness(vec![Step::GatedTools(
        entered.clone(),
        release.clone(),
        vec![(
            "todo_write",
            json!({"items":[{"content":"Unauthorized change","status":"completed"}]}),
        )],
    )]);
    let run = start(
        &runtime,
        Role::Worker,
        RunConfig {
            ownership: Some(crate::ownership::OwnerPermit {
                registry_path: path,
                thread_id: "thread".into(),
                lease: owner.lease.clone(),
                run_id: None,
            }),
            ..config()
        },
        true,
    );
    entered.notified().await;
    // Advance the registry's logical claim clock, without elapsed-time assertions.
    let current = registry.attach("thread").unwrap();
    registry
        .update("thread", |state| {
            state.claim(&current.lease, "replacement", current.lease.expires_at, 0)
        })
        .unwrap();
    release.notify_one();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    assert!(runtime.thread_todo("thread").is_none());
    assert!(
        events
            .drain_pending_snapshot()
            .iter()
            .all(|event| !matches!(
                event.kind,
                event_bus::EventKind::Orchestrator(OrchestratorEvent::ThreadTodoUpdated { .. })
            ))
    );
}
