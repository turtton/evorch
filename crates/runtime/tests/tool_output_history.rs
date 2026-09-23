mod support;

use std::sync::Arc;
use std::time::Duration;

use event_bus::{AgentRunPhase, EventBus, EventKind, ToolEvent};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::{AgentRuntime, Role, RunConfig};
use serde_json::{Value, json};
use support::{ScriptedModel, drain_events, text_response, tool_response};
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};

struct LargeOutput;

fn original_output(index: u64) -> String {
    format!("output-{index}-").repeat(if index == 1 { 3000 } else { 200 })
}

#[async_trait::async_trait]
impl Tool for LargeOutput {
    fn name(&self) -> &'static str {
        "read"
    }

    fn schema(&self) -> Value {
        json!({"type": "object", "required": ["index"], "properties": {
            "index": {"type": "integer"}
        }})
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }

    async fn execute(&self, args: Value) -> Result<ToolResult, ToolError> {
        let index = args["index"].as_u64().expect("index");
        let content = original_output(index);
        Ok(if index == 0 {
            ToolResult::error(content)
        } else {
            ToolResult::success(content)
        })
    }
}

#[tokio::test]
async fn returned_tool_outputs_remain_an_unchanged_model_prefix_after_ten_results() {
    // Given: ten bulky results, including an error and an output requiring an artifact.
    // Both the old eight-result retention boundary and its two pruning paths are crossed.
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(LargeOutput)).unwrap();
    let script = (0..10)
        .map(|index| {
            Ok(tool_response(
                &format!("call-{index}"),
                "read",
                json!({"index": index}),
            ))
        })
        .chain([Ok(text_response("done", FinishReason::Stop))]);
    let model = Arc::new(ScriptedModel::new(script));
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone()).with_compaction(
        config::CompactionConfig {
            context_window_tokens: 1_000_000,
            ..Default::default()
        },
    );

    // When: the real executor and agent loop return all results over successive turns.
    let run = runtime.delegate_background(
        Role::Worker,
        "Inspect ten outputs".into(),
        RunConfig::default(),
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), runtime.wait(run))
            .await
            .unwrap()
            .unwrap(),
        AgentRunPhase::Done
    );

    // Then: every request retains all previously submitted messages byte for byte.
    let observed = model.observed().await;
    assert_eq!(observed.len(), 11);
    for (turn, requests) in observed.windows(2).enumerate() {
        assert!(
            requests[1].starts_with(&requests[0]),
            "model request {} rewrote a previously sent message",
            turn + 1
        );
    }
    let final_results: Vec<_> = observed
        .last()
        .unwrap()
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } => Some((tool_call_id, content, is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(final_results.len(), 10);
    let completed: Vec<_> = drain_events(&mut events)
        .await
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Tool(ToolEvent::ToolCompleted {
                call_id,
                output: Some(output),
                is_error,
                detail,
                ..
            }) => Some((call_id, output, is_error, detail)),
            _ => None,
        })
        .collect();
    assert_eq!(completed.len(), 10);
    for (index, ((call_id, content, is_error), (event_id, output, event_error, detail))) in
        final_results.iter().zip(&completed).enumerate()
    {
        assert_eq!(**call_id, format!("call-{index}"));
        assert_eq!(*call_id, event_id);
        assert_eq!(**is_error, index == 0);
        assert_eq!(*is_error, event_error);
        assert_eq!(
            content.as_slice(),
            [ToolResultContent::Text {
                text: output.clone()
            }]
        );
        assert!(output.len() > 1024 && output.len() < 20 * 1024);
        if index == 1 {
            // The output was limited before its first event/model delivery, and the
            // complete body remains retrievable through the original artifact path.
            let path = detail.as_ref().unwrap()["output_artifact"]["path"]
                .as_str()
                .unwrap();
            assert!(output.contains(path));
            assert_eq!(std::fs::read_to_string(path).unwrap(), original_output(1));
            assert!(
                detail.as_ref().unwrap()["output_artifact"]["complete"]
                    .as_bool()
                    .unwrap()
            );
        } else {
            assert_eq!(*output, original_output(index as u64));
        }
    }
}
