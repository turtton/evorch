//! apply_patch end to end through routing, HTTP/SSE and usage: a model selected for
//! apply_patch sees it as its only editing tool, calls it freeform (Responses, Codex)
//! or as a JSON function (Chat Completions), and the calls and outputs stay in the
//! reused wire prefix, across compaction and edit-tool switch boundaries too.

mod chat_switch;
mod checkpoint_guard;
mod codex_reconfigure;
mod codex_switch;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use apply_patch::tool_contract::{LARK_GRAMMAR, NAME, json_schema};
use config::EditToolConfig;
use event_bus::{AgentRunPhase, CompactionEvent, CompactionReason, EventKind};
use mock_openai::ScriptedResponse;
use runtime::{Role, RunConfig};
use serde_json::{Value, json};
use tools::Tool;

use super::{BulkRead, Harness, MODEL, Wire, harness_with_model, through_phase, verify_trace};

const GPT5: &str = "gpt-5.5";
/// One patch split across stream deltas, including inside a hunk.
const PATCH: [&str; 4] = [
    "*** Begin Patch\n*** Add File: added.txt\n",
    "+hello\n*** Update File: notes.txt\n",
    "@@\n-beta\n+gam",
    "ma\n*** End Patch",
];
const PATCHED: &str = "Success. Updated the following files:\nA added.txt\nM notes.txt\n";
const FOLLOW_UP: [&str; 2] = [
    "*** Begin Patch\n*** Update File: notes.txt\n",
    "@@\n-gamma\n+delta\n*** End Patch",
];
const FOLLOWED_UP: &str = "Success. Updated the following files:\nM notes.txt\n";

/// A workspace whose `notes.txt` [`PATCH`] updates.
fn workspace() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    std::fs::write(root.join("notes.txt"), "alpha\nbeta\n").unwrap();
    (directory, root)
}

/// Every editing tool is registered and authorized; only the model's selection narrows
/// what each request shows.
fn editing_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(tools::Edit),
        Arc::new(tools::Write),
        Arc::new(tools::ApplyPatch::new()),
    ]
}

/// GPT-5 selects apply_patch by default on Responses wires; the Chat profile's
/// `mock-model` selects it through an explicit model entry.
const fn served_model(wire: Wire) -> &'static str {
    match wire {
        Wire::ChatCompletions => MODEL,
        Wire::Responses | Wire::Codex => GPT5,
    }
}

fn patch_harness(wire: Wire, script: Vec<ScriptedResponse>, root: &Path) -> Harness {
    let harness = harness_with_model(
        wire,
        served_model(wire),
        script,
        1_000_000,
        Arc::new(BulkRead),
        editing_tools(),
        None,
        0.5,
        |config| {
            if matches!(wire, Wire::ChatCompletions) {
                config.providers.get_mut("local").unwrap().models[0].edit_tool =
                    Some(EditToolConfig::ApplyPatch);
            }
        },
    );
    harness
        .runtime
        .set_project_root(root.to_path_buf())
        .unwrap();
    harness
}

fn patch_call(wire: Wire, call_id: &str, patch: &[&str]) -> ScriptedResponse {
    ScriptedResponse::custom_tool_call(
        &format!("response-{call_id}"),
        served_model(wire),
        0,
        call_id,
        NAME,
        patch,
    )
}

fn read_call(model: &str, index: usize) -> ScriptedResponse {
    ScriptedResponse::tool_call(
        &format!("response-read-{index}"),
        model,
        0,
        &format!("read-{index}"),
        "read",
        [json!({"index": index}).to_string()],
    )
}

