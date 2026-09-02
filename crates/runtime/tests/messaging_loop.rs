mod support;

// allow: SIZE_OK — 4 つの AC6 シナリオが共有する同期 helper を同一 integration suite に置く。

use std::sync::Arc;

use agents::Role;
use event_bus::{
    AgentMessageEvent, AgentMessageKind, AgentRunPhase, DeliveryDisposition, Event, EventBus,
    EventKind, LifecycleEvent,
};
use providers::{ContentBlock, FinishReason, Message, Role as MessageRole};
use runtime::{AgentRuntime, RunConfig, RunId};
use sandbox::DirectSandbox;
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response, tool_response};

fn runtime_with(model: Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (AgentRuntime::new(Arc::clone(&bus), executor, model), bus)
}

async fn wait_for_observed(model: &ScriptedModel, count: usize) -> Vec<Vec<Message>> {
    timeout(Duration::from_secs(2), async {
        loop {
            let observed = model.observed().await;
            if observed.len() >= count {
                return observed;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("model observation timeout")
}

async fn wait_for_phase(runtime: &AgentRuntime, run_id: RunId, phase: AgentRunPhase) {
    timeout(Duration::from_secs(2), async {
        loop {
            if runtime
                .inspect_agent(run_id)
                .expect("run remains inspectable")
                .phase
                == phase
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("phase transition timeout");
}

async fn events_until_terminal(
    receiver: &mut event_bus::EventReceiver,
    run_id: RunId,
) -> Vec<Event> {
    let run_id = run_id.to_string();
    let mut events = Vec::new();
    loop {
        let event = timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("event timeout")
            .expect("event receiver remains open");
        let terminal = matches!(
            &event.kind,
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id: event_run_id,
                to: AgentRunPhase::Done | AgentRunPhase::Error,
                ..
            }) if event_run_id == &run_id
        );
        events.push(event);
        if terminal {
            return events;
        }
    }
}

fn messages_for_marker(observed: &[Vec<Message>], marker: &str) -> Vec<Vec<Message>> {
    observed
        .iter()
        .filter(|messages| {
            messages.first().is_some_and(|message| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text == marker))
            })
        })
        .cloned()
        .collect()
}

fn contains_agent_message(messages: &[Message], header: &str, content: &str) -> bool {
    messages.iter().any(|message| {
        message.role == MessageRole::User
            && message.content.iter().any(|block| {
                matches!(block, ContentBlock::Text { text } if text.starts_with(header) && text.contains(content))
            })
    })
}

#[tokio::test]
async fn running_recipient_injects_parent_steering_before_next_completion() {
    // Given: Waiting の親 run-1 と、最初の completion が tool use で gate 中の子 run-2
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated([], Arc::clone(&gate)));
    model
        .add_keyed("PARENT", [Ok(text_response("waiting", FinishReason::Stop))])
        .await;
    model
        .add_keyed(
            "CHILD",
            [
                Ok(tool_response(
                    "denied-delegate",
                    "delegate_background",
                    json!({ "role": "worker", "prompt": "unused" }),
                )),
                Ok(text_response("done", FinishReason::Stop)),
            ],
        )
        .await;
    let (runtime, _bus) = runtime_with(Arc::clone(&model));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    let _parent_request = wait_for_observed(&model, 1).await;
    gate.notify_one();
    wait_for_phase(&runtime, parent, AgentRunPhase::Waiting).await;
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .expect("parent exists");
    let _first_child_request = wait_for_observed(&model, 2).await;

    // When: 子の in-flight completion 中に親からメッセージを配送して completion を解放する
    assert_eq!(
        runtime.send_agent_message(
            parent,
            child,
            AgentMessageKind::Send,
            "steering content",
            None,
        ),
        Ok("msg-1".to_string())
    );
    gate.notify_one();
    let observed = wait_for_observed(&model, 3).await;
    gate.notify_one();

    // Then: 次の child request に親発メッセージが user context として注入される
    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Done));
    let child_requests = messages_for_marker(&observed, "CHILD");
    assert_eq!(child_requests.len(), 2);
    assert!(contains_agent_message(
        &child_requests[1],
        "[agent-message id=msg-1 from=run-1 kind=send]",
        "steering content"
    ));
    assert_eq!(runtime.cancel(parent), Ok(()));
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Error));
}

