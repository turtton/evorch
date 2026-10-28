//! The real `read` tool's path aliases end to end through the runtime's prepared-call
//! path, HTTP/SSE and usage. Aliases are folded into `path` only for execution: the
//! history keeps the model's original arguments, the tool specs stay fixed, and every
//! request extends the previous wire input.

use std::sync::Arc;

use event_bus::AgentRunPhase;
use mock_openai::ScriptedResponse;
use runtime::{Role, RunConfig};
use serde_json::{Value, json};
use tools::Tool;

use super::{Harness, MODEL, Wire, harness_with_wire, text_response, through_phase, verify_trace};

const FIXTURE_TEXT: &str = "alias fixture text";

fn read_call(index: usize, arguments: &Value) -> ScriptedResponse {
    ScriptedResponse::tool_call(
        &format!("response-{index}"),
        MODEL,
        0,
        &format!("call-{index}"),
        "read",
        [arguments.to_string()],
    )
}

fn wire_requests(harness: &Harness) -> Vec<Value> {
    harness
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == harness.wire.path())
        .map(|request| request.body)
        .collect()
}

/// The replayed history in the wire's own shape.
fn history(wire: Wire, request: &Value) -> &Vec<Value> {
    match wire {
        Wire::ChatCompletions => request["messages"].as_array().unwrap(),
        Wire::Responses | Wire::Codex => request["input"].as_array().unwrap(),
    }
}

/// The arguments the model sent for `call_id` and the output returned to it.
fn replayed_call(wire: Wire, request: &Value, call_id: &str) -> (Value, String) {
    let history = history(wire, request);
    let (arguments, output) =
        match wire {
            Wire::ChatCompletions => {
                (
                    history
                        .iter()
                        .find_map(|message| {
                            message["tool_calls"]
                                .as_array()?
                                .iter()
                                .find(|call| call["id"] == call_id)
                        })
                        .unwrap_or_else(|| panic!("no {call_id} call in {history:?}"))["function"]
                        ["arguments"]
                        .clone(),
                    history
                        .iter()
                        .find(|message| {
                            message["role"] == "tool" && message["tool_call_id"] == call_id
                        })
                        .unwrap()["content"]
                        .clone(),
                )
            }
            Wire::Responses | Wire::Codex => (
                history
                    .iter()
                    .find(|item| item["type"] == "function_call" && item["call_id"] == call_id)
                    .unwrap_or_else(|| panic!("no {call_id} call in {history:?}"))["arguments"]
                    .clone(),
                history
                    .iter()
                    .find(|item| {
                        item["type"] == "function_call_output" && item["call_id"] == call_id
                    })
                    .unwrap()["output"]
                    .clone(),
            ),
        };
    (
        serde_json::from_str(arguments.as_str().unwrap()).unwrap(),
        output.as_str().unwrap().to_owned(),
    )
}

/// Given: the real `read` tool and a model that calls it with an alias, with two
///        different paths, again correctly, and with a repeated path and an empty alias
/// When: the Worker runs to completion
/// Then: the alias call reads the file, the conflict returns an argument error naming
///       both values, and the later calls succeed; the original arguments stay in the
///       replayed history, the tool specs never change, and each request extends the
///       previous wire input
async fn aliases_keep_the_wire_prefix(wire: Wire) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let note = root.join("note.txt");
    std::fs::write(&note, FIXTURE_TEXT).unwrap();
    let vault = root.join("vault");
    std::fs::create_dir(&vault).unwrap();
    let calls = [
        json!({"file": note}),
        json!({"path": note, "target": vault}),
        json!({"path": note}),
        json!({"path": note, "file": note, "filename": ""}),
    ];
    let script = calls
        .iter()
        .enumerate()
        .map(|(index, arguments)| read_call(index, arguments))
        .chain([text_response("done")])
        .collect();
    let mut harness = harness_with_wire(
        wire,
        script,
        1_000_000,
        Arc::new(tools::Read),
        Vec::new(),
        None,
        0.5,
        |_| {},
    );
    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Read the note".into(),
        RunConfig::default(),
    );
    let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    // The input-derived mock cache and the contract's append-only assertion.
    verify_trace(&harness, run, &events, 0);

    // Independent of both: tools never change and each history extends the previous one.
    let requests = wire_requests(&harness);
    assert_eq!(requests.len(), calls.len() + 1);
    let read_spec = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| match wire {
            Wire::ChatCompletions => tool["function"]["name"] == "read",
            Wire::Responses | Wire::Codex => tool["name"] == "read",
        })
        .unwrap();
    let parameters = match wire {
        Wire::ChatCompletions => &read_spec["function"]["parameters"],
        Wire::Responses | Wire::Codex => &read_spec["parameters"],
    };
    assert_eq!(*parameters, tools::Read.schema());
    for alias in ["file", "file_path", "filename", "target"] {
        assert!(parameters["properties"].get(alias).is_none(), "{alias}");
    }
    for pair in requests.windows(2) {
        assert_eq!(pair[0]["tools"], pair[1]["tools"]);
        let (before, after) = (history(wire, &pair[0]), history(wire, &pair[1]));
        assert!(after.len() > before.len());
        assert_eq!(before[..], after[..before.len()]);
    }

    // The model's own arguments are replayed as sent; only the results differ.
    let last = requests.last().unwrap();
    let results: Vec<_> = (0..calls.len())
        .map(|index| {
            let (arguments, output) = replayed_call(wire, last, &format!("call-{index}"));
            assert_eq!(arguments, calls[index], "call-{index}");
            output
        })
        .collect();
    for index in [0, 2, 3] {
        assert!(results[index].contains(FIXTURE_TEXT), "{}", results[index]);
    }
    assert!(!results[1].contains(FIXTURE_TEXT), "{}", results[1]);
    assert!(results[1].contains(&format!("target=\"{}\"", vault.display())));
    assert!(results[1].contains(&format!("path=\"{}\"", note.display())));
}

#[tokio::test]
async fn read_aliases_and_conflicts_preserve_tools_history_and_the_wire_prefix() {
    aliases_keep_the_wire_prefix(Wire::ChatCompletions).await;
}

#[tokio::test]
async fn openai_responses_read_aliases_and_conflicts_preserve_tools_history_and_the_wire_prefix() {
    aliases_keep_the_wire_prefix(Wire::Responses).await;
}
