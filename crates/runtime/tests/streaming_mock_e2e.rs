//! Configured runtime E2E against the shared streaming-capable OpenAI mock.
//!
//! The runtime uses `complete()`, sending `stream: false`: these tests exercise
//! the mock's JSON mode, including argument/text fragment reassembly. SSE mode
//! is covered separately by client-level tests.
//! Completion is asserted via both `wait()` and the bus lifecycle Done event.
//! Collection stops on the required-event predicate; timeouts are failsafes only.

use std::sync::Arc;
use std::time::Duration;

use config::{Config, LoadOptions};
use event_bus::{
    AgentRunPhase, EventBus, EventKind, LifecycleEvent, MessageEvent, ProviderEvent, ToolEvent,
};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::MapEnv;
use runtime::{ModelSource, Role, RunConfig, RuntimeComposition, compose_runtime};
use sandbox::DirectSandbox;
use sandbox::credential::{CredentialStore, FileCredentialStore};
use serde_json::json;
use tools::ToolExecutor;

const KEY_ENV: &str = "EVORCH_TEST_KEY_STREAMING_MOCK_E2E";
const KEY: &str = "streaming-mock-e2e-key";
const MODEL: &str = "local-model";
const MAX_RECV_ITERS: usize = 10_000;

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
    let mut receiver = bus.subscribe();
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
    let expected_run_id = run_id.to_string();
    let mut provider_started = 0;
    let mut provider_completed = 0;
    let mut tool_started = 0;
    let mut tool_completed = 0;
    let mut message_deltas = Vec::new();
    let mut lifecycle_done = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        for _ in 0..MAX_RECV_ITERS {
            let event = receiver.recv().await.expect("event loss or bus closure");
            match &event.kind {
                EventKind::Provider(ProviderEvent::RequestStarted {
                    profile, run_id, ..
                }) if profile.as_deref() == Some("local")
                    && run_id.as_deref() == Some(expected_run_id.as_str()) =>
                {
                    provider_started += 1;
                }
                EventKind::Provider(ProviderEvent::RequestCompleted {
                    profile, run_id, ..
                }) if profile.as_deref() == Some("local")
                    && run_id.as_deref() == Some(expected_run_id.as_str()) =>
                {
                    provider_completed += 1;
                }
                EventKind::Tool(ToolEvent::ToolStarted {
                    tool_name, run_id, ..
                }) if tool_name == "edit"
                    && run_id.as_deref() == Some(expected_run_id.as_str()) =>
                {
                    tool_started += 1;
                }
                EventKind::Tool(ToolEvent::ToolCompleted {
                    tool_name,
                    run_id,
                    is_error: false,
                    ..
                }) if tool_name == "edit"
                    && run_id.as_deref() == Some(expected_run_id.as_str()) =>
                {
                    tool_completed += 1;
                }
                EventKind::Message(MessageEvent::MessageDelta { delta, run_id })
                    if run_id.as_deref() == Some(expected_run_id.as_str()) =>
                {
                    message_deltas.push(delta.clone());
                }
                // event-bus/src/event.rs defines `to`; see also
                // state_transitions.rs::run_emits_pending_running_done_in_order.
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    run_id,
                    to: AgentRunPhase::Done,
                    ..
                }) if run_id == &expected_run_id => lifecycle_done = true,
                EventKind::Lifecycle(_)
                | EventKind::Provider(_)
                | EventKind::Tool(_)
                | EventKind::Message(_)
                | EventKind::Usage(_)
                | EventKind::Fault(_)
                | EventKind::AgentMessage(_)
                | EventKind::Compaction(_)
                | EventKind::Orchestrator(_)
            | EventKind::Diagnostic(_)
            | EventKind::Ownership(_) => {}
            }
            let all_required = provider_started == 2
                && provider_completed == 2
                && tool_started == 1
                && tool_completed == 1
                && !message_deltas.is_empty()
                && message_deltas.concat() == "All done."
                && lifecycle_done;
            if all_required {
                return;
            }
        }
        panic!("required events missing after {MAX_RECV_ITERS} receives");
    })
    .await
    .expect("required events missing within overall collection failsafe");
    assert_eq!(provider_started, 2);
    assert_eq!(provider_completed, 2);
    assert_eq!(tool_started, 1);
    assert_eq!(tool_completed, 1);
    assert!(
        lifecycle_done,
        "target run must emit lifecycle Done on the bus"
    );
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
