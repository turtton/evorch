//! Rebuilding the same Codex profile/model with a different editing override is a
//! ModelSwitch boundary, not permission to replay the previous opaque checkpoint.

use std::sync::Arc;

use config::{Config, EditToolConfig, LoadOptions};
use event_bus::{
    AgentRunPhase, CompactionEvent, CompactionReason, EventBus, EventKind, ProviderEvent,
};
use mock_openai::cache_contract::{
    CacheProtocol, assert_append_only, assert_compaction_transition,
};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use runtime::compose::SwitchableModel;
use runtime::{ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use sandbox::credential::FileCredentialStore;
use serde_json::{Value, json};
use tools::ToolExecutor;

use super::{GPT5, NAME, PATCH, editing_tools, patch_call, read_call, text, workspace};
use crate::{BulkRead, CODEX_ACCOUNT, Wire, codex_model, contract_prompts, through_phase};

const BLOB: &str = "opaque-state-before-edit-tool-reconfiguration";

#[tokio::test]
async fn reconfigure_apply_patch_to_edit_resummarizes_then_reuses_the_prefix() {
    reconfigure(EditToolConfig::ApplyPatch, EditToolConfig::Edit).await;
}

#[tokio::test]
async fn reconfigure_edit_to_apply_patch_resummarizes_then_reuses_the_prefix() {
    reconfigure(EditToolConfig::Edit, EditToolConfig::ApplyPatch).await;
}

// Given: one Codex profile/model has produced an official opaque checkpoint.
// When: configuration is rebuilt with the opposite model-level edit_tool override.
// Then: the next request follows one ModelSwitch re-summary boundary, never replays
//       the old blob, and subsequent tool turns reuse the new wire prefix/cache.
async fn reconfigure(before_tool: EditToolConfig, after_tool: EditToolConfig) {
    let review_storage = crate::support::ReviewStorage::new();
    let (_workspace, root) = workspace();
    let editing_call = match before_tool {
        EditToolConfig::ApplyPatch => patch_call(Wire::Codex, "editing-after-checkpoint", &PATCH),
        EditToolConfig::Edit => ScriptedResponse::tool_call(
            "edit-after-checkpoint",
            GPT5,
            0,
            "editing-after-checkpoint",
            "edit",
            [json!({
                "path": root.join("notes.txt"), "old_string": "beta", "new_string": "gamma"
            })
            .to_string()],
        ),
    };
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            text(GPT5, &"analysis before reconfiguration ".repeat(100)),
            ScriptedResponse::compaction("compaction", GPT5, BLOB),
            editing_call,
            text(GPT5, "ready for reconfiguration"),
            text(GPT5, "Notes contain alpha and gamma. Continue reading."),
            read_call(GPT5, 0),
            text(GPT5, "done"),
        ],
        vec![GPT5.into()],
    );
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join(config::PROJECT_CONFIG_DIR)).unwrap();
    std::fs::write(
        config::project_main_config_path(directory.path()),
        format!(
            r#"
version = 3
[providers.local]
type = "openai-codex"
api_protocol = "openai-codex-responses"
base_url = "{base_url}"
credential = {{ type = "keyring", service = "evorch", account = "{CODEX_ACCOUNT}" }}
models = [{{ id = "{GPT5}", enabled = true }}]
default_model = "{GPT5}"
[[routing.routes.worker]]
profile = "local"
[agents.worker]
models = [{{ route = "worker" }}]
[compaction]
context_window_tokens = 1000000
threshold = 0.5
keep_recent_tokens = 1
max_summary_bytes = 256
summarizer = "model"
"#,
            base_url = mock.codex_base_url(),
        ),
    )
    .unwrap();
    let mut config = Config::load(&LoadOptions {
        project_dir: Some(directory.path().into()),
        user_config_dir: Some(directory.path().join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    config.providers.get_mut("local").unwrap().models[0].edit_tool = Some(before_tool);
    let bus = Arc::new(EventBus::new(1024));
    let mut receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(BulkRead)).unwrap();
    for tool in editing_tools() {
        executor.register(tool).unwrap();
    }
    let store = Arc::new(FileCredentialStore::open(directory.path().join("credentials")).unwrap());
    let live = Arc::new(SwitchableModel::new(codex_model(
        &config,
        &store,
        &bus,
        &mock.codex_base_url(),
    )));
    let runtime = compose_runtime(RuntimeComposition {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(directory.path().join("empty-user-config")),
        config: &config,
        bus: bus.clone(),
        executor: Arc::new(executor),
        credential_store: store.clone(),
        env: Arc::new(routing::MapEnv::default()),
        model_source: ModelSource::Fixed(live.clone()),
        workspace: None,
    })
    .unwrap()
    .runtime
    .with_sequential_run_ids()
    .with_system_prompts(contract_prompts())
    .with_compaction(config.compaction.clone());
    runtime.set_project_root(root).unwrap();
    let run = runtime.delegate_background(
        Role::Worker,
        "Analyze the notes".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    let mut events = through_phase(&mut receiver, run, AgentRunPhase::Waiting).await;
    runtime.compact(run).unwrap();
    review_storage.attach(&runtime);
    runtime
        .send_message(run, "Create the checkpoint".into())
        .unwrap();
    events.extend(through_phase(&mut receiver, run, AgentRunPhase::Waiting).await);

    config.providers.get_mut("local").unwrap().models[0].edit_tool = Some(after_tool);
    live.replace(codex_model(&config, &store, &bus, &mock.codex_base_url()));
    runtime.send_message(run, "Continue".into()).unwrap();
    events.extend(through_phase(&mut receiver, run, AgentRunPhase::Waiting).await);
    runtime.stop(run, runtime::StopScope::SelfOnly).unwrap();
    events.extend(through_phase(&mut receiver, run, AgentRunPhase::Stopped).await);
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Stopped);

    let checkpoints: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Compaction(CompactionEvent::Compacted {
                reason,
                checkpoint_id,
                ..
            }) => Some((*reason, checkpoint_id)),
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
    let requests: Vec<Value> = mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.method == "POST" && request.path == Wire::Codex.path())
        .map(|request| request.body)
        .collect();
    assert_eq!(requests.len(), 7);
    assert_eq!(
        requests[1]["input"].as_array().unwrap().last().unwrap()["type"],
        "compaction_trigger"
    );
    for (request, selection) in [
        (&requests[0], before_tool),
        (&requests[1], before_tool),
        (&requests[2], before_tool),
        (&requests[3], before_tool),
        (&requests[5], after_tool),
        (&requests[6], after_tool),
    ] {
        let names: Vec<_> = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names.contains(&NAME),
            selection == EditToolConfig::ApplyPatch
        );
        for name in ["edit", "write"] {
            assert_eq!(names.contains(&name), selection == EditToolConfig::Edit);
        }
    }
    assert!(
        requests[2]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "compaction" && item["encrypted_content"] == BLOB)
    );
    assert!(requests[4]["tools"].as_array().unwrap().is_empty());
    for item in requests[4]["input"].as_array().unwrap() {
        assert!(
            !matches!(
                item["type"].as_str(),
                Some(
                    "custom_tool_call"
                        | "custom_tool_call_output"
                        | "function_call"
                        | "function_call_output"
                        | "tool_use"
                        | "tool_result"
                )
            ),
            "{item}"
        );
    }
    let summary_input = requests[4]["input"].to_string();
    assert!(summary_input.contains("[tool call"), "{summary_input}");
    assert!(summary_input.contains("[tool result"), "{summary_input}");
    for request in &requests[5..] {
        assert!(
            request["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["type"] != "compaction")
        );
        assert!(!request.to_string().contains(BLOB));
        assert!(!request.to_string().contains("editing-after-checkpoint"));
    }
    let mut previous = requests[3].clone();
    previous["tools"] = requests[5]["tools"].clone();
    assert_compaction_transition(
        CacheProtocol::Codex,
        &previous,
        &requests[5],
        checkpoints[1].1,
    )
    .unwrap();
    assert_append_only(CacheProtocol::Codex, &requests[5], &requests[6]).unwrap();
    let usage: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                input_tokens,
                cache_read_tokens,
                ..
            }) => Some((*input_tokens, *cache_read_tokens)),
            _ => None,
        })
        .collect();
    let after_usage = &usage[usage.len() - 2..];
    // The profile/model is unchanged, so stable instructions may already be cached.
    assert!(
        after_usage[1].1 >= after_usage[0].0,
        "the new prefix is reused: {after_usage:?}"
    );
    assert_eq!(mock.remaining_scripts(), 0);
}
