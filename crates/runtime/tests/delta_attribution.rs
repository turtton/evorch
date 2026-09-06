mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind, MessageEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, RunConfig};
use sandbox::DirectSandbox;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, drain_events, text_response};

fn runtime_with(model: Arc<ScriptedModel>) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    (AgentRuntime::new(Arc::clone(&bus), executor, model), bus)
}

#[tokio::test]
async fn concurrent_runs_attribute_message_deltas_to_their_own_run_id() {
    // Given: two keyed responses blocked on separate gates on one event bus.
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "ALPHA-",
            [Ok(text_response("alpha text", FinishReason::Stop))],
        )
        .await;
    model
        .add_keyed(
            "BETA-",
            [Ok(text_response("beta text", FinishReason::Stop))],
        )
        .await;
    let gate_a = Arc::new(Notify::new());
    let gate_b = Arc::new(Notify::new());
    model.gate_key("ALPHA-", Arc::clone(&gate_a)).await;
    model.gate_key("BETA-", Arc::clone(&gate_b)).await;
    let (runtime, bus) = runtime_with(Arc::clone(&model));
    let mut rx = bus.subscribe();

    // When: both model calls overlap, then release B before A.
    let a = runtime.delegate_background(Role::Worker, "ALPHA-run".into(), RunConfig::default());
    let b = runtime.delegate_background(Role::Worker, "BETA-run".into(), RunConfig::default());
    timeout(Duration::from_secs(2), async {
        // On this current-thread runtime, each model call yields at its gate.
        while model.observed().await.len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both runs entered the model before either gate was released");
    let agents = runtime.list_agents();
    for run_id in [a, b] {
        assert_eq!(
            agents
                .iter()
                .find(|agent| agent.run_id == run_id)
                .map(|agent| agent.phase),
            Some(AgentRunPhase::Running)
        );
    }
    gate_b.notify_one();
    gate_a.notify_one();
    timeout(Duration::from_secs(2), async {
        assert_eq!(runtime.wait(a).await, Ok(AgentRunPhase::Done));
        assert_eq!(runtime.wait(b).await, Ok(AgentRunPhase::Done));
    })
    .await
    .expect("both released runs finish");
    let events = drain_events(&mut rx).await;

    // Then: every message delta is attributed to its own run.
    let message_deltas = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Message(MessageEvent::MessageDelta { delta, run_id }) => {
                Some((delta.as_str(), run_id.clone()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        message_deltas.contains(&("alpha text", Some(a.to_string()))),
        "alpha attribution missing: {message_deltas:?}"
    );
    assert!(message_deltas.contains(&("beta text", Some(b.to_string()))));
    assert!(message_deltas.iter().all(|(_, run_id)| run_id.is_some()));
}
