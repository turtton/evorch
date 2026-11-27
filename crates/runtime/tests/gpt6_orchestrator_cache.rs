//! gpt-6.x は GPT-5 以降の family に分類され、Orchestrator の `delegate` 説明に
//! `SUBAGENT_CONTEXT_NOTE` が付く。通常ターンでは System と tools (順序込み) が不変で、
//! snapshot のある run は開始時 family を復元後も意図的に固定する。snapshot 導入前の
//! record だけは復元時の family を使い、その後の tools と prefix は固定される。
mod support;

use std::sync::Arc;

use config::{Config, LoadOptions};
use event_bus::EventReceiver;
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent, ProviderEvent};
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::MapEnv;
use runtime::{
    AgentRuntime, ModelSource, Role, RunConfig, RunStore, RuntimeComposition,
    SUBAGENT_CONTEXT_NOTE, compose_runtime,
};
use sandbox::credential::FileCredentialStore;
use serde_json::Value;
use storage::{Storage, StorageConfig};
use tools::ToolExecutor;

const LEGACY_MODEL: &str = "legacy-model";
const GPT6_MODEL: &str = "gpt-6.1-sol";
const KEY_ENV: &str = "EVORCH_GPT6_CACHE_KEY";
const FAMILY_MARK: &str = "GPT5-FAMILY-MARK";

fn reply(index: usize, model: &str) -> ScriptedResponse {
    ScriptedResponse::text_stream(&format!("reply-{index}"), model, ["done"])
}

fn chat_requests(mock: &StreamingMockOpenAi) -> Vec<Value> {
    mock.recorded_requests()
        .into_iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| request.body)
        .collect()
}

/// Orchestrator が `route` のモデルに結ばれた設定を読む。
fn load_config(directory: &std::path::Path, mock: &StreamingMockOpenAi, route: &str) -> Config {
    std::fs::create_dir_all(directory.join(config::PROJECT_CONFIG_DIR)).unwrap();
    std::fs::write(
        config::project_main_config_path(directory),
        format!(
            r#"
version = 3
[providers.p]
type = "openai-compatible"
base_url = "{base}"
api_key_env = "{KEY_ENV}"
models = ["{LEGACY_MODEL}", "{GPT6_MODEL}"]
default_model = "{LEGACY_MODEL}"
[[routing.routes.legacy]]
profile = "p"
model = "{LEGACY_MODEL}"
[[routing.routes.gpt6]]
profile = "p"
model = "{GPT6_MODEL}"
[agents.orchestrator]
models = [{{ route = "{route}" }}]
"#,
            base = mock.base_url()
        ),
    )
    .unwrap();
    Config::load(&LoadOptions {
        project_dir: Some(directory.into()),
        user_config_dir: Some(directory.join(config::PROJECT_CONFIG_DIR)),
        read_env: false,
        ..Default::default()
    })
    .unwrap()
}

/// 本番と同じ composition root で runtime を作る。ユーザー preset の `family-gpt5` は
/// 目印の本文に上書きし、System が再合成されたかどうかを見分けられるようにする。
fn build_runtime(
    directory: &std::path::Path,
    config: &Config,
    storage_config: &StorageConfig,
    storage: &Storage,
) -> (AgentRuntime, EventReceiver) {
    let user_dir = directory.join("user-config");
    std::fs::create_dir_all(user_dir.join("presets")).unwrap();
    std::fs::write(
        user_dir.join("presets/family-gpt5.md"),
        format!("{FAMILY_MARK}\n"),
    )
    .unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let receiver = bus.subscribe();
    let runtime = compose_runtime(RuntimeComposition {
        model_catalog_dir: Some(test_support::model_catalog_dir()),
        user_config_dir: Some(user_dir),
        config,
        bus: bus.clone(),
        executor: Arc::new(ToolExecutor::new(bus)),
        credential_store: Arc::new(
            FileCredentialStore::open(directory.join("credentials")).unwrap(),
        ),
        env: Arc::new(MapEnv::from_iter([(KEY_ENV, "offline-test-key")])),
        model_source: ModelSource::Configured,
        workspace: None,
    })
    .unwrap()
    .runtime
    .with_run_store(RunStore::open(storage_config, storage.handle()).unwrap());
    (runtime, receiver)
}

