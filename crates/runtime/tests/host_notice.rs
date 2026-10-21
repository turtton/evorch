mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent};
use providers::{ContentBlock, FinishReason};
use runtime::{AgentRuntime, RunConfig, RunId, RuntimeError};
use sandbox::DirectSandbox;
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

async fn reached(events: &mut EventReceiver, run: RunId, phase: AgentRunPhase) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.expect("event bus open");
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
                event.kind
                && run_id == run.to_string()
                && to == phase
            {
                return;
            }
        }
    })
    .await
    .expect("run reaches the phase");
}

#[tokio::test]
async fn notices_skip_runs_that_will_not_read_them() {
    // Given: an interactive root waiting for input and its child, which has ended.
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "consult",
            (0..4).map(|_| Ok(text_response("waiting", FinishReason::Stop))),
        )
        .await;
    model
        .add_keyed(
            "evaluate",
            [Ok(text_response("started", FinishReason::Stop))],
        )
        .await;
    let bus = Arc::new(EventBus::new(256));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model.clone());
    let mut events = bus.subscribe();
    let root = runtime.delegate_background(
        Role::Orchestrator,
        "consult".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..RunConfig::default()
        },
    );
    reached(&mut events, root, AgentRunPhase::Waiting).await;
    let child = runtime
        .delegate_background_as_child(root, Role::Evaluator, "evaluate", RunConfig::default())
        .unwrap();
    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Done));
    model
        .add_keyed("evaluate", [Ok(text_response("alone", FinishReason::Stop))])
        .await;
    let lone = runtime.delegate_background(Role::Worker, "evaluate".into(), RunConfig::default());
    assert_eq!(runtime.wait(lone).await, Ok(AgentRunPhase::Done));

    // When: a notice is delivered to the ended child and to the lone ended run.
    let delivered = runtime.deliver_notice(child, "Evaluation group g finished");
    let undelivered = runtime.deliver_notice(lone, "Evaluation group h finished");

    // Then: the root reads the child's notice on its next request; the lone run has no reader.
    assert_eq!(delivered, Ok(root));
    assert!(matches!(
        undelivered,
        Err(RuntimeError::RunTerminated { .. })
    ));
    let notice = |block: &ContentBlock| matches!(block, ContentBlock::Text { text } if text == "Evaluation group g finished");
    let read = async {
        let mut index = 0;
        while !model
            .wait_for_request(index)
            .await
            .last()
            .is_some_and(|message| message.content.iter().any(notice))
        {
            index += 1;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), read)
        .await
        .expect("the root reads the notice");
}
