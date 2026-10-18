//! A chain fallback to a smaller window compacts at an explicit cache boundary, then the
//! new member's prefix is preserved across later turns (ADR 0030).
mod support;
use std::sync::Arc;

use config::{Config, LoadOptions};
use event_bus::{
    AgentRunPhase, CompactionEvent, CompactionReason, EventBus, EventKind, LifecycleEvent,
    ProviderEvent,
};
use mock_openai::cache_contract::{
    CacheProtocol, assert_append_only, assert_compaction_transition,
};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::MapEnv;
use runtime::{ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use sandbox::credential::FileCredentialStore;
use serde_json::{Value, json};
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};

const PRIMARY_MODEL: &str = "mock-model";
const SMALL_MODEL: &str = "small-model";
const KEY_ENV: &str = "EVORCH_SWITCH_CACHE_KEY";

struct Read;

#[async_trait::async_trait]
impl Tool for Read {
    fn name(&self) -> &'static str {
        "read"
    }
    fn schema(&self) -> Value {
        json!({"type":"object", "required":["index"], "properties":{"index":{"type":"integer"}}})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    async fn execute(&self, input: Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success(
            format!("result-{}\n", input["index"]).repeat(40),
        ))
    }
}

fn read(model: &str, index: usize) -> ScriptedResponse {
    ScriptedResponse::tool_call(
        &format!("response-{index}"),
        model,
        0,
        &format!("call-{index}"),
        "read",
        [json!({"index":index}).to_string()],
    )
}

fn chat_requests(mock: &StreamingMockOpenAi) -> Vec<Value> {
    mock.recorded_requests()
        .into_iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| request.body)
        .collect()
}

