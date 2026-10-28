//! `run_js` and the prompt cache: its description is generated once per run, so
//! every request of a run carries byte-identical tool definitions and the cached
//! prefix is reused (AGENTS.md "Cache correctness gate").

use std::sync::Arc;

use event_bus::AgentRunPhase;
use runtime::{Role, RunConfig};
use serde_json::Value;
use tools::Tool;

use super::{BulkRead, Harness, MODEL, Wire, harness_with_model, read_response, text_response};

fn harness(enabled: bool) -> Harness {
    harness_with_model(
        Wire::ChatCompletions,
        MODEL,
        vec![read_response(1), read_response(3), text_response("done")],
        1_000_000,
        Arc::new(BulkRead),
        vec![
            Arc::new(tools::Write) as Arc<dyn Tool>,
            Arc::new(tools::RunJs::new()),
        ],
        None,
        0.5,
        |config| config.tools.run_js.enabled = enabled,
    )
}

fn requests(harness: &Harness) -> Vec<Value> {
    harness
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == harness.wire.path())
        .map(|request| request.body)
        .collect()
}

fn tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn run_js_definition_is_byte_identical_across_the_turns_of_a_run() {
    let harness = harness(true);
    let run =
        harness
            .runtime
            .delegate_background(Role::Worker, "work".into(), RunConfig::default());
    assert_eq!(harness.runtime.wait(run).await, Ok(AgentRunPhase::Done));

    let requests = requests(&harness);
    assert_eq!(requests.len(), 3);
    let first = serde_json::to_string(&requests[0]["tools"]).unwrap();
    for request in &requests[1..] {
        assert_eq!(serde_json::to_string(&request["tools"]).unwrap(), first);
    }
    let run_js = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["function"]["name"] == "run_js")
        .expect("run_js is offered when enabled");
    let description = run_js["function"]["description"].as_str().unwrap();
    assert!(
        description.contains("read(args: { index: number })"),
        "declarations come from this run's tools: {description}"
    );
    assert!(description.contains("write(args: { content: string; path: string })"));
}

#[tokio::test]
async fn disabled_run_js_leaves_the_tool_definitions_unchanged() {
    let harness = harness(false);
    let run =
        harness
            .runtime
            .delegate_background(Role::Worker, "work".into(), RunConfig::default());
    assert_eq!(harness.runtime.wait(run).await, Ok(AgentRunPhase::Done));

    let requests = requests(&harness);
    let names = tool_names(&requests[0]);
    assert!(!names.contains(&"run_js".to_owned()), "{names:?}");
    assert!(
        !requests[0]["tools"].to_string().contains("run_js"),
        "no definition mentions run_js while it is off"
    );
}
