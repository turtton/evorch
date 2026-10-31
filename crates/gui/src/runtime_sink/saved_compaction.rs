use super::*;

pub(super) struct CompactionJob {
    result: tokio::sync::oneshot::Receiver<LoopEvent>,
    task: tokio::task::JoinHandle<()>,
}

impl RuntimeCommandSink {
    pub(super) fn compact_chat(
        &mut self,
        thread_id: String,
        permit: Option<runtime::ownership::OwnerPermit>,
    ) -> Vec<LoopEvent> {
        let result: Result<String, String> = (|| {
            let run = *self
                .chat_runs
                .get(&thread_id)
                .or_else(|| self.goal_runs.get(&thread_id))
                .ok_or("No active conversation run to compact")?;
            if self.runtime.inspect_agent(run).ok().is_some_and(|run| {
                matches!(
                    run.phase,
                    event_bus::AgentRunPhase::Pending
                        | event_bus::AgentRunPhase::Running
                        | event_bus::AgentRunPhase::Waiting
                )
            }) {
                self.runtime
                    .compact(run)
                    .map_err(|error| format!("Could not request context compaction: {error}"))?;
                return Ok("Context compaction requested for the next turn boundary".into());
            }
            if self
                .runtime
                .restore_diagnostics(run)
                .map_err(|error| format!("Could not request context compaction: {error}"))?
                .is_none()
            {
                return Err(
                    "Could not request context compaction: no saved conversation context".into(),
                );
            }
            let mut authority = RunConfig {
                ownership: permit,
                ..Default::default()
            };
            self.renew_team_authority(&thread_id, run, None, &mut authority)?;
            let runtime = self.runtime.clone();
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let thread = thread_id.clone();
            let task = self.handle.spawn(async move {
                let text = match runtime.compact_saved_chat(run, authority).await {
                    Ok(()) => {
                        "Context compaction complete. Use /continue when ready to resume".into()
                    }
                    Err(error) => format!("Could not compact saved conversation: {error}"),
                };
                let _ = sender.send(LoopEvent::ChatNotice {
                    thread_id: thread,
                    text,
                });
            });
            self.compaction_jobs.insert(
                thread_id.clone(),
                CompactionJob {
                    result: receiver,
                    task,
                },
            );
            Ok("Compacting saved conversation without starting an agent turn".into())
        })();
        vec![LoopEvent::ChatNotice {
            thread_id,
            text: result.unwrap_or_else(|error| error),
        }]
    }

    pub(super) fn poll_compactions(&mut self) -> Vec<LoopEvent> {
        let mut events = Vec::new();
        self.compaction_jobs.retain(|thread, job| {
            if !job.task.is_finished() { return true; }
            match job.result.try_recv() {
                Ok(event) => { events.push(event); false }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => true,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    events.push(LoopEvent::ChatNotice { thread_id: thread.clone(),
                        text: "Saved context compaction task ended without a result; retry /compaction".into() });
                    false
                }
            }
        });
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::{AgentRunPhase, EventKind, LifecycleEvent};
    use std::sync::atomic::{AtomicUsize, Ordering};

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
                let mut sink =
                    RuntimeCommandSink::new(runtime.clone(), tokio::runtime::Handle::current());
                sink.bind_goal_context("thread", "project", &run.to_string());
                let before = model.0.load(Ordering::SeqCst);
                let count = runtime.list_agents().len();
                assert!(
                    matches!(sink.submit(WorkbenchCommand::CompactChat { thread_id: "thread".into() }).as_slice(),
                    [LoopEvent::ChatNotice { text, .. }] if text.starts_with("Compacting saved"))
                );
                assert!(
                    sink.has_background_work(),
                    "the result is only collected by poll"
                );
                for command in [
                    WorkbenchCommand::CompactChat {
                        thread_id: "thread".into(),
                    },
                    WorkbenchCommand::StopChat {
                        thread_id: "thread".into(),
                    },
                    WorkbenchCommand::ContinueChat(crate::model::commands::ChatContinuation {
                        project_root: None,
                        thread_id: "thread".into(),
                        composer_role: Default::default(),
                        model_preference: None,
                        automatic: false,
                    }),
                ] {
                    assert!(
                        matches!(sink.submit(command).as_slice(), [LoopEvent::ChatNotice { text, .. }]
                        if text.contains("in progress"))
                    );
                }
                assert!(matches!(sink.submit(WorkbenchCommand::SendChat(
                    crate::model::commands::ChatSubmission {
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
                )).as_slice(), [LoopEvent::ChatRejected { reason, .. }] if reason.contains("in progress")));
                (&mut sink.compaction_jobs.get_mut("thread").unwrap().task)
                    .await
                    .unwrap();
                let result = sink.poll();
                assert!(
                    matches!(result.as_slice(), [LoopEvent::ChatNotice { thread_id, text }]
                    if thread_id == "thread" && if max_bytes == 0 {
                        text.starts_with("Could not compact saved conversation")
                    } else { text.contains("complete. Use /continue") })
                );
                assert!(sink.poll().is_empty());
                assert!(!sink.has_background_work());
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
}
