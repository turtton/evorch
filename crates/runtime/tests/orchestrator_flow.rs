mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, Event, EventBus, EventKind, LifecycleEvent};
use providers::{ContentBlock, FinishReason};
use runtime::{AgentRuntime, RunConfig};
use serde_json::json;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response, tool_response, tool_responses};

fn runtime_with(model: Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(Arc::clone(&bus)));
    (AgentRuntime::new(Arc::clone(&bus), executor, model), bus)
}

async fn events_until_completed(
    receiver: &mut event_bus::EventReceiver,
    run_id: &str,
) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        let event = timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("event timeout")
            .expect("event receiver remains open");
        let completed = matches!(
            &event.kind,
            EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCompleted { task_id })
                if task_id == run_id
        );
        events.push(event);
        if completed {
            return events;
        }
    }
}

fn phases(events: &[Event], run_id: &str) -> Vec<AgentRunPhase> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id: event_run_id,
                to,
                ..
            }) if event_run_id == run_id => Some(*to),
            EventKind::Lifecycle(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_) => None,
        })
        .collect()
}

#[tokio::test]
async fn orchestrator_drives_children_entirely_through_meta_tool_uses() {
    // Given: run-1 の Orchestrator と、marker ごとに独立した子 run のモデルスクリプト
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "ORCH",
            [
                Ok(tool_responses([
                    (
                        "delegate-worker",
                        "delegate_background",
                        json!({ "role": "worker", "prompt": "W1" }),
                    ),
                    (
                        "delegate-explorer",
                        "delegate_background",
                        json!({ "role": "explorer", "prompt": "E1", "interactive": true }),
                    ),
                ])),
                Ok(tool_response(
                    "wait-worker",
                    "wait",
                    json!({ "run_id": "run-2" }),
                )),
                Ok(tool_response(
                    "message-explorer",
                    "send_message",
                    json!({ "run_id": "run-3", "message": "continue" }),
                )),
                Ok(tool_response(
                    "finish",
                    "finish",
                    json!({ "result": "all done" }),
                )),
            ],
        )
        .await;
    model
        .add_keyed("W1", [Ok(text_response("worker done", FinishReason::Stop))])
        .await;
    model
        .add_keyed(
            "E1",
            [
                Ok(text_response("need input", FinishReason::Stop)),
                Ok(text_response("explorer done", FinishReason::Stop)),
            ],
        )
        .await;
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let mut receiver = bus.subscribe();

    // When: モデルが meta-op ToolUse だけで子の生成・待機・再開・完了を指示する
    let orchestrator =
        runtime.delegate_background(Role::Orchestrator, "ORCH".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(orchestrator).await, Ok(AgentRunPhase::Done));
    let events = events_until_completed(&mut receiver, &orchestrator.to_string()).await;

    // Then: 親子すべての位相と background lifecycle が実 API の結果として観測できる
    assert_eq!(
        phases(&events, &orchestrator.to_string()),
        vec![
            AgentRunPhase::Pending,
            AgentRunPhase::Running,
            AgentRunPhase::Waiting,
            AgentRunPhase::Running,
            AgentRunPhase::Waiting,
            AgentRunPhase::Running,
            AgentRunPhase::Done,
        ]
    );
    assert_eq!(
        phases(&events, "run-2"),
        vec![
            AgentRunPhase::Pending,
            AgentRunPhase::Running,
            AgentRunPhase::Done,
        ]
    );
    assert_eq!(
        phases(&events, "run-3"),
        vec![
            AgentRunPhase::Pending,
            AgentRunPhase::Running,
            AgentRunPhase::Waiting,
            AgentRunPhase::Running,
            AgentRunPhase::Done,
        ]
    );
    for child in ["run-2", "run-3"] {
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Lifecycle(LifecycleEvent::BackgroundTaskStarted { task_id })
                if task_id == child
        )));
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCompleted { task_id })
                if task_id == child
        )));
    }
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Lifecycle(LifecycleEvent::BackgroundTaskCompleted { task_id })
            if task_id == &orchestrator.to_string()
    )));
    assert_eq!(
        runtime
            .inspect_agent(orchestrator)
            .expect("orchestrator run exists")
            .message_count,
        11
    );
    let observed = model.observed().await;
    let final_context = observed
        .iter()
        .rfind(|messages| {
            messages.first().is_some_and(|message| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Text { text } if text == "ORCH"))
            })
        })
        .expect("orchestrator context was observed");
    assert!(final_context.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolResult { tool_call_id, is_error: false, .. }
                    if tool_call_id == "message-explorer"
            )
        })
    }));
}

#[tokio::test]
async fn worker_cannot_spawn_child_through_meta_tool_use() {
    // Given: delegate_background を要求した Worker と、その後停止するモデルスクリプト
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "denied-delegate",
            "delegate_background",
            json!({ "role": "worker", "prompt": "CHILD" }),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let mut receiver = bus.subscribe();

    // When: Worker run を終端まで実行する
    let worker =
        runtime.delegate_background(Role::Worker, "WORKER".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(worker).await, Ok(AgentRunPhase::Done));
    let events = events_until_completed(&mut receiver, &worker.to_string()).await;

    // Then: CapabilityDenied の ToolResult が返り、子 run は登録も開始もされない
    assert_eq!(runtime.list_agents().len(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.kind,
                EventKind::Lifecycle(LifecycleEvent::BackgroundTaskStarted { .. })
            ))
            .count(),
        1
    );
    let observed = model.observed().await;
    let second_turn = observed.get(1).expect("worker second model turn");
    assert!(second_turn.iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::ToolResult {
                    tool_call_id,
                    is_error: true,
                    ..
                } if tool_call_id == "denied-delegate"
            )
        })
    }));
}