/// `run` が `phase` に達するまでイベントを読み、観測した各リクエストの
/// (input_tokens, cache_read_tokens) を返す。
async fn through(
    receiver: &mut EventReceiver,
    run: runtime::RunId,
    phase: AgentRunPhase,
) -> Vec<(u64, u64)> {
    let mut usage = Vec::new();
    loop {
        let event = receiver.recv().await.unwrap();
        match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                input_tokens,
                cache_read_tokens,
                ..
            }) => usage.push((*input_tokens, *cache_read_tokens)),
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. })
                if run_id == &run.to_string() && *to == phase =>
            {
                return usage;
            }
            _ => {}
        }
    }
}

fn tools_of(request: &Value) -> &Vec<Value> {
    request["tools"].as_array().expect("tools 配列がある")
}

fn tool_names(request: &Value) -> Vec<&str> {
    tools_of(request)
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect()
}

fn delegate_description(request: &Value) -> &str {
    tools_of(request)
        .iter()
        .find(|tool| tool["function"]["name"] == "delegate")
        .and_then(|tool| tool["function"]["description"].as_str())
        .expect("Orchestrator は delegate を持つ")
}

fn system_of(request: &Value) -> &Value {
    assert_eq!(request["messages"][0]["role"], "system");
    &request["messages"][0]
}

// Given: gpt-6.1-sol の Orchestrator の対話 run
// When: 3 ターン実行し、wire の各リクエストと入力由来 mock cache の使用量を見る
// Then: System と tools (順序込み) が全ターンでバイト一致し、delegate の説明に
//       SUBAGENT_CONTEXT_NOTE がちょうど 1 回あり、cache は前ターンの入力を再利用する
#[tokio::test]
async fn gpt6_orchestrator_keeps_system_and_tools_byte_stable_across_turns() {
    let review_storage = support::ReviewStorage::new();
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        (0..3).map(|index| reply(index, GPT6_MODEL)).collect(),
        vec![GPT6_MODEL.into()],
    );
    let directory = tempfile::tempdir().unwrap();
    let config = load_config(directory.path(), &mock, "gpt6");
    let storage_config = StorageConfig {
        db_path: directory.path().join("turns.db"),
        ..Default::default()
    };
    let storage = Storage::open(storage_config.clone()).unwrap();
    let (runtime, mut receiver) =
        build_runtime(directory.path(), &config, &storage_config, &storage);
    review_storage.attach(&runtime);

    let run = runtime.delegate_background(
        Role::Orchestrator,
        "goal".into(),
        RunConfig {
            interactive: true,
            keep_alive: true,
            ..Default::default()
        },
    );
    let mut usage = through(&mut receiver, run, AgentRunPhase::Waiting).await;
    for message in ["second turn", "third turn"] {
        runtime.send_message(run, message.into()).unwrap();
        usage.extend(through(&mut receiver, run, AgentRunPhase::Waiting).await);
    }

    let requests = chat_requests(&mock);
    assert_eq!(requests.len(), 3);
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    for request in &requests {
        assert_eq!(
            tools_of(request),
            tools_of(&requests[0]),
            "tools は全ターンで同一"
        );
        assert_eq!(
            tool_names(request),
            tool_names(&requests[0]),
            "tools の順序は不変"
        );
        assert_eq!(
            system_of(request),
            system_of(&requests[0]),
            "System は全ターンで同一"
        );
        assert_eq!(
            delegate_description(request)
                .matches(SUBAGENT_CONTEXT_NOTE)
                .count(),
            1
        );
    }
    assert!(
        system_of(&requests[0])["content"]
            .to_string()
            .contains(FAMILY_MARK),
        "gpt-6.x は GPT-5 以降の family section を受ける"
    );
    assert_eq!(usage.len(), 3);
    for pair in usage.windows(2) {
        assert!(
            pair[1].1 >= pair[0].0,
            "前ターンの入力を cache から再利用する: {usage:?}"
        );
    }
    assert_eq!(mock.remaining_scripts(), 0);
}

// 開始時 family の snapshot がある場合、設定上の binding を変更して復元しても
// delegate の追記を再選択しない。これは B の意図した挙動変更である。
#[tokio::test]
async fn stored_initial_family_is_intentionally_frozen_across_restore() {
    assert_restored_family(true).await;
}

// 導入前の record は現在設定の説明 override を選ばず、従来どおり family を計算する。
// 最初の復元後は tools と prefix が固定され、入力由来 mock cache を再利用する。
#[tokio::test]
async fn pre_snapshot_chat_recomputes_family_once_then_stays_fixed() {
    assert_restored_family(false).await;
}

