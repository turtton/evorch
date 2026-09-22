mod support;

use std::sync::Arc;

use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent};
use providers::{ContentBlock, FinishReason};
use runtime::{AgentRuntime, Role, RunConfig, RunId, RuntimeError};
use serde_json::json;
use support::{ScriptedModel, text_response, tool_responses};
use tokio::sync::Notify;
use tools::ToolExecutor;

struct Fixture {
    runtime: AgentRuntime,
    model: Arc<ScriptedModel>,
    events: EventReceiver,
    gates: [Arc<Notify>; 2],
}

impl Fixture {
    async fn new(first_fails: bool) -> Self {
        let bus = Arc::new(EventBus::new(256));
        let events = bus.subscribe();
        let model = Arc::new(ScriptedModel::new([]));
        model
            .add_keyed(
                "ROOT",
                [
                    Ok(tool_responses([
                        ("first", "delegate", json!({"prompt": "FIRST"})),
                        (
                            "second",
                            "delegate",
                            json!({"prompt": "SECOND", "background": false}),
                        ),
                    ])),
                    Ok(text_response("done", FinishReason::Stop)),
                ],
            )
            .await;
        let gates = [Arc::new(Notify::new()), Arc::new(Notify::new())];
        for (index, marker) in ["FIRST", "SECOND"].into_iter().enumerate() {
            model.gate_key(marker, gates[index].clone()).await;
            let response = if index == 0 && first_fails {
                Err(RuntimeError::Model {
                    reason: "child failed".into(),
                })
            } else {
                Ok(text_response(marker, FinishReason::Stop))
            };
            model.add_keyed(marker, [response]).await;
        }
        let runtime =
            AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone());
        Self {
            runtime,
            model,
            events,
            gates,
        }
    }

    async fn start_both(&mut self) -> RunId {
        let parent = self.runtime.delegate_background(
            Role::Orchestrator,
            "ROOT".into(),
            RunConfig::default(),
        );
        let mut started = Vec::new();
        while started.len() < 2 {
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                run_id,
                parent_run_id,
                ..
            }) = self.events.recv().await.expect("start event").kind
                && parent_run_id.as_deref() == Some(parent.to_string().as_str())
            {
                eprintln!("awaited child started: {run_id}");
                started.push(run_id);
            }
        }
        assert_eq!(started, ["run-2", "run-3"]);
        parent
    }

    async fn assert_results(&self, first: &str) {
        let observed = self.model.observed().await;
        let results: Vec<_> = observed
            .last()
            .expect("parent next turn")
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } => content.first().map(|item| match item {
                    providers::ToolResultContent::Text { text } => {
                        (tool_call_id.as_str(), text.as_str(), *is_error)
                    }
                }),
                _ => None,
            })
            .collect();
        assert_eq!(
            results,
            [("first", first, false), ("second", "Done", false)]
        );
    }
}

#[tokio::test]
async fn awaited_delegates_overlap_and_results_keep_call_order() {
    // Given: neither child can complete until the test releases its gate.
    let mut fixture = Fixture::new(false).await;
    // When: both start in one batch, then the second completes first.
    let parent = fixture.start_both().await;
    fixture.gates[1].notify_one();
    assert_eq!(
        fixture.runtime.wait(RunId::new(3)).await,
        Ok(AgentRunPhase::Done)
    );
    fixture.gates[0].notify_one();
    assert_eq!(fixture.runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    // Then: completion order does not change result order.
    fixture.assert_results("Done").await;
}

#[tokio::test]
async fn failed_awaited_child_does_not_prevent_sibling_result() {
    // Given: the first child fails after both have started.
    let mut fixture = Fixture::new(true).await;
    // When: release the failure before allowing the sibling to complete.
    let parent = fixture.start_both().await;
    fixture.gates[0].notify_one();
    assert_eq!(
        fixture.runtime.wait(RunId::new(2)).await,
        Ok(AgentRunPhase::Error)
    );
    fixture.gates[1].notify_one();
    assert_eq!(fixture.runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    // Then: the existing phase-result contract is preserved independently.
    fixture.assert_results("Error").await;
}

#[tokio::test]
async fn parent_cancellation_reaches_all_awaited_children() {
    // Given: both awaited children are blocked on their gates.
    let mut fixture = Fixture::new(false).await;
    let parent = fixture.start_both().await;
    // When: cancel the parent without releasing either child.
    fixture.runtime.cancel(parent).expect("cancel parent");
    // Then: the entire family terminates without a gate release.
    for run in [parent, RunId::new(2), RunId::new(3)] {
        assert_eq!(fixture.runtime.wait(run).await, Ok(AgentRunPhase::Error));
    }
}

#[tokio::test]
async fn cancelled_awaited_child_does_not_prevent_sibling_result() {
    // Given: two children have started and neither has completed.
    let mut fixture = Fixture::new(false).await;
    let parent = fixture.start_both().await;
    // When: cancel only the first, and let the sibling complete normally.
    fixture.runtime.cancel(RunId::new(2)).expect("cancel child");
    assert_eq!(
        fixture.runtime.wait(RunId::new(2)).await,
        Ok(AgentRunPhase::Error)
    );
    fixture.gates[1].notify_one();
    assert_eq!(fixture.runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    // Then: cancellation retains the existing per-call phase result.
    fixture.assert_results("Error").await;
}

#[tokio::test]
async fn non_delegate_meta_op_remains_a_wave_barrier() {
    // Given: a list_agents effect separates an overlapping pair from a tail delegate.
    let mut fixture = Fixture::new(false).await;
    fixture
        .model
        .add_keyed(
            "ROOT",
            [
                Ok(tool_responses([
                    ("first", "delegate", json!({"prompt": "FIRST"})),
                    ("second", "delegate", json!({"prompt": "SECOND"})),
                    ("barrier", "list_agents", json!({})),
                    ("tail", "delegate", json!({"prompt": "TAIL"})),
                ])),
                Ok(text_response("done", FinishReason::Stop)),
            ],
        )
        .await;
    fixture
        .model
        .add_keyed("TAIL", [Ok(text_response("tail", FinishReason::Stop))])
        .await;
    // When: both children are blocked, then released in reverse order.
    let parent = fixture.start_both().await;
    assert_eq!(fixture.runtime.list_agents().len(), 3);
    fixture.gates[1].notify_one();
    assert_eq!(
        fixture.runtime.wait(RunId::new(3)).await,
        Ok(AgentRunPhase::Done)
    );
    fixture.gates[0].notify_one();
    assert_eq!(fixture.runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    // Then: the barrier saw the completed pair, but not the tail child.
    let observed = fixture.model.observed().await;
    let results: Vec<_> = observed
        .last()
        .expect("parent turn")
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                ..
            } => Some((tool_call_id.as_str(), content)),
            _ => None,
        })
        .collect();
    assert_eq!(
        results.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        ["first", "second", "barrier", "tail"]
    );
    let providers::ToolResultContent::Text { text } = &results[2].1[0];
    let agents: serde_json::Value = serde_json::from_str(text).expect("agent list");
    let agents = agents.as_array().expect("array");
    assert_eq!(agents.len(), 3);
    for child in &agents[1..] {
        assert_eq!(child["phase"], "Done");
    }
}
