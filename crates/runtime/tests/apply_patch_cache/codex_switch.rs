//! D3: switching between apply_patch and edit/write is a re-summary boundary. Even when
//! the next member could replay the provider checkpoint, the checkpoint written while
//! the model edited with apply_patch is re-summarized as text, so the new member only
//! sees history it can act on, and its new prefix is preserved afterwards.

use std::sync::Arc;

use config::{Config, LoadOptions};
use event_bus::{
    AgentRunPhase, CompactionEvent, CompactionReason, EventBus, EventKind, ProviderEvent,
};
use mock_openai::cache_contract::{
    CacheProtocol, assert_append_only, assert_compaction_transition,
};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use runtime::{ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use sandbox::credential::FileCredentialStore;
use serde_json::Value;
use tools::ToolExecutor;

use super::{
    GPT5, NAME, PATCH, PATCHED, assert_only_apply_patch, assert_replayed_call, editing_tools,
    patch_call, read_call, text, workspace,
};
use crate::{BulkRead, CODEX_ACCOUNT, Wire, codex_model, contract_prompts, through_phase};

const BLOB: &str = "opaque-codex-state-written-while-editing-with-apply-patch";

fn codex_requests(mock: &StreamingMockOpenAi) -> Vec<Value> {
    mock.recorded_requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == Wire::Codex.path())
        .map(|request| request.body)
        .collect()
}

fn requests_compaction(request: &Value) -> bool {
    request["input"]
        .as_array()
        .unwrap()
        .last()
        .is_some_and(|item| item["type"] == "compaction_trigger")
}

fn tool_names(request: &Value) -> Vec<&str> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect()
}

/// Tool definitions other than `excluded`, in wire order.
fn tools_without(request: &Value, excluded: &[&str]) -> Vec<Value> {
    request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tool| !excluded.contains(&tool["name"].as_str().unwrap()))
        .cloned()
        .collect()
}

