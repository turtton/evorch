//! D3 without a checkpoint: incompatible editing calls are summarized before the
//! fallback member's first agent request, then its new cache prefix remains stable.

use std::sync::Arc;

use config::{Config, EditToolConfig, LoadOptions};
use event_bus::{
    AgentRunPhase, CompactionEvent, CompactionReason, EventBus, EventKind, ProviderEvent,
};
use mock_openai::cache_contract::{
    CacheProtocol, assert_append_only, assert_compaction_transition,
};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use runtime::{ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use sandbox::credential::FileCredentialStore;
use serde_json::{Value, json};
use tools::ToolExecutor;

use super::{NAME, PATCH, editing_tools, patch_call, read_call, text, workspace};
use crate::{BulkRead, KEY_ENV, MODEL, Wire, contract_prompts, through_phase};

#[tokio::test]
async fn chat_apply_patch_to_edit_fallback_summarizes_without_a_checkpoint_and_reuses_prefix() {
    fallback(EditToolConfig::ApplyPatch, EditToolConfig::Edit).await;
}

#[tokio::test]
async fn chat_edit_to_apply_patch_fallback_summarizes_without_a_checkpoint_and_reuses_prefix() {
    fallback(EditToolConfig::Edit, EditToolConfig::ApplyPatch).await;
}

#[tokio::test]
async fn same_edit_interface_fallback_keeps_raw_history_and_prefix_without_summarizing() {
    for selection in [EditToolConfig::ApplyPatch, EditToolConfig::Edit] {
        fallback(selection, selection).await;
    }
}

async fn fallback(before: EditToolConfig, after: EditToolConfig) {
    let (_workspace, root) = workspace();
    let editing_call = match before {
        EditToolConfig::ApplyPatch => patch_call(Wire::ChatCompletions, "old-editing-call", &PATCH),
        EditToolConfig::Edit => ScriptedResponse::tool_call(
            "edit-response",
            MODEL,
            0,
            "old-editing-call",
            "edit",
            [json!({
                "path": root.join("notes.txt"), "old_string": "beta", "new_string": "gamma"
            })
            .to_string()],
        ),
    };
    let head =
        StreamingMockOpenAi::spawn_with_prompt_cache_models(vec![editing_call], vec![MODEL.into()]);
    let switching = before != after;
    let mut script = Vec::new();
    if switching {
        script.push(text(
            MODEL,
            "Notes now contain alpha and gamma. Continue by reading.",
        ));
    }
    script.extend([read_call(MODEL, 0), text(MODEL, "done")]);
    let next = StreamingMockOpenAi::spawn_with_prompt_cache_models(script, vec![MODEL.into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join(config::PROJECT_CONFIG_DIR)).unwrap();
    std::fs::write(
        config::project_main_config_path(directory.path()),
        format!(
            r#"
version = 3
[providers.head]
type = "openai-compatible"
base_url = "{head_url}"
api_key_env = "{KEY_ENV}"
models = ["{MODEL}"]
default_model = "{MODEL}"
[providers.next]
type = "openai-compatible"
base_url = "{next_url}"
api_key_env = "{KEY_ENV}"
models = ["{MODEL}"]
default_model = "{MODEL}"
[[routing.routes.head]]
profile = "head"
[[routing.routes.next]]
profile = "next"
[agents.worker]
models = [{{ route = "head" }}, {{ route = "next" }}]
[compaction]
context_window_tokens = 1000000
threshold = 0.5
keep_recent_tokens = 200000
max_summary_bytes = 1024
summarizer = "model"
"#,
            head_url = head.base_url(),
            next_url = next.base_url()
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
    config.providers.get_mut("head").unwrap().models[0].edit_tool = Some(before);
    config.providers.get_mut("next").unwrap().models[0].edit_tool = Some(after);
    let bus = Arc::new(EventBus::new(1024));
    let mut receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(BulkRead)).unwrap();
    for tool in editing_tools() {
        executor.register(tool).unwrap();
    }
    let store = Arc::new(FileCredentialStore::open(directory.path().join("credentials")).unwrap());
    let runtime = compose_runtime(RuntimeComposition {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(directory.path().join("empty-user-config")),
        config: &config,
        bus,
        executor: Arc::new(executor),
        credential_store: store,
        env: Arc::new(routing::MapEnv::from_iter([(KEY_ENV, "offline-key")])),
        model_source: ModelSource::Configured,
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
        "Change beta to gamma, then read".into(),
        RunConfig::default(),
    );
    let events = through_phase(&mut receiver, run, AgentRunPhase::Done).await;
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(
        std::fs::read_to_string(root.join("notes.txt")).unwrap(),
        "alpha\ngamma\n"
    );
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
        if switching {
            vec![CompactionReason::ModelSwitch]
        } else {
            vec![]
        }
    );
    assert!(events.iter().all(|event| !matches!(&event.kind,
        EventKind::Diagnostic(diagnostic) if diagnostic.code == "CacheRegression")));
    let requests = |mock: &StreamingMockOpenAi| -> Vec<Value> {
        mock.recorded_requests()
            .into_iter()
            .filter(|request| {
                request.method == "POST" && request.path == Wire::ChatCompletions.path()
            })
            .map(|request| request.body)
            .collect()
    };
    let before_requests = requests(&head);
    let after_requests = requests(&next);
    assert_eq!(after_requests.len(), if switching { 3 } else { 2 });
    // The head's first turn must be an ordinary agent request, not a summary.
    assert!(!before_requests[0]["tools"].as_array().unwrap().is_empty());
    let agent_requests = if switching {
        &after_requests[1..]
    } else {
        &after_requests[..]
    };
    for request in agent_requests {
        let names: Vec<_> = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.contains(&NAME), after == EditToolConfig::ApplyPatch);
        for tool in ["edit", "write"] {
            assert_eq!(names.contains(&tool), after == EditToolConfig::Edit);
        }
        let old_call_present = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| calls.iter().any(|call| call["id"] == "old-editing-call"))
            });
        assert_eq!(old_call_present, !switching, "{request}");
        let old_result_present = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["tool_call_id"] == "old-editing-call");
        assert_eq!(old_result_present, !switching, "{request}");
    }
    let mut previous = before_requests.last().unwrap().clone();
    previous["tools"] = agent_requests[0]["tools"].clone();
    if switching {
        assert!(
            after_requests[0]["tools"]
                .as_array()
                .is_none_or(Vec::is_empty)
        );
        for message in after_requests[0]["messages"].as_array().unwrap() {
            assert!(message.get("tool_calls").is_none(), "{message}");
            assert!(message.get("tool_call_id").is_none(), "{message}");
            assert_ne!(message["role"], "tool", "{message}");
        }
        let summary_input = after_requests[0]["messages"].to_string();
        assert!(summary_input.contains("[tool call"), "{summary_input}");
        assert!(summary_input.contains("[tool result"), "{summary_input}");
        assert!(summary_input.contains("gamma"), "{summary_input}");
        assert_compaction_transition(
            CacheProtocol::OpenAi,
            &previous,
            &agent_requests[0],
            checkpoints[0].1,
        )
        .unwrap();
    } else {
        assert_append_only(CacheProtocol::OpenAi, &previous, &agent_requests[0]).unwrap();
    }
    assert_append_only(
        CacheProtocol::OpenAi,
        &agent_requests[0],
        &agent_requests[1],
    )
    .unwrap();
    let usage: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                profile,
                input_tokens,
                cache_read_tokens,
                ..
            }) if profile.as_deref() == Some("next") => Some((*input_tokens, *cache_read_tokens)),
            _ => None,
        })
        .collect();
    assert_eq!(usage.len(), if switching { 3 } else { 2 });
    let usage = &usage[usage.len() - 2..];
    assert!(
        usage[1].1 >= usage[0].0,
        "new prefix must be reused: {usage:?}"
    );
    assert_eq!(head.remaining_scripts() + next.remaining_scripts(), 0);
}