async fn switch_preserves_current_note_and_new_prefix(compacts: bool) {
    let review_storage = support::ReviewStorage::new();
    // Given: a worker chain whose primary answers two turns and then fails, and a
    // fallback whose 40K window cannot hold the accumulated history.
    let old_reply = "old analysis that can be summarized ".repeat(3_000);
    let primary = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            ScriptedResponse::tool_call(
                "note",
                PRIMARY_MODEL,
                0,
                "note",
                "working_note_write",
                [json!({"body":"Switch finding with original evidence references"}).to_string()],
            ),
            read(PRIMARY_MODEL, 0),
            ScriptedResponse::text_stream("reply", PRIMARY_MODEL, [old_reply.as_str()]),
        ],
        vec![PRIMARY_MODEL.into()],
    );
    let small = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            read(SMALL_MODEL, 1),
            ScriptedResponse::text_stream("reply", SMALL_MODEL, ["done"]),
        ],
        vec![SMALL_MODEL.into()],
    );
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join(config::PROJECT_CONFIG_DIR)).unwrap();
    std::fs::write(
        config::project_main_config_path(directory.path()),
        format!(
            r#"
version = 3
[providers.primary]
type = "openai-compatible"
base_url = "{primary}"
api_key_env = "{KEY_ENV}"
models = [{{ id = "{PRIMARY_MODEL}", enabled = true, preset = "primary-output" }}]
default_model = "{PRIMARY_MODEL}"
[providers.small]
type = "openai-compatible"
base_url = "{small}"
api_key_env = "{KEY_ENV}"
models = [{{ id = "{SMALL_MODEL}", enabled = true, metadata_source = "manual", context_window = {target_window}, preset = "small-output" }}]
default_model = "{SMALL_MODEL}"
[model_presets.primary-output]
max_output_tokens = 32768
[model_presets.small-output]
max_output_tokens = 16384
[[routing.routes.primary]]
profile = "primary"
[[routing.routes.small]]
profile = "small"
[agents.worker]
models = [{{ route = "primary" }}, {{ route = "small" }}]
[compaction]
context_window_tokens = 1000000
threshold = 0.5
keep_recent_tokens = 1
max_summary_bytes = 256
summarizer = "structural"
"#,
            primary = primary.base_url(),
            small = small.base_url(),
            target_window = if compacts { 40_000 } else { 1_000_000 },
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
    let storage_config = storage::StorageConfig {
        db_path: directory.path().join("switch-notes.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let mut receiver = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(Read)).unwrap();
    let runtime = compose_runtime(RuntimeComposition {
        user_config_dir: Some(directory.path().join("empty-user-config")),
        config: &config,
        bus,
        executor: Arc::new(executor),
        credential_store: Arc::new(
            FileCredentialStore::open(directory.path().join("credentials")).unwrap(),
        ),
        env: Arc::new(MapEnv::from_iter([(KEY_ENV, "offline-test-key")])),
        model_source: ModelSource::Configured,
        workspace: None,
    })
    .unwrap()
    .runtime
    .with_run_store(runtime::RunStore::open(&storage_config, storage.handle()).unwrap())
    .with_compaction(config.compaction.clone());
    // The run was told an allowed directory, which the boundary must restate (ADR 0032).
    let root = directory.path().canonicalize().unwrap();
    let shared = root.join("shared");
    std::fs::create_dir(&shared).unwrap();
    runtime.set_project_root(root.clone()).unwrap();
    runtime.set_project_allowed_directories(root, vec![shared.clone()]);
    let run = runtime.delegate_background(
        Role::Worker,
        "Preserve this original goal".into(),
        RunConfig {
            interactive: true,
            ..Default::default()
        },
    );
    let mut events = Vec::new();
    let mut through = async |phase: AgentRunPhase| loop {
        let event = receiver.recv().await.unwrap();
        let reached = matches!(&event.kind,
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
                if run_id == &run.to_string() && *to == phase);
        events.push(event);
        if reached {
            break;
        }
    };
    through(AgentRunPhase::Waiting).await;
    // When: the next turn fails over to the small member.
    review_storage.attach(&runtime);
    runtime
        .send_message(run, "Continue after this boundary".into())
        .unwrap();
    through(AgentRunPhase::Done).await;
    // Then: one model-switch compaction precedes the first request to the new member.
    let checkpoints = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Compaction(CompactionEvent::Compacted {
                reason,
                checkpoint_id,
                ..
            }) => Some((*reason, checkpoint_id.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(checkpoints.len(), usize::from(compacts), "{checkpoints:?}");
    let before = chat_requests(&primary);
    let after = chat_requests(&small);
    assert_eq!(after.len(), 2);
    for request in &before {
        assert_eq!(request["max_tokens"], 32_768);
    }
    for request in &after {
        assert_eq!(request["max_tokens"], 16_384);
    }
    for pair in before.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    // Model switching is an explicit cache boundary; only this boundary may
    // replace the model and its output ceiling. Later requests keep both fixed.
    let mut last_primary = before.last().unwrap().clone();
    last_primary["model"] = json!(SMALL_MODEL);
    last_primary["max_tokens"] = json!(16_384);
    if compacts {
        let (reason, checkpoint_id) = &checkpoints[0];
        assert_eq!(*reason, CompactionReason::ModelSwitch);
        assert_compaction_transition(
            CacheProtocol::OpenAi,
            &last_primary,
            &after[0],
            checkpoint_id,
        )
        .unwrap();
    } else {
        assert_append_only(CacheProtocol::OpenAi, &last_primary, &after[0]).unwrap();
    }
    assert_append_only(CacheProtocol::OpenAi, &after[0], &after[1]).unwrap();
    let note_blocks = |request: &Value| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| {
                message["role"] == "user"
                    && message["content"].to_string().contains("[working-note]")
            })
            .count()
    };
    assert_eq!(note_blocks(before.last().unwrap()), 0);
    assert_eq!(note_blocks(&after[0]), 1);
    assert_eq!(note_blocks(&after[1]), 1);
    assert!(
        after[0]["messages"]
            .to_string()
            .contains("Switch finding with original evidence references")
    );
    let listed = format!("* {} (read-write)", shared.display());
    let restatements = |request: &Value| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| {
                message["role"] == "user"
                    && message["content"]
                        .to_string()
                        .contains("[workspace update] Current allowed directories")
            })
            .count()
    };
    assert!(
        before[0]["messages"][0]["content"]
            .to_string()
            .contains(&listed)
    );
    assert_eq!(restatements(before.last().unwrap()), 0);
    assert_eq!(restatements(&after[0]), usize::from(compacts));
    assert_eq!(restatements(&after[1]), usize::from(compacts));
    let usage = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                profile,
                input_tokens,
                cache_read_tokens,
                ..
            }) if profile.as_deref() == Some("small") => Some((*input_tokens, *cache_read_tokens)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0].1, 0, "the new member starts cold");
    assert!(
        usage[1].1 >= usage[0].0,
        "the new prefix is reused: {usage:?}"
    );
    assert_eq!(primary.remaining_scripts() + small.remaining_scripts(), 0);
}

#[tokio::test]
async fn fallback_to_a_smaller_window_compacts_once_then_preserves_the_new_prefix() {
    switch_preserves_current_note_and_new_prefix(true).await;
}

#[tokio::test]
async fn fallback_without_compaction_presents_current_note_then_preserves_the_new_prefix() {
    switch_preserves_current_note_and_new_prefix(false).await;
}
