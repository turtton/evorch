mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolResultContent, ToolSpec};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, DelegateImage, RunConfig, RunId, RuntimeError,
};
use serde_json::{Value, json};
use support::{text_response, tool_response, tool_responses};
use tokio::sync::{Mutex, Notify};
use tools::ToolExecutor;

struct WaitingModel {
    turn: AtomicUsize,
    parent_requests: Mutex<Vec<(Vec<Message>, Vec<ToolSpec>)>>,
    wait_requested: Notify,
    release_wait: Notify,
    child_started: Notify,
    release_child: Notify,
    batch: bool,
}

#[async_trait::async_trait]
impl AgentModel for WaitingModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        if role == Role::Worker {
            self.child_started.notify_one();
            self.release_child.notified().await;
            return Ok(text_response("child completed", FinishReason::Stop));
        }
        self.parent_requests
            .lock()
            .await
            .push((messages.to_vec(), tools.to_vec()));
        Ok(match self.turn.fetch_add(1, Ordering::SeqCst) {
            0 => tool_response(
                "delegate",
                "delegate",
                json!({"prompt":"CHILD", "background":true}),
            ),
            1 => {
                self.wait_requested.notify_one();
                self.release_wait.notified().await;
                if self.batch {
                    tool_responses([
                        (
                            "wait-first",
                            "wait",
                            json!({"run_ids":["run-2"],"mode":"all"}),
                        ),
                        (
                            "wait-second",
                            "wait",
                            json!({"run_ids":["run-2"],"mode":"all"}),
                        ),
                    ])
                } else {
                    tool_response("wait-first", "wait", json!({"run_id":"run-2"}))
                }
            }
            2 => tool_response("finish", "finish", json!({"result":"handled user input"})),
            turn => panic!("unexpected parent turn {turn}"),
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "waiting-model".into()
    }
}

#[derive(Clone, Copy)]
enum Delivery {
    BeforeWait,
    DuringWait,
    CancelledDuringWait,
    WithCompletion,
}

async fn scenario(delivery: Delivery, batch: bool) {
    let model = Arc::new(WaitingModel {
        turn: AtomicUsize::new(0),
        parent_requests: Mutex::new(Vec::new()),
        wait_requested: Notify::new(),
        release_wait: Notify::new(),
        child_started: Notify::new(),
        release_child: Notify::new(),
        batch,
    });
    let bus = Arc::new(EventBus::new(128));
    let mut events = bus.subscribe();
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone());
    let parent =
        runtime.delegate_background(Role::Orchestrator, "PARENT".into(), RunConfig::default());
    model.wait_requested.notified().await;
    model.child_started.notified().await;
    if matches!(
        delivery,
        Delivery::DuringWait | Delivery::CancelledDuringWait
    ) {
        model.release_wait.notify_one();
        loop {
            if matches!(events.recv().await.unwrap().kind,
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to: AgentRunPhase::Waiting, .. })
                    if run_id == parent.to_string())
            {
                break;
            }
        }
    } else if matches!(delivery, Delivery::WithCompletion) {
        // Make terminal observation and queued user input ready together. The
        // completion arm wins, so input must survive in the receiver.
        model.release_child.notify_one();
        runtime.wait(RunId::new(2)).await.unwrap();
    }
    runtime
        .send_message_with_images(
            parent,
            "Please inspect this image".into(),
            vec![DelegateImage {
                media_type: "image/png".into(),
                data: "aW1hZ2U=".into(),
            }],
        )
        .unwrap();
    runtime
        .send_message(parent, "Use the updated requirement".into())
        .unwrap();
    if !matches!(
        delivery,
        Delivery::DuringWait | Delivery::CancelledDuringWait
    ) {
        model.release_wait.notify_one();
    }
    if matches!(delivery, Delivery::CancelledDuringWait) {
        runtime.cancel(parent).unwrap();
        assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Error);
        assert_eq!(model.parent_requests.lock().await.len(), 2);
        model.release_child.notify_one();
        runtime.wait(RunId::new(2)).await.unwrap();
        return;
    }
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), runtime.wait(parent))
            .await
            .expect("wait must wake immediately")
            .unwrap(),
        AgentRunPhase::Done,
    );
    let requests = model.parent_requests.lock().await;
    assert_eq!(requests.len(), 3);
    for pair in requests.windows(2) {
        assert!(
            pair[1].0.starts_with(&pair[0].0),
            "previous provider input prefix changed"
        );
        assert_eq!(pair[1].1, pair[0].1, "tool schema or order changed");
    }
    let messages = &requests[2].0;
    let input_index = messages
        .iter()
        .position(|message| {
            message.content.first()
                == Some(&ContentBlock::Text {
                    text: "Please inspect this image".into(),
                })
        })
        .expect("UI text reaches the next model request");
    assert_eq!(
        messages[input_index].content,
        vec![
            ContentBlock::Text {
                text: "Please inspect this image".into()
            },
            ContentBlock::Image {
                media_type: "image/png".into(),
                data: "aW1hZ2U=".into()
            },
        ]
    );
    assert_eq!(
        messages[input_index + 1].content,
        vec![ContentBlock::Text {
            text: "Use the updated requirement".into(),
        }]
    );
    let mut wait_results = 0;
    for (index, message) in messages.iter().enumerate() {
        for block in &message.content {
            if let ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } = block
                && tool_call_id.starts_with("wait-")
            {
                assert!(
                    index < input_index,
                    "UI input split ToolUse from ToolResult"
                );
                assert!(!is_error);
                let ToolResultContent::Text { text } = &content[0];
                let result: Value = serde_json::from_str(text).unwrap();
                assert_eq!(result["user_input_ready"], true);
                assert_eq!(result["inbox_ready"], false);
                assert_eq!(result["timed_out"], false);
                assert_eq!(
                    result["runs"][0]["status"],
                    if matches!(delivery, Delivery::WithCompletion) {
                        "completed"
                    } else {
                        "still_running"
                    }
                );
                wait_results += 1;
            }
        }
    }
    assert_eq!(wait_results, if batch { 2 } else { 1 });
    drop(requests);
    model.release_child.notify_one();
    runtime.wait(RunId::new(2)).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn user_input_with_images_interrupts_wait_and_reaches_next_model_request() {
    scenario(Delivery::DuringWait, false).await;
}

#[tokio::test(start_paused = true)]
async fn queued_user_input_interrupts_every_wait_in_the_same_tool_batch() {
    scenario(Delivery::BeforeWait, true).await;
}

#[tokio::test(start_paused = true)]
async fn user_input_is_preserved_when_terminal_observation_wins_the_receive_race() {
    scenario(Delivery::WithCompletion, false).await;
}

#[tokio::test(start_paused = true)]
async fn cancellation_wins_when_user_input_arrives_in_the_same_wait() {
    scenario(Delivery::CancelledDuringWait, false).await;
}