fn text(model: &str, text: &str) -> ScriptedResponse {
    ScriptedResponse::text_stream("reply", model, [text])
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

fn tool_name(wire: Wire, tool: &Value) -> &str {
    let name = match wire {
        Wire::ChatCompletions => &tool["function"]["name"],
        Wire::Responses | Wire::Codex => &tool["name"],
    };
    name.as_str().unwrap()
}

/// apply_patch is the only editing tool, in the wire's own shape.
fn assert_only_apply_patch(wire: Wire, request: &Value) {
    let tools = request["tools"].as_array().unwrap();
    let names: Vec<_> = tools.iter().map(|tool| tool_name(wire, tool)).collect();
    for editing in ["edit", "write"] {
        assert!(!names.contains(&editing), "{names:?}");
    }
    let patch: Vec<_> = tools
        .iter()
        .filter(|tool| tool_name(wire, tool) == NAME)
        .collect();
    assert_eq!(patch.len(), 1, "{names:?}");
    match wire {
        Wire::ChatCompletions => {
            assert_eq!(patch[0]["type"], "function");
            assert_eq!(patch[0]["function"]["parameters"], json_schema());
        }
        Wire::Responses | Wire::Codex => {
            assert_eq!(patch[0]["type"], "custom");
            assert_eq!(
                patch[0]["format"],
                json!({"type": "grammar", "syntax": "lark", "definition": LARK_GRAMMAR})
            );
        }
    }
}

/// The call and its output, adjacent in the history as the wire replays them.
fn assert_replayed_call(wire: Wire, request: &Value, call_id: &str, patch: &str, output: &str) {
    match wire {
        Wire::ChatCompletions => {
            let messages = request["messages"].as_array().unwrap();
            let call = messages
                .iter()
                .position(|message| message["tool_calls"][0]["id"] == call_id)
                .unwrap_or_else(|| panic!("no {call_id} call in {messages:?}"));
            let function = &messages[call]["tool_calls"][0]["function"];
            assert_eq!(function["name"], NAME);
            let arguments: Value =
                serde_json::from_str(function["arguments"].as_str().unwrap()).unwrap();
            assert_eq!(arguments, json!({"input": patch}));
            assert_eq!(messages[call + 1]["role"], "tool");
            assert_eq!(messages[call + 1]["tool_call_id"], call_id);
            assert_eq!(messages[call + 1]["content"], output);
        }
        Wire::Responses | Wire::Codex => {
            let input = request["input"].as_array().unwrap();
            let call = input
                .iter()
                .position(|item| item["call_id"] == call_id)
                .unwrap_or_else(|| panic!("no {call_id} call in {input:?}"));
            assert_eq!(
                input[call..call + 2],
                [
                    json!({"type": "custom_tool_call", "call_id": call_id, "name": NAME, "input": patch}),
                    json!({"type": "custom_tool_call_output", "call_id": call_id, "output": output}),
                ]
            );
        }
    }
}

/// Given: a Worker whose model selects apply_patch on `wire`, with edit, write and
///        apply_patch all registered
/// When: it patches the workspace with a streamed call, reads, and answers
/// Then: the files change, every request shows apply_patch alone, the call and its
///       output are replayed in the wire's shape, and each request keeps the whole
///       previous one as a prefix that the input-derived cache reports as cached
async fn patch_turns_reuse_the_whole_previous_request(wire: Wire) {
    let (_workspace, root) = workspace();
    let model = served_model(wire);
    let mut harness = patch_harness(
        wire,
        vec![
            patch_call(wire, "patch-call", &PATCH),
            read_call(model, 0),
            text(model, "done"),
        ],
        &root,
    );
    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Patch the notes".into(),
        RunConfig::default(),
    );
    let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    verify_trace(&harness, run, &events, 0);
    assert_eq!(
        std::fs::read_to_string(root.join("notes.txt")).unwrap(),
        "alpha\ngamma\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("added.txt")).unwrap(),
        "hello\n"
    );
    let requests = wire_requests(&harness);
    assert_eq!(requests.len(), 3);
    for request in &requests {
        assert_eq!(request["model"], model);
        assert_only_apply_patch(wire, request);
    }
    for request in &requests[1..] {
        assert_replayed_call(wire, request, "patch-call", &PATCH.concat(), PATCHED);
    }
}

#[tokio::test]
async fn openai_responses_freeform_patch_turns_reuse_the_whole_previous_request() {
    patch_turns_reuse_the_whole_previous_request(Wire::Responses).await;
}

#[tokio::test]
async fn codex_freeform_patch_turns_reuse_the_whole_previous_request() {
    patch_turns_reuse_the_whole_previous_request(Wire::Codex).await;
}

#[tokio::test]
async fn chat_function_patch_turns_reuse_the_whole_previous_request() {
    patch_turns_reuse_the_whole_previous_request(Wire::ChatCompletions).await;
}

/// Given: a patch and a long reply on `wire`, then a manual compaction
/// When: the follow-up patches again after the boundary
/// Then: the boundary replaces history once with the checkpoint, and the request after
///       the new call keeps the post-boundary request, including the replayed call and
///       output, as a prefix the input-derived cache reports as cached
async fn patch_after_compaction_reuses_the_new_prefix(wire: Wire) {
    let review_storage = crate::support::ReviewStorage::new();
    let (_workspace, root) = workspace();
    let model = served_model(wire);
    let mut harness = patch_harness(
        wire,
        vec![
            patch_call(wire, "patch-before", &PATCH),
            text(model, &"old analysis that can be summarized ".repeat(100)),
            patch_call(wire, "patch-after", &FOLLOW_UP),
            text(model, "done"),
        ],
        &root,
    );
    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Patch the notes twice".into(),
        RunConfig {
            interactive: true,
            ..Default::default()
        },
    );
    let mut events = through_phase(&mut harness.receiver, run, AgentRunPhase::Waiting).await;
    harness.runtime.compact(run).unwrap();
    review_storage.attach(&harness.runtime);
    harness
        .runtime
        .send_message(run, "Continue after this boundary".into())
        .unwrap();
    events.extend(through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await);
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::Compaction(CompactionEvent::Compacted {
            reason: CompactionReason::Manual,
            ..
        })
    )));
    verify_trace(&harness, run, &events, 1);
    assert_eq!(
        std::fs::read_to_string(root.join("notes.txt")).unwrap(),
        "alpha\ndelta\n"
    );
    let requests = wire_requests(&harness);
    assert_eq!(requests.len(), 4);
    for request in &requests {
        assert_only_apply_patch(wire, request);
    }
    assert_replayed_call(wire, &requests[1], "patch-before", &PATCH.concat(), PATCHED);
    assert_replayed_call(
        wire,
        &requests[3],
        "patch-after",
        &FOLLOW_UP.concat(),
        FOLLOWED_UP,
    );
}

#[tokio::test]
async fn openai_responses_patch_after_compaction_reuses_the_new_prefix() {
    patch_after_compaction_reuses_the_new_prefix(Wire::Responses).await;
}

#[tokio::test]
async fn chat_patch_after_compaction_reuses_the_new_prefix() {
    patch_after_compaction_reuses_the_new_prefix(Wire::ChatCompletions).await;
}
