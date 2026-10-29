//! Switching Web tools on a live runtime (the GUI's Agent features settings)
//! keeps the tool definitions a run already sent and its wire prefix; only runs
//! started afterwards see the new setting.

use std::sync::Arc;

use event_bus::AgentRunPhase;
use mock_openai::ScriptedResponse;
use runtime::{Role, RunConfig};
use serde_json::{Value, json};
use tools::{SearchError, SearchOptions, SearchProvider, SearchResults, Tool, WebSearch};

use super::{
    BulkRead, Harness, MODEL, Wire, harness_with_model, read_response, text_response,
    through_phase, verify_trace_of,
};

struct CannedSearch;

#[async_trait::async_trait]
impl SearchProvider for CannedSearch {
    fn name(&self) -> &str {
        "canned"
    }

    async fn search(&self, _: &str, _: &SearchOptions) -> Result<SearchResults, SearchError> {
        Ok(SearchResults {
            content: "1. evorch — https://example.invalid/evorch".into(),
            result_count: 1,
            request_id: None,
            usage: None,
        })
    }
}

fn harness(enabled: bool, script: Vec<ScriptedResponse>) -> Harness {
    harness_with_model(
        Wire::ChatCompletions,
        MODEL,
        script,
        1_000_000,
        Arc::new(BulkRead),
        vec![Arc::new(WebSearch::for_providers(
            Arc::new(CannedSearch),
            Arc::new(CannedSearch),
        )) as Arc<dyn Tool>],
        None,
        0.5,
        |config| {
            config.sandbox.web_tools_enabled = enabled;
            // The harness routes the worker only; the Web researcher shares its route.
            config.agents.roles.web_researcher.models =
                vec![config::ChainEntryConfig::route("worker")];
        },
    )
}

fn search_response(index: usize) -> ScriptedResponse {
    ScriptedResponse::tool_call(
        &format!("response-{index}"),
        MODEL,
        0,
        &format!("call-{index}"),
        "web_search",
        [json!({"query": "evorch"}).to_string()],
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

fn offers_search(request: &Value) -> bool {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "web_search")
}

fn tool_result<'a>(request: &'a Value, call_id: &str) -> &'a str {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == call_id)
        .unwrap_or_else(|| panic!("result of {call_id} in the history"))["content"]
        .as_str()
        .unwrap()
}

struct ReleaseOnDrop(mock_openai::ResponseGate);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// Starts a Web researcher run with Web tools `enabled`, switches them while the
/// mock holds the first response, verifies the run's wire trace, then starts one
/// more run. Returns the requests of both runs.
async fn switch_web_tools_during_a_run(
    enabled: bool,
    mut script: Vec<ScriptedResponse>,
    next_run: Vec<ScriptedResponse>,
) -> (Vec<Value>, Vec<Value>) {
    let (gate, arrived) = mock_openai::ResponseGate::new();
    script[0] = script[0].clone().with_gate(gate.clone());
    script.extend(next_run);
    let mut harness = harness(enabled, script);
    let _release = ReleaseOnDrop(gate.clone());

    let run = harness.runtime.delegate_background(
        Role::WebResearcher,
        "research".into(),
        RunConfig::default(),
    );
    tokio::task::spawn_blocking(move || arrived.recv().unwrap())
        .await
        .unwrap();
    harness.runtime.set_web_tools_enabled(!enabled);
    gate.release();
    let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
    let switched = requests(&harness);
    verify_trace_of(&harness, run, &events, 0, &switched);

    let next = harness.runtime.delegate_background(
        Role::WebResearcher,
        "next".into(),
        RunConfig::default(),
    );
    through_phase(&mut harness.receiver, next, AgentRunPhase::Done).await;
    assert_eq!(harness.mock.remaining_scripts(), 0);
    let next_run = requests(&harness).split_off(switched.len());
    (switched, next_run)
}

fn assert_same_tools(requests: &[Value]) {
    let first = serde_json::to_string(&requests[0]["tools"]).unwrap();
    for request in &requests[1..] {
        assert_eq!(serde_json::to_string(&request["tools"]).unwrap(), first);
    }
}

#[tokio::test]
async fn switching_web_tools_off_during_a_run_keeps_its_tools_and_wire_prefix() {
    let (run, next_run) = switch_web_tools_during_a_run(
        true,
        vec![
            search_response(0),
            search_response(1),
            text_response("done"),
        ],
        vec![text_response("next")],
    )
    .await;

    // The run keeps offering web_search, but calls after the switch are refused.
    assert_eq!(run.len(), 3);
    assert_same_tools(&run);
    assert!(run.iter().all(offers_search));
    for (request, call) in [(&run[1], "call-0"), (&run[2], "call-1")] {
        let refused = tool_result(request, call);
        assert!(refused.contains("Web tools are disabled"), "{refused}");
    }
    // The next run is started without it.
    assert!(!offers_search(&next_run[0]));
}

#[tokio::test]
async fn switching_web_tools_on_during_a_run_keeps_its_tools_and_wire_prefix() {
    let (run, next_run) = switch_web_tools_during_a_run(
        false,
        vec![read_response(0), read_response(1), text_response("done")],
        vec![text_response("next")],
    )
    .await;

    assert_eq!(run.len(), 3);
    assert_same_tools(&run);
    assert!(!run.iter().any(offers_search));
    assert!(offers_search(&next_run[0]));
}
