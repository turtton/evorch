use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

#[derive(Default)]
struct ReservingShell {
    floor: AtomicU64,
    fail: AtomicBool,
    reservations: AtomicUsize,
}

#[async_trait::async_trait]
impl tools::Tool for ReservingShell {
    fn name(&self) -> &str {
        "shell"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn permissions(&self) -> tools::Permissions {
        tools::Permissions::process()
    }
    async fn execute(&self, _: serde_json::Value) -> Result<tools::ToolResult, tools::ToolError> {
        unreachable!("restoring history never replays shell")
    }
    fn reserve_shell_job_handles(&self, next: u64) -> Result<(), tools::ToolError> {
        self.reservations.fetch_add(1, Ordering::Relaxed);
        if self.fail.load(Ordering::Relaxed) {
            return Err(tools::ToolError::Io {
                detail: "forced allocator failure".into(),
            });
        }
        self.floor.fetch_max(next, Ordering::Relaxed);
        Ok(())
    }
}

#[tokio::test]
async fn quoted_huge_handles_restore_without_replaying_shell_or_rewriting_history() {
    test_support::isolated_state_dir();
    for entry in ["same_run", "new_chat", "fork"] {
        for (raw, summary) in [
            ("job-12", "job-99, job-184467440737095516160"),
            ("job-12, job-18446744073709551614", "job-99"),
        ] {
            let fixture = Fixture::new();
            let shell = Arc::new(ReservingShell::default());
            let mut executor = ToolExecutor::new(fixture.runtime.shared.bus.clone());
            executor.register(shell.clone()).unwrap();
            *fixture.runtime.shared.executor.lock().unwrap() = Arc::new(executor);
            let run = fixture
                .runtime
                .delegate_chat(
                    "shell-thread",
                    Role::Worker,
                    "original".into(),
                    RunConfig::default(),
                )
                .unwrap();
            fixture.runtime.wait(run).await.unwrap();
            let store = fixture.runtime.shared.run_store.get().unwrap();
            let mut record = store.restore_record(run).unwrap().unwrap();
            let mut history: Vec<providers::Message> =
                serde_json::from_str(&record.messages_json).unwrap();
            history.splice(
                0..0,
                [
                    providers::Message {
                        role: providers::Role::User,
                        content: vec![providers::ContentBlock::Text { text: raw.into() }],
                    },
                    providers::Message {
                        role: providers::Role::Assistant,
                        content: vec![providers::ContentBlock::ToolUse {
                            id: "saved-shell".into(),
                            name: "shell".into(),
                            input: serde_json::json!({
                                "command": "printf 'job-20 job-18446744073709551615'"
                            }),
                        }],
                    },
                    providers::Message {
                        role: providers::Role::User,
                        content: vec![providers::ContentBlock::ToolResult {
                            tool_call_id: "saved-shell".into(),
                            content: vec![providers::ToolResultContent::Text {
                                text: "review: job-30 job-4294967295 job-18446744073709551614"
                                    .into(),
                            }],
                            is_error: false,
                        }],
                    },
                ],
            );
            let context_len = history
                .iter()
                .filter(|message| message.role != providers::Role::System)
                .count() as u64;
            record.messages_json = serde_json::to_string(&history).unwrap();
            record.checkpoints_json = serde_json::to_string(&[crate::CompactionCheckpoint {
                id: "summary".into(),
                summary: providers::Message {
                    role: providers::Role::User,
                    content: vec![providers::ContentBlock::Text {
                        text: summary.into(),
                    }],
                },
                range: (0, 1),
            }])
            .unwrap();
            fixture
                .storage
                .handle()
                .upsert_run_context(&record)
                .unwrap();
            let before = store.restore_record(run).unwrap().unwrap();
            let mut events = fixture.runtime.shared.bus.subscribe();
            let config = RunConfig {
                interactive: true,
                ..RunConfig::default()
            };
            let result = match entry {
                "same_run" => fixture
                    .runtime
                    .continue_goal(run, "continue".into(), config),
                "new_chat" => fixture.runtime.delegate_chat(
                    "shell-thread",
                    Role::Worker,
                    "continue".into(),
                    config,
                ),
                "fork" => fixture.runtime.delegate_chat_seeded(
                    "fork-thread",
                    Role::Worker,
                    "continue".into(),
                    config,
                    Some(crate::ChatForkSeed {
                        source_run_id: run.to_string(),
                        context_len,
                    }),
                ),
                _ => unreachable!(),
            };
            let resumed = result.unwrap_or_else(|error| panic!("{entry}: {raw}: {error}"));
            assert_eq!(shell.reservations.load(Ordering::Relaxed), 1);
            assert_eq!(shell.floor.load(Ordering::Relaxed), 100);
            let phase = loop {
                let event = events.recv().await.unwrap();
                if let event_bus::EventKind::Lifecycle(
                    event_bus::LifecycleEvent::AgentRunStateChanged { run_id, to, .. },
                ) = event.kind
                    && run_id == resumed.to_string()
                    && matches!(
                        to,
                        AgentRunPhase::Waiting
                            | AgentRunPhase::Done
                            | AgentRunPhase::Error
                            | AgentRunPhase::Stopped
                    )
                {
                    assert!(
                        matches!(to, AgentRunPhase::Waiting | AgentRunPhase::Done),
                        "{entry}: {to:?}",
                    );
                    break to;
                }
            };
            if phase == AgentRunPhase::Waiting {
                fixture.runtime.cancel(resumed).unwrap();
            }
            fixture.runtime.wait(resumed).await.unwrap();
            let after = store.restore_record(resumed).unwrap().unwrap();
            let restored: Vec<providers::Message> =
                serde_json::from_str(&after.messages_json).unwrap();
            assert_eq!(&restored[..history.len()], history.as_slice());
            assert_eq!(after.checkpoints_json, before.checkpoints_json);
            if entry != "same_run" {
                assert_eq!(store.restore_record(run).unwrap().unwrap(), before);
            }
        }
    }
}

#[tokio::test]
async fn legacy_history_is_reserved_before_same_run_resume_or_new_chat_and_failures_refuse_restore()
{
    test_support::isolated_state_dir();
    for same_run in [true, false] {
        let fixture = Fixture::new();
        let shell = Arc::new(ReservingShell::default());
        let mut executor = ToolExecutor::new(fixture.runtime.shared.bus.clone());
        executor.register(shell.clone()).unwrap();
        *fixture.runtime.shared.executor.lock().unwrap() = Arc::new(executor);
        let run = fixture
            .runtime
            .delegate_chat(
                "shell-thread",
                Role::Worker,
                "original".into(),
                RunConfig::default(),
            )
            .unwrap();
        fixture.runtime.wait(run).await.unwrap();
        let store = fixture.runtime.shared.run_store.get().unwrap();
        let mut record = store.restore_record(run).unwrap().unwrap();
        let mut history: Vec<providers::Message> =
            serde_json::from_str(&record.messages_json).unwrap();
        history.insert(
            0,
            providers::Message {
                role: providers::Role::User,
                content: vec![providers::ContentBlock::Text {
                    text: "saved job-9001, earlier job-19; quoted job-18446744073709551614".into(),
                }],
            },
        );
        record.messages_json = serde_json::to_string(&history).unwrap();
        fixture
            .storage
            .handle()
            .upsert_run_context(&record)
            .unwrap();
        let before = store.restore_record(run).unwrap().unwrap().messages_json;
        let restore = || {
            if same_run {
                fixture
                    .runtime
                    .continue_goal(run, "continue".into(), RunConfig::default())
            } else {
                fixture.runtime.delegate_chat(
                    "shell-thread",
                    Role::Worker,
                    "continue".into(),
                    RunConfig::default(),
                )
            }
        };
        shell.fail.store(true, Ordering::Relaxed);
        assert!(matches!(
            restore(),
            Err(RuntimeError::RunRestoreFailed { .. })
        ));
        assert_eq!(
            store.restore_record(run).unwrap().unwrap().messages_json,
            before
        );
        assert!(store.restore_record(run).unwrap().unwrap().restorable);
        shell.fail.store(false, Ordering::Relaxed);
        let resumed = restore().unwrap();
        assert_eq!(shell.floor.load(Ordering::Relaxed), 9002);
        assert_eq!(resumed == run, same_run);
        fixture.runtime.cancel(resumed).unwrap();
        fixture.runtime.wait(resumed).await.unwrap();
    }
}
