//! run 登録時に発火する canonical 開始イベント `AgentRunStarted` の契約。
//!
//! `spawn_run` は既存の `AgentRunStateChanged` (registered) /
//! `BackgroundTaskStarted` と併存しつつ、run_id / parent_run_id / agent_name /
//! role 語彙を載せた `AgentRunStarted` を発火する。

mod support;

use std::sync::Arc;
use std::time::Duration;

use agents::Role;
use event_bus::{AgentRunPhase, Event, EventBus, EventKind, LifecycleEvent};
use providers::FinishReason;
use runtime::{AgentModel, AgentRuntime, RunConfig};
use sandbox::DirectSandbox;
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

fn runtime_with(bus: &Arc<EventBus>, model: Arc<dyn AgentModel>) -> AgentRuntime {
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    AgentRuntime::new(Arc::clone(bus), executor, model)
}

async fn drain_events(receiver: &mut event_bus::EventReceiver) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(event) = tokio::time::timeout(Duration::from_millis(200), receiver.recv()).await {
        match event {
            Ok(event) => events.push(event),
            Err(_) => break,
        }
    }
    events
}

fn agent_run_started(events: Vec<Event>) -> Vec<(String, Option<String>, String, String)> {
    events
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                run_id,
                parent_run_id,
                agent_name,
                role,
            }) => Some((run_id, parent_run_id, agent_name, role)),
            _ => None,
        })
        .collect()
}

// Given: EventBus を購読済みのランタイムと表示名つき config
// When: delegate_background で run を登録し完了まで待つ
// Then: AgentRunStarted が run_id / parent なし / agent_name / role 語彙を載せて 1 回だけ発火する
#[tokio::test]
async fn spawn_run_emits_agent_run_started_with_registration_details() {
    let bus = Arc::new(EventBus::new(64));
    let runtime = runtime_with(
        &bus,
        Arc::new(ScriptedModel::new([Ok(text_response(
            "done",
            FinishReason::Stop,
        ))])),
    );
    let mut events = bus.subscribe();

    let run_id = runtime.delegate_background(
        Role::Worker,
        "task".to_string(),
        RunConfig {
            name: Some("worker-w1".to_string()),
            ..RunConfig::default()
        },
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let started = agent_run_started(drain_events(&mut events).await);

    assert_eq!(
        started,
        vec![(
            run_id.to_string(),
            None,
            "worker-w1".to_string(),
            "worker".to_string()
        )]
    );
}

// Given: 親 run と子 run (表示名つき) を同じランタイムへ登録
// When: 両 run が完了する
// Then: 子 run の AgentRunStarted.parent_run_id は親 run の ID で、親 run 自身は None
#[tokio::test]
async fn delegated_child_run_records_parent_run_id_in_agent_run_started() {
    let bus = Arc::new(EventBus::new(64));
    let model = ScriptedModel::new([]);
    model
        .add_keyed(
            "parent-prompt",
            [Ok(text_response("parent done", FinishReason::Stop))],
        )
        .await;
    model
        .add_keyed(
            "child-prompt",
            [Ok(text_response("child done", FinishReason::Stop))],
        )
        .await;
    let runtime = runtime_with(&bus, Arc::new(model));
    let mut events = bus.subscribe();

    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "parent-prompt".to_string(),
        RunConfig::default(),
    );
    let child = runtime
        .delegate_background_as_child(
            parent,
            Role::Explorer,
            "child-prompt".to_string(),
            RunConfig {
                name: Some("child-agent".to_string()),
                ..RunConfig::default()
            },
        )
        .expect("親 run が存在する");
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Done));

    let started = agent_run_started(drain_events(&mut events).await);

    assert_eq!(started.len(), 2);
    assert!(started.contains(&(
        parent.to_string(),
        None,
        "Orchestrator".to_string(),
        "orchestrator".to_string()
    )));
    assert!(started.contains(&(
        child.to_string(),
        Some(parent.to_string()),
        "child-agent".to_string(),
        "explorer".to_string()
    )));
}