async fn assert_restored_family(has_snapshot: bool) {
    let review_storage = support::ReviewStorage::new();
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            reply(0, LEGACY_MODEL),
            reply(1, GPT6_MODEL),
            reply(2, GPT6_MODEL),
        ],
        vec![LEGACY_MODEL.into(), GPT6_MODEL.into()],
    );
    let directory = tempfile::tempdir().unwrap();
    let storage_config = StorageConfig {
        db_path: directory.path().join("restore.db"),
        ..Default::default()
    };
    let storage = Storage::open(storage_config.clone()).unwrap();
    // 保存側: family を持たないモデルでの chat。
    let legacy_config = load_config(directory.path(), &mock, "legacy");
    let (legacy, mut legacy_events) =
        build_runtime(directory.path(), &legacy_config, &storage_config, &storage);
    review_storage.attach(&legacy);
    let prior = legacy.delegate_background(
        Role::Orchestrator,
        "prior request".into(),
        RunConfig {
            name: Some(format!("chat:{}:thread", Role::Orchestrator.name())),
            ..Default::default()
        },
    );
    through(&mut legacy_events, prior, AgentRunPhase::Done).await;
    legacy.wait(prior).await.unwrap();
    let mut record = legacy
        .with_storage_reader(|db| db.run_context(&prior.to_string()))
        .unwrap()
        .flatten()
        .unwrap();
    drop(legacy);
    let handle = storage.handle();
    let mut descriptor: Value = serde_json::from_str(&record.config_json).unwrap();
    assert!(descriptor.get("tool_descriptions").is_some());
    if !has_snapshot {
        // 導入前 build の record を再現する。稼働中の writer は上の wait で完了済み。
        descriptor
            .as_object_mut()
            .unwrap()
            .remove("tool_descriptions");
        record.config_json = descriptor.to_string();
        handle.upsert_run_context(&record).unwrap();
    }

    // 復元側: gpt-6.1-sol。再起動後の runtime で 2 回続けて復元する。
    let config = load_config(directory.path(), &mock, "gpt6");
    let (runtime, mut events) = build_runtime(directory.path(), &config, &storage_config, &storage);
    review_storage.attach(&runtime);
    let mut usage = Vec::new();
    for message in ["followup", "followup again"] {
        let run = runtime
            .delegate_chat(
                "thread",
                Role::Orchestrator,
                message.into(),
                RunConfig::default(),
            )
            .unwrap();
        usage.extend(through(&mut events, run, AgentRunPhase::Done).await);
    }

    let requests = chat_requests(&mock);
    assert_eq!(requests.len(), 3);
    let (before, first, second) = (&requests[0], &requests[1], &requests[2]);
    // 旧説明: note はない。
    assert_eq!(
        delegate_description(before)
            .matches(SUBAGENT_CONTEXT_NOTE)
            .count(),
        0
    );
    assert_eq!(tool_names(first), tool_names(before));
    if has_snapshot {
        // 初期 family が保存されていれば、新しい family の追記を足さない。
        assert_eq!(tools_of(first), tools_of(before));
    } else {
        // 旧 record のみ、delegate の追記 1 回分以外は変更しない。
        assert_eq!(
            delegate_description(first),
            format!("{}{SUBAGENT_CONTEXT_NOTE}", delegate_description(before))
        );
        for (old, new) in tools_of(before).iter().zip(tools_of(first)) {
            if old["function"]["name"] != "delegate" {
                assert_eq!(old, new);
            }
        }
    }
    // 保存済みの System は再合成されない (gpt-5 以降の family section が復元後も入らない)。
    for request in &requests {
        assert_eq!(system_of(request), system_of(before));
    }
    assert!(
        !system_of(first)["content"]
            .to_string()
            .contains(FAMILY_MARK)
    );
    // いずれの record も、復元後の tools と prefix は固定され cache を再利用する。
    assert_eq!(tools_of(second), tools_of(first));
    assert_eq!(
        delegate_description(second)
            .matches(SUBAGENT_CONTEXT_NOTE)
            .count(),
        usize::from(!has_snapshot)
    );
    assert_append_only(CacheProtocol::OpenAi, first, second).unwrap();
    assert_eq!(usage.len(), 2);
    assert!(
        usage[1].1 >= usage[0].0,
        "境界後の prefix は再利用される: {usage:?}"
    );
    assert_eq!(mock.remaining_scripts(), 0);
}
