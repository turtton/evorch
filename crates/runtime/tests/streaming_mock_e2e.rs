//! Configured runtime E2E against the shared streaming-capable OpenAI mock.
//!
//! The runtime uses `complete()`, sending `stream: false`: these tests exercise
//! the mock's JSON mode, including argument/text fragment reassembly. SSE mode
//! is covered separately by client-level tests.

mod support;

use std::sync::Arc;
use std::time::Duration;

use config::{Config, LoadOptions};
use event_bus::{AgentRunPhase, EventBus, EventKind, MessageEvent, ProviderEvent, ToolEvent};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::MapEnv;
use runtime::{ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use sandbox::DirectSandbox;
use sandbox::credential::{CredentialStore, FileCredentialStore};
use serde_json::json;
use tools::ToolExecutor;

use support::drain_events;

const KEY_ENV: &str = "EVORCH_TEST_KEY_STREAMING_MOCK_E2E";
const KEY: &str = "streaming-mock-e2e-key";
const MODEL: &str = "local-model";

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

// Keep the composition precedent's four inputs explicit: config, bus, project,
// and injected credentials are independent harness dependencies.
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
    let credential_store: Arc<dyn CredentialStore> =
        Arc::new(FileCredentialStore::open(root.join("credentials")).expect("credential store"));
    RuntimeComposition {
        config,
        bus,
        executor,
        credential_store,
        env: Arc::new(env),
        model_source: ModelSource::Configured,
        workspace: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_run_with_tool_call_completes_over_mock() {
    // Given: configured credentials and a three-fragment edit followed by text.
    let directory = tempfile::tempdir().expect("project directory");
    let edited = directory.path().join("worker-output.txt");
    let path_chunk = format!("{{\"path\":{},", json!(edited));
    let mock = StreamingMockOpenAi::spawn(vec![
        ScriptedResponse::tool_call(
            "chatcmpl-1",
            MODEL,
            0,
            "call_1",
            "edit",
            [
                path_chunk.as_str(),
                "\"new_string\":\"written by ",
                "streaming mock e2e\"}",
            ],
        ),
        ScriptedResponse::text_stream("chatcmpl-2", MODEL, ["All ", "done."]),
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

    // When: a Worker executes through the real provider and standard edit tool.
    let run_id = composed.runtime.delegate_background(
        Role::Worker,
        "Write the requested file.".to_string(),
        RunConfig::default(),
    );
    let phase = tokio::time::timeout(Duration::from_secs(5), composed.runtime.wait(run_id))
        .await
        .expect("worker timeout");

    // Then: completion, correlated events, file contents and wire history agree.
    assert_eq!(phase, Ok(AgentRunPhase::Done));
    assert!(
        std::fs::read_to_string(&edited)
            .expect("edited file")
            .contains("written by streaming mock e2e")
    );
    let events = drain_events(&mut events).await;
    let expected_run_id = run_id.to_string();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.kind,
                EventKind::Provider(ProviderEvent::RequestStarted { profile, run_id, .. })
                    if profile.as_deref() == Some("local")
                        && run_id.as_deref() == Some(expected_run_id.as_str())
            ))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.kind,
                EventKind::Provider(ProviderEvent::RequestCompleted { profile, run_id, .. })
                    if profile.as_deref() == Some("local")
                        && run_id.as_deref() == Some(expected_run_id.as_str())
            ))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.kind,
                EventKind::Tool(ToolEvent::ToolStarted { tool_name, run_id, .. })
                    if tool_name == "edit" && run_id.as_deref() == Some(expected_run_id.as_str())
            ))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.kind,
                EventKind::Tool(ToolEvent::ToolCompleted { tool_name, run_id, is_error: false, .. })
                    if tool_name == "edit" && run_id.as_deref() == Some(expected_run_id.as_str())
            ))
            .count(),
        1
    );
    let message_deltas = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Message(MessageEvent::MessageDelta { delta, run_id })
                if run_id.as_deref() == Some(expected_run_id.as_str()) =>
            {
                Some(delta.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(!message_deltas.is_empty());
    assert_eq!(message_deltas.concat(), "All done.");

    let requests = mock.recorded_requests();
    assert_eq!(requests.len(), 2);
    let authorization = format!("Bearer {KEY}");
    for request in &requests {
        assert_eq!(
            request.authorization.as_deref(),
            Some(authorization.as_str())
        );
        assert_eq!(request.body["model"], MODEL);
        assert!(!request.stream);
        assert_eq!(request.body["stream"], false);
    }
    assert!(
        requests[1].body["messages"]
            .as_array()
            .expect("message history")
            .iter()
            .any(|message| message["role"] == "tool" && message["tool_call_id"] == "call_1")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unscripted_request_fails_run_not_hangs() {
    // Given: a configured runtime whose mock has no scripted response.
    let directory = tempfile::tempdir().expect("project directory");
    let mock = StreamingMockOpenAi::spawn(Vec::new());
    let config = load_config(directory.path(), &mock.base_url());
    let bus = Arc::new(EventBus::new(256));
    let composed = compose_runtime(composition(
        &config,
        bus,
        directory.path(),
        MapEnv::from_iter([(KEY_ENV, KEY)]),
    ))
    .expect("configured runtime");

    // When: the Worker receives the mock's unscripted-request HTTP error.
    let run_id = composed.runtime.delegate_background(
        Role::Worker,
        "Request an unscripted completion.".to_string(),
        RunConfig::default(),
    );
    let phase = tokio::time::timeout(Duration::from_secs(5), composed.runtime.wait(run_id))
        .await
        .expect("unscripted request must terminate within five seconds");

    // Then: Error is the runtime's terminal failure phase, not a wait error.
    assert_eq!(phase, Ok(AgentRunPhase::Error));
    assert!(!mock.recorded_requests().is_empty());
}