#[tokio::test]
async fn running_recipient_holds_child_aside_until_turn_end() {
    // Given: 親 run-1 の最初の completion が gate 中で、子 run-2 が存在する
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated([], Arc::clone(&gate)));
    model
        .add_keyed(
            "PARENT",
            [
                Ok(text_response("turn stop", FinishReason::Stop)),
                Ok(text_response("final", FinishReason::Stop)),
            ],
        )
        .await;
    model
        .add_keyed(
            "CHILD",
            [Ok(text_response("child done", FinishReason::Stop))],
        )
        .await;
    let (runtime, _bus) = runtime_with(Arc::clone(&model));
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .expect("parent exists");
    let initial_requests = wait_for_observed(&model, 2).await;

    // When: 親の request が in-flight の間に子から aside を配送して Stop を解放する
    assert_eq!(
        runtime.send_agent_message(child, parent, AgentMessageKind::Send, "aside content", None,),
        Ok("msg-1".to_string())
    );
    gate.notify_waiters();
    let observed = wait_for_observed(&model, 3).await;
    gate.notify_waiters();

    // Then: in-flight request には無く、Stop boundary 後の継続 request にだけ注入されて完了する
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    let initial_parent = messages_for_marker(&initial_requests, "PARENT");
    assert_eq!(initial_parent.len(), 1);
    assert!(!contains_agent_message(
        &initial_parent[0],
        "[agent-message id=msg-1 from=run-2 kind=send]",
        "aside content"
    ));
    let parent_requests = messages_for_marker(&observed, "PARENT");
    assert_eq!(parent_requests.len(), 2);
    assert!(contains_agent_message(
        &parent_requests[1],
        "[agent-message id=msg-1 from=run-2 kind=send]",
        "aside content"
    ));
}

#[tokio::test]
async fn waiting_recipient_wakes_to_running_on_agent_message() {
    // Given: 親 run-1 と、最初の Stop で Waiting になった interactive 子 run-2
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "PARENT",
            [Ok(text_response("parent waiting", FinishReason::Stop))],
        )
        .await;
    model
        .add_keyed(
            "CHILD",
            [
                Ok(text_response("need input", FinishReason::Stop)),
                Ok(text_response("woke", FinishReason::Stop)),
            ],
        )
        .await;
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let mut receiver = bus.subscribe();
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig {
            interactive: true,
            ..RunConfig::default()
        },
    );
    wait_for_phase(&runtime, parent, AgentRunPhase::Waiting).await;
    let child = runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "CHILD",
            RunConfig {
                interactive: true,
                ..RunConfig::default()
            },
        )
        .expect("parent exists");
    wait_for_phase(&runtime, child, AgentRunPhase::Waiting).await;

    // When: Waiting の子へ親から AgentMessage を配送する
    assert_eq!(
        runtime.send_agent_message(parent, child, AgentMessageKind::Send, "wake content", None,),
        Ok("msg-1".to_string())
    );
    let events = events_until_terminal(&mut receiver, child).await;

    // Then: Wake disposition、Running 復帰、注入後 completion、Done が観測される
    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Done));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::AgentMessage(AgentMessageEvent::Delivered {
            disposition: DeliveryDisposition::Wake,
            ..
        })
    )));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id,
            from: AgentRunPhase::Waiting,
            to: AgentRunPhase::Running,
            ..
        }) if run_id == &child.to_string()
    )));
    let observed = model.observed().await;
    let child_requests = messages_for_marker(&observed, "CHILD");
    assert_eq!(child_requests.len(), 2);
    assert!(contains_agent_message(
        &child_requests[1],
        "[agent-message id=msg-1 from=run-1 kind=send]",
        "wake content"
    ));
    assert_eq!(runtime.cancel(parent), Ok(()));
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Error));
}

#[tokio::test]
async fn aside_flush_does_not_override_cancel() {
    // Given: gate 中の親 run-1 と、その子 run-2
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated([], Arc::clone(&gate)));
    model
        .add_keyed("PARENT", [Ok(text_response("unused", FinishReason::Stop))])
        .await;
    model
        .add_keyed("CHILD", [Ok(text_response("unused", FinishReason::Stop))])
        .await;
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let mut receiver = bus.subscribe();
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .expect("parent exists");
    let observed = wait_for_observed(&model, 2).await;

    // When: 親へ aside を配送した後、model gate を開く前に cancel する
    assert_eq!(
        runtime.send_agent_message(
            child,
            parent,
            AgentMessageKind::Send,
            "cancelled aside",
            None,
        ),
        Ok("msg-1".to_string())
    );
    assert_eq!(runtime.cancel(parent), Ok(()));
    let events = events_until_terminal(&mut receiver, parent).await;

    // Then: cancel が優先され Error で終端し、親の次 completion は発行されない
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Error));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCancelled { task_id })
            if task_id == &parent.to_string()
    )));
    assert_eq!(messages_for_marker(&observed, "PARENT").len(), 1);
    assert_eq!(runtime.cancel(child), Ok(()));
    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Error));
}
