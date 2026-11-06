//! Saved-context compaction through the command service: the job reports its
//! result through `poll`, excludes other commands while it runs, and never
//! starts an agent turn.

use event_bus::EventBus;
use event_bus::{AgentRunPhase, EventKind, LifecycleEvent};
use runtime::{AgentRuntime, Role, RunConfig, RunId};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use workbench_host::CommandHost;
use workbench_host::commands::{ChatInput, CommandService, ContinueInput, HostCommand, HostEvent};

fn submit_as_desktop(host: &mut CommandHost, command: HostCommand) -> Vec<HostEvent> {
    host.submit_local(command, &workbench_host::provenance::local_desktop())
}

struct Model(AtomicUsize);
#[async_trait::async_trait]
impl runtime::AgentModel for Model {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(providers::ChatResponse {
            usage_reported: true,
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "older analysis ".repeat(30),
                }],
            },
            finish_reason: providers::FinishReason::Stop,
            usage: Default::default(),
        })
    }
}

async fn waiting(events: &mut event_bus::EventReceiver, run: RunId) {
    loop {
        if matches!(events.recv().await.unwrap().kind, EventKind::Lifecycle(
            LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Waiting, .. }
        ) if run_id == run.to_string())
        {
            return;
        }
    }
}

#[tokio::test]
async fn saved_command_reports_completion_or_failure_and_never_starts_an_agent_turn() {
    for max_bytes in [0, 16_384] {
        for restored_only in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let config = storage::StorageConfig {
                db_path: dir.path().join("saved.db"),
                ..Default::default()
            };
            let storage = storage::Storage::open(config.clone()).unwrap();
            let model = Arc::new(Model(AtomicUsize::new(0)));
            let bus = Arc::new(EventBus::new(128));
            let mut events = bus.subscribe();
            let make_runtime = || {
                AgentRuntime::new(
                    bus.clone(),
                    Arc::new(tools::ToolExecutor::new(bus.clone())),
                    model.clone(),
                )
                .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap())
                .with_compaction(config::CompactionConfig {
                    context_window_tokens: 1_000_000,
                    keep_recent_tokens: 1,
                    summarizer: config::SummarizerKind::Structural,
                    max_summary_bytes: max_bytes,
                    ..Default::default()
                })
            };
            let runtime = make_runtime();
            let run = runtime.delegate_background(
                Role::Worker,
                "original goal".into(),
                RunConfig::default(),
            );
            runtime.wait(run).await.unwrap();
            runtime
                .continue_goal(run, "latest turn".into(), RunConfig::default())
                .unwrap();
            waiting(&mut events, run).await;
            runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
            runtime.wait(run).await.unwrap();
            let runtime = if restored_only {
                make_runtime()
            } else {
                runtime
            };
            let mut sink = CommandHost::new(CommandService::new(
                runtime.clone(),
                tokio::runtime::Handle::current(),
            ));
            sink.service_mut()
                .bind_goal_context("thread", "project", &run.to_string());
            let before = model.0.load(Ordering::SeqCst);
            let count = runtime.list_agents().len();
            events.drain_pending_snapshot();
            assert!(
                matches!(submit_as_desktop(&mut sink, HostCommand::CompactChat { thread_id: "thread".into() }).as_slice(),
                [HostEvent::ChatNotice { text, .. }] if text.starts_with("Compacting saved"))
            );
            assert!(
                sink.service().has_background_work(),
                "the result is only collected by poll"
            );
            for command in [
                HostCommand::CompactChat {
                    thread_id: "thread".into(),
                },
                HostCommand::StopChat {
                    thread_id: "thread".into(),
                },
                HostCommand::ContinueChat(ContinueInput {
                    project_root: None,
                    thread_id: "thread".into(),
                    composer_role: Default::default(),
                    model_preference: None,
                    automatic: false,
                }),
            ] {
                assert!(
                    matches!(submit_as_desktop(&mut sink, command).as_slice(), [HostEvent::ChatNotice { text, .. }]
                    if text.contains("in progress"))
                );
            }
            assert!(matches!(submit_as_desktop(&mut sink, HostCommand::SendChat(
                ChatInput {
                    submission_id: "not-accepted".into(),
                    project_root: None,
                    composer_role: Default::default(),
                    images: Vec::new(),
                    thread_id: "thread".into(),
                    text: "keep this draft".into(),
                    model_preference: None,
                    fork_seed: None,
                    skills: Vec::new(),
                }
            )).as_slice(), [HostEvent::ChatRejected { reason, .. }] if reason.contains("in progress")));
            sink.service().background_work_notify().notified().await;
            let progress = events.drain_pending_snapshot();
            let starts: Vec<_> = progress
                .iter()
                .filter_map(|event| match &event.kind {
                    EventKind::Lifecycle(LifecycleEvent::RunProgress {
                        compaction_operation_id: Some(id),
                        activity: event_bus::RunActivity::Compaction,
                        ..
                    }) => Some(id),
                    _ => None,
                })
                .collect();
            assert_eq!(starts.len(), 1);
            let terminals: Vec<_> = progress
                .iter()
                .filter_map(|event| match &event.kind {
                    EventKind::Compaction(event_bus::CompactionEvent::Compacted {
                        operation_id,
                        ..
                    }) if max_bytes > 0 => Some(operation_id),
                    EventKind::Compaction(event_bus::CompactionEvent::Aborted {
                        operation_id,
                        ..
                    }) if max_bytes == 0 => Some(operation_id),
                    _ => None,
                })
                .collect();
            assert_eq!(starts, terminals);
            let result = sink.service_mut().poll();
            assert!(
                matches!(result.as_slice(), [HostEvent::ChatNotice { thread_id, text, .. }]
                if thread_id == "thread" && if max_bytes == 0 {
                    text.starts_with("Could not compact saved conversation")
                } else { text.contains("complete. Use /continue") })
            );
            assert!(sink.service_mut().poll().is_empty());
            assert!(!sink.service().has_background_work());
            assert_eq!(model.0.load(Ordering::SeqCst), before);
            assert_eq!(runtime.list_agents().len(), count);
            if !restored_only {
                assert_eq!(
                    runtime.inspect_agent(run).unwrap().phase,
                    AgentRunPhase::Stopped
                );
            }
        }
    }
}
