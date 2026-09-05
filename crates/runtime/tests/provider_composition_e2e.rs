mod support;

use std::sync::Arc;
use std::time::Duration;

use config::{Config, LoadOptions};
use event_bus::{AgentRunPhase, EventBus, EventKind, MessageEvent, ProviderEvent, ToolEvent};
use routing::MapEnv;
use runtime::{
    CompositionError, ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime,
};
use sandbox::DirectSandbox;
use sandbox::credential::{CredentialStore, FileCredentialStore};
use serde_json::json;
use tools::ToolExecutor;

use support::drain_events;
use support::mock_openai::RecordingMockOpenAi;

const KEY_ENV: &str = "EVORCH_TEST_KEY_COMPOSITION_E2E";
const KEY: &str = "composition-e2e-key";
const MODEL: &str = "local-model";

fn openai_tool_response(id: &str, name: &str, arguments: serde_json::Value) -> String {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments.to_string() }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
    })
    .to_string()
}

fn openai_text_response(text: &str) -> String {
    json!({
        "choices": [{
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
    })
    .to_string()
}

fn load_config(root: &std::path::Path, base_url: &str) -> Config {
    std::fs::write(
        root.join("evorch.toml"),
        format!(
            r#"[providers.local]
type = "openai-compatible"
base_url = "{base_url}"
api_key_env = "{KEY_ENV}"
models = ["{MODEL}"]
default_model = "{MODEL}"
"#
        ),
    )
    .expect("write config");
    Config::load(&LoadOptions {
        project_dir: Some(root.to_path_buf()),
        user_config_dir: Some(root.join("empty-user-config")),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect("load config")
}

fn credential_store(root: &std::path::Path) -> Arc<dyn CredentialStore> {
    Arc::new(FileCredentialStore::open(root.join("credentials")).expect("credential store"))
}

fn composition<'a>(
    config: &'a Config,
    bus: Arc<EventBus>,
    root: &std::path::Path,
    env: MapEnv,
) -> RuntimeComposition<'a> {
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    RuntimeComposition {
        config,
        bus,
        executor,
        credential_store: credential_store(root),
        env: Arc::new(env),
        model_source: ModelSource::Configured,
        workspace: None,
    }
}

// Given: sugar provider config、MapEnv credential、4 応答の local OpenAI mock
// When: Orchestrator が blocking delegate で Worker を起動し Worker が edit する
// Then: 両 run、result、provider/tool events、disk、全 wire request が同じ composition を通る
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_runtime_runs_blocking_delegate_and_worker_edit_end_to_end() {
    let directory = tempfile::tempdir().expect("project directory");
    let edited = directory.path().join("worker-output.txt");
    let mock = RecordingMockOpenAi::spawn(vec![
        openai_tool_response(
            "delegate-1",
            "delegate",
            json!({ "role": "worker", "prompt": "WORKER-EDIT" }),
        ),
        openai_tool_response(
            "edit-1",
            "edit",
            json!({ "path": edited, "new_string": "written by worker" }),
        ),
        openai_text_response("worker final text"),
        openai_text_response("orchestrator final text"),
    ]);
    let config = load_config(directory.path(), &mock.base_url());
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let composed = compose_runtime(composition(
        &config,
        Arc::clone(&bus),
        directory.path(),
        MapEnv::from_iter([(KEY_ENV, KEY)]),
    ))
    .expect("configured runtime");

    let root = composed.runtime.delegate_background(
        Role::Orchestrator,
        "ORCHESTRATOR-DELEGATE".to_string(),
        RunConfig::default(),
    );
    let root_phase = tokio::time::timeout(Duration::from_secs(5), composed.runtime.wait(root))
        .await
        .expect("root timeout")
        .expect("root exists");

    assert_eq!(root_phase, AgentRunPhase::Done);
    let agents = composed.runtime.list_agents();
    let worker = agents
        .iter()
        .find(|agent| agent.role_name == Role::Worker.name())
        .expect("worker run");
    assert_eq!(worker.phase, AgentRunPhase::Done);
    assert_eq!(
        composed.runtime.run_result(worker.run_id),
        Ok(Some("worker final text".to_string()))
    );
    assert_eq!(
        std::fs::read_to_string(&edited).expect("edited file"),
        "written by worker"
    );

    let drained = drain_events(&mut events).await;
    assert!(drained.iter().any(|event| matches!(
        &event.kind,
        EventKind::Provider(ProviderEvent::RequestCompleted { profile, .. })
            if profile.as_deref() == Some("local")
    )));
    assert!(drained.iter().any(|event| matches!(
        &event.kind,
        EventKind::Tool(ToolEvent::ToolCompleted { tool_name, is_error: false, .. })
            if tool_name == "edit"
    )));
    let message_deltas = drained
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Message(MessageEvent::MessageDelta { delta }) => Some(delta.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(message_deltas.contains(&"worker final text"));
    assert!(message_deltas.contains(&"orchestrator final text"));

    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    let initial_prompts = requests
        .iter()
        .map(|request| request.body["messages"][0]["content"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        initial_prompts,
        [
            Some("ORCHESTRATOR-DELEGATE"),
            Some("WORKER-EDIT"),
            Some("WORKER-EDIT"),
            Some("ORCHESTRATOR-DELEGATE")
        ]
    );
    for request in requests {
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer composition-e2e-key")
        );
        assert_eq!(request.body["model"], MODEL);
    }
}

// Given: 同じ sugar provider config と credential を含まない MapEnv
// When: configured runtime を compose する
// Then: run 開始前に MissingCredential で失敗し mock request は 0 件のまま
#[test]
fn configured_runtime_fails_before_run_when_credential_is_missing() {
    let directory = tempfile::tempdir().expect("project directory");
    let mock = RecordingMockOpenAi::spawn(Vec::new());
    let config = load_config(directory.path(), &mock.base_url());
    let bus = Arc::new(EventBus::new(32));

    let error = match compose_runtime(composition(
        &config,
        bus,
        directory.path(),
        MapEnv::default(),
    )) {
        Ok(_) => panic!("missing credential must fail"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        CompositionError::Routing(routing::RoutingError::MissingCredential { profile, var })
            if profile == "local" && var == KEY_ENV
    ));
    assert_eq!(mock.request_count(), 0);
}