// Given: a Worker chain of two Codex members serving the same GPT-5 model, the first
//        selecting apply_patch by default and the second edit through its model entry;
//        the first patches, answers, and compacts officially at a manual boundary
// When: the first member fails and the chain falls back to the second, which replays
//       Codex compaction
// Then: the edit-tool switch still re-summarizes the checkpoint once (ModelSwitch), the
//       new member sees edit/write without apply_patch, the opaque checkpoint or any
//       apply_patch call, every other tool and the instructions are unchanged, and the
//       new member's prefix is reused afterwards
#[tokio::test]
async fn edit_tool_switch_resummarizes_a_replayable_codex_checkpoint_then_reuses_the_new_prefix() {
    let review_storage = crate::support::ReviewStorage::new();
    let (_workspace, root) = workspace();
    let patch_mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            patch_call(Wire::Codex, "patch-call", &PATCH),
            text(GPT5, &"old analysis that can be summarized ".repeat(100)),
            ScriptedResponse::compaction("compaction", GPT5, BLOB),
        ],
        vec![GPT5.into()],
    );
    let edit_mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![read_call(GPT5, 0), text(GPT5, "done")],
        vec![GPT5.into()],
    );
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join(config::PROJECT_CONFIG_DIR)).unwrap();
    let credential =
        format!(r#"{{ type = "keyring", service = "evorch", account = "{CODEX_ACCOUNT}" }}"#);
    std::fs::write(
        config::project_main_config_path(directory.path()),
        format!(
            r#"
version = 3
[providers.patch]
type = "openai-codex"
api_protocol = "openai-codex-responses"
base_url = "{patch}"
credential = {credential}
models = ["{GPT5}"]
default_model = "{GPT5}"
[providers.edit]
type = "openai-codex"
api_protocol = "openai-codex-responses"
base_url = "{edit}"
credential = {credential}
models = [{{ id = "{GPT5}", enabled = true, edit_tool = "edit" }}]
default_model = "{GPT5}"
[[routing.routes.patch]]
profile = "patch"
[[routing.routes.edit]]
profile = "edit"
[agents.worker]
models = [{{ route = "patch" }}, {{ route = "edit" }}]
[compaction]
context_window_tokens = 1000000
threshold = 0.5
keep_recent_tokens = 1
max_summary_bytes = 256
summarizer = "structural"
"#,
            patch = patch_mock.codex_base_url(),
            edit = edit_mock.codex_base_url(),
        ),
    )
    .unwrap();
    let config = Config::load(&LoadOptions {
        project_dir: Some(directory.path().into()),
        user_config_dir: Some(directory.path().join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let mut receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(BulkRead)).unwrap();
    for tool in editing_tools() {
        executor.register(tool).unwrap();
    }
    let store = Arc::new(FileCredentialStore::open(directory.path().join("credentials")).unwrap());
    let model = codex_model(&config, &store, &bus, &patch_mock.codex_base_url());
    let runtime = compose_runtime(RuntimeComposition {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(directory.path().join("empty-user-config")),
        config: &config,
        bus,
        executor: Arc::new(executor),
        credential_store: store,
        env: Arc::new(routing::MapEnv::default()),
        model_source: ModelSource::Fixed(model),
        workspace: None,
    })
    .unwrap()
    .runtime
    .with_sequential_run_ids()
    .with_system_prompts(contract_prompts())
    .with_compaction(config.compaction.clone());
    runtime.set_project_root(root.clone()).unwrap();
    let run = runtime.delegate_background(
        Role::Worker,
        "Patch the notes".into(),
        RunConfig {
            interactive: true,
            ..Default::default()
        },
    );
    let mut events = through_phase(&mut receiver, run, AgentRunPhase::Waiting).await;
    runtime.compact(run).unwrap();
    review_storage.attach(&runtime);
    runtime
        .send_message(run, "Continue after this boundary".into())
        .unwrap();
    events.extend(through_phase(&mut receiver, run, AgentRunPhase::Done).await);
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);

    let checkpoints: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Compaction(CompactionEvent::Compacted {
                reason,
                checkpoint_id,
                ..
            }) => Some((*reason, checkpoint_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        checkpoints
            .iter()
            .map(|(reason, _)| *reason)
            .collect::<Vec<_>>(),
        [CompactionReason::Manual, CompactionReason::ModelSwitch]
    );
    assert!(events.iter().all(|event| !matches!(&event.kind,
        EventKind::Diagnostic(diagnostic) if diagnostic.code == "CacheRegression")));
    assert_eq!(
        std::fs::read_to_string(root.join("notes.txt")).unwrap(),
        "alpha\ngamma\n"
    );

    // The apply_patch member: two turns, its official compaction, then the request the
    // fallback abandons (retried unchanged), which replays the opaque checkpoint.
    let (compactions, before): (Vec<_>, Vec<_>) = codex_requests(&patch_mock)
        .into_iter()
        .partition(requests_compaction);
    assert_eq!(compactions.len(), 1);
    assert!(before.len() >= 3, "{}", before.len());
    for request in &before {
        assert_only_apply_patch(Wire::Codex, request);
    }
    assert_replayed_call(
        Wire::Codex,
        &before[1],
        "patch-call",
        &PATCH.concat(),
        PATCHED,
    );
    assert_append_only(CacheProtocol::Codex, &before[0], &before[1]).unwrap();
    assert_compaction_transition(
        CacheProtocol::Codex,
        &before[1],
        &before[2],
        &checkpoints[0].1,
    )
    .unwrap();
    assert!(
        before[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "compaction" && item["encrypted_content"] == BLOB)
    );
    for retry in &before[3..] {
        assert_eq!(retry, &before[2]);
    }

    // The edit member: the re-summary is the boundary's one history replacement.
    let after = codex_requests(&edit_mock);
    assert_eq!(after.len(), 2);
    for request in &after {
        let names = tool_names(request);
        assert!(
            names.contains(&"edit") && names.contains(&"write"),
            "{names:?}"
        );
        assert!(!names.contains(&NAME), "{names:?}");
        for item in request["input"].as_array().unwrap() {
            assert!(
                !matches!(
                    item["type"].as_str(),
                    Some("compaction" | "custom_tool_call" | "custom_tool_call_output")
                ),
                "{item}"
            );
            assert_ne!(item["name"], NAME, "{item}");
        }
    }
    let last = before.last().unwrap();
    assert_eq!(
        tools_without(last, &[NAME]),
        tools_without(&after[0], &["edit", "write"])
    );
    let mut switched = last.clone();
    switched["tools"] = after[0]["tools"].clone();
    assert_compaction_transition(
        CacheProtocol::Codex,
        &switched,
        &after[0],
        &checkpoints[1].1,
    )
    .unwrap();
    assert_append_only(CacheProtocol::Codex, &after[0], &after[1]).unwrap();
    let usage: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                profile,
                input_tokens,
                cache_read_tokens,
                ..
            }) if profile.as_deref() == Some("edit") => Some((*input_tokens, *cache_read_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0].1, 0, "the new member starts cold");
    assert!(
        usage[1].1 >= usage[0].0,
        "the new prefix is reused: {usage:?}"
    );
    assert_eq!(
        patch_mock.remaining_scripts() + edit_mock.remaining_scripts(),
        0
    );
}
