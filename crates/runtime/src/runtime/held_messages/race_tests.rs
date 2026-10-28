use super::*;
use crate::mailbox::CompletionNotice;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use tokio::sync::{Notify, oneshot};

#[derive(Default)]
struct RestoringModel {
    requests: Mutex<Vec<Vec<Message>>>,
    resumed: Notify,
}

#[async_trait::async_trait]
impl AgentModel for RestoringModel {
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let parent = messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, ContentBlock::Text { text } if text == "PARENT"));
        if parent {
            let count = {
                let mut requests = self.requests.lock().unwrap();
                requests.push(messages.to_vec());
                requests.len()
            };
            if count > 1 {
                self.resumed.notify_one();
                return std::future::pending().await;
            }
        } else if !messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, ContentBlock::Text { text } if text == "CHILD"))
        {
            return std::future::pending().await;
        }
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "racing completion".into(),
                }],
            },
            finish_reason: FinishReason::Stop,
            usage: Default::default(),
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "restore-race".into()
    }
}

struct Release(std::sync::mpsc::Sender<()>);

impl Drop for Release {
    fn drop(&mut self) {
        // A failed assertion must not strand a spawn_blocking worker.
        let _ = self.0.send(());
    }
}

fn pause(runtime: &AgentRuntime, run: RunId, point: HookPoint) -> (oneshot::Receiver<()>, Release) {
    let (entered, waiting) = oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    runtime.on_execution(
        point,
        &runtime.execution_snapshot(run).unwrap(),
        move || {
            entered.send(()).unwrap();
            released.recv().unwrap();
        },
    );
    (waiting, Release(release))
}

#[derive(Clone, Copy, PartialEq)]
enum Observation {
    Same,
    None,
    PreviousGeneration,
}

#[derive(Clone, Copy, PartialEq)]
enum Boundary {
    BeforeRestoreGate,
    RestoreCommit,
    LiveCommit,
}

async fn race(boundary: Boundary, observation: Observation) {
    test_support::isolated_state_dir();
    let directory = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: directory.path().join("held-race.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let model = Arc::new(RestoringModel::default());
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let root = runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
    let parent = runtime
        .delegate_background_as_child(root, Role::Worker, "PARENT", RunConfig::default())
        .unwrap();
    assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Done);
    // Only this test's wake task may extract the report.
    runtime.shared.operator_stops.lock().unwrap().insert(parent);
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .unwrap();
    assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    let old = runtime
        .run_output(parent, child)
        .unwrap()
        .completion_notice
        .unwrap();
    if observation == Observation::PreviousGeneration {
        runtime.observe_completion(&old);
        runtime.spawn_run_with_handoff(
            child,
            Some(parent),
            Role::Worker,
            "CHILD".into(),
            RunConfig::default(),
            RunContinuation::Fresh,
            None,
        );
        assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    }
    let notice: Arc<CompletionNotice> = runtime
        .run_output(parent, child)
        .unwrap()
        .completion_notice
        .unwrap();
    runtime
        .shared
        .operator_stops
        .lock()
        .unwrap()
        .remove(&parent);
    let before = runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(parent)
        .unwrap()
        .unwrap();
    let previous_mailbox = runtime.run_mailbox(parent).unwrap();
    let point = if boundary == Boundary::RestoreCommit {
        HookPoint::RestoreMessagePrepared
    } else {
        HookPoint::HeldMessageTaken
    };
    let (entered, mut release) = pause(&runtime, parent, point);
    let worker = runtime.clone();
    let delivery = tokio::task::spawn_blocking(move || worker.wake_subagent(root, parent));
    entered.await.unwrap();
    assert!(
        runtime.shared.held_messages.lock().unwrap()[&parent].is_empty(),
        "the receipt is already outside both queues"
    );
    if boundary != Boundary::RestoreCommit {
        // An explicit message wins restoration while completion delivery is parked.
        runtime
            .send_agent_message(root, parent, AgentMessageKind::Send, "resume", None)
            .unwrap();
        model.resumed.notified().await;
        if boundary == Boundary::LiveCommit {
            let (entered, at_commit) = pause(&runtime, parent, HookPoint::LiveMessagePrepared);
            drop(release);
            release = at_commit;
            entered.await.unwrap();
        }
    }
    match observation {
        Observation::Same => runtime.observe_completion(&notice),
        Observation::None => {}
        Observation::PreviousGeneration => {
            assert_ne!(old.generation, notice.generation);
            runtime.observe_completion(&old);
        }
    }
    drop(release);
    delivery.await.unwrap();
    if boundary == Boundary::RestoreCommit {
        if observation == Observation::Same {
            assert_eq!(
                runtime.inspect_agent(parent).unwrap().phase,
                AgentRunPhase::Done
            );
            assert!(Arc::ptr_eq(
                &previous_mailbox,
                &runtime.run_mailbox(parent).unwrap()
            ));
            let after = runtime
                .shared
                .run_store
                .get()
                .unwrap()
                .restore_record(parent)
                .unwrap()
                .unwrap();
            assert_eq!(after.messages_json, before.messages_json);
            assert_eq!(after.config_json, before.config_json);
            assert_eq!(model.requests.lock().unwrap().len(), 1);
        } else {
            model.resumed.notified().await;
            let requests = model.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            let last = serde_json::to_string(&requests[1]).unwrap();
            assert_eq!(last.matches("[agent-message").count(), 1);
            assert!(last.contains(&format!("from={child}")));
        }
    } else {
        let mailbox = runtime.run_mailbox(parent).unwrap();
        assert!(
            mailbox.drain_interrupting().is_empty(),
            "automatic notice must never turn into an explicit message"
        );
        let messages = mailbox.drain_all();
        assert_eq!(
            messages.len(),
            usize::from(observation != Observation::Same)
        );
        if let Some(message) = messages.first() {
            assert_eq!(message.sender_run_id, child.to_string());
            assert_eq!(message.content, "racing completion");
        }
    }
}

#[tokio::test]
async fn held_receipt_survives_extraction_and_a_concurrent_explicit_restore() {
    for observation in [
        Observation::Same,
        Observation::None,
        Observation::PreviousGeneration,
    ] {
        race(Boundary::BeforeRestoreGate, observation).await;
    }
}

#[tokio::test]
async fn held_receipt_is_rechecked_before_restored_registration() {
    for observation in [
        Observation::Same,
        Observation::None,
        Observation::PreviousGeneration,
    ] {
        race(Boundary::RestoreCommit, observation).await;
    }
}

#[tokio::test]
async fn held_receipt_is_rechecked_at_live_mailbox_commit() {
    for observation in [
        Observation::Same,
        Observation::None,
        Observation::PreviousGeneration,
    ] {
        race(Boundary::LiveCommit, observation).await;
    }
}
