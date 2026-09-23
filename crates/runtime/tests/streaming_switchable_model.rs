use std::sync::Arc;
use std::time::Duration;

use config::{Config, LoadOptions};
use event_bus::{Event, EventBus, EventKind, MessageEvent};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use routing::{ComposeDeps, MapEnv};
use runtime::compose::{SwitchableModel, compose_routed_model};
use runtime::{AgentInvocationContext, AgentModel, Role};
use sandbox::credential::FileCredentialStore;

/// GUI production runs lose streaming at the hot-swappable model boundary.
///
/// GUI startup wraps the production model (`gui/src/bin/evorch-gui.rs:601-615`).
/// Chat submits Worker runs (`gui/src/runtime_sink.rs:439-452`); team goals
/// select Orchestrator (`runtime_sink.rs:351-355`). Both reach the agent loop.
/// `compose/live.rs:42-62` implements only `SwitchableModel::complete`, so an
/// agent-loop streaming call (`agent_loop.rs:634-640`) enters the deferred default
/// (`model.rs:63-94`). That calls the wrapper's `complete` (`live.rs:50-52`),
/// then `RoutedModel::complete` (`compose.rs:346`) passes `None` instead of the
/// bus: `compose.rs:315` invokes provider `send`, not `send_streaming`.
/// OpenAI-compatible requests therefore have `stream: false`; the entire text
/// is emitted as one delta only after the HTTP response completes. Codex's
/// `send` similarly consumes its SSE internally without publishing text deltas.
/// This affects both Orchestrator and Worker, not provider capability selection:
/// the identical unwrapped RoutedModel preserves both wire streaming and deltas.
/// Existing configured-runtime SSE tests bypass this GUI wrapper.
/// GUI repaint is requested per event (`gui/src/events/mod.rs:23-36`), but
/// cannot display text that the wrapper has not yet emitted.
///
/// The fixture serves the same two fragments as SSE or full JSON according to
/// the actual request flag. No sleeps or scheduler-dependent timing assertions
/// are needed to distinguish real streaming from the deferred fallback.
#[tokio::test]
async fn streaming_preserves_provider_deltas_when_routed_model_is_switchable() {
    // Given: the same configured provider, response and bus for both model paths.
    let directory = tempfile::tempdir().expect("project");
    let mock = StreamingMockOpenAi::spawn(
        (0..4)
            .map(|_| ScriptedResponse::text_stream("reply", "gpt-4o", ["first ", "second"]))
            .collect(),
    );
    std::fs::write(
        directory.path().join("evorch.toml"),
        format!(
            r#"[providers.local]
type = "openai-compatible"
base_url = "{}"
api_key_env = "T4_STREAMING_KEY"
models = ["gpt-4o"]
default_model = "gpt-4o"
"#,
            mock.base_url()
        ),
    )
    .expect("config fixture");
    let config = Config::load(&LoadOptions {
        project_dir: Some(directory.path().to_path_buf()),
        user_config_dir: Some(directory.path().join("empty-user-config")),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect("config");
    let bus = Arc::new(EventBus::new(64));
    let routed: Arc<dyn AgentModel> = compose_routed_model(
        &config,
        ComposeDeps {
            credential_store: Arc::new(
                FileCredentialStore::open(directory.path().join("credentials"))
                    .expect("credentials"),
            ),
            event_bus: Some(Arc::clone(&bus)),
            env: Arc::new(MapEnv::from_iter([("T4_STREAMING_KEY", "fixture-key")])),
            catalog: model::ModelCatalog::new(),
            factory: routing::factory::FactoryOptions::default(),
        },
    )
    .expect("routed model");
    let switchable: Arc<dyn AgentModel> = Arc::new(SwitchableModel::new(Arc::clone(&routed)));
    let mut observed = Vec::new();

    // When: invoke the production streaming interface with and without wrapping.
    for role in [Role::Orchestrator, Role::Worker] {
        for (path, model) in [("direct", &routed), ("switchable", &switchable)] {
            let invocation = AgentInvocationContext {
                run_id: format!("{role:?}-{path}"),
                ..AgentInvocationContext::default()
            };
            let mut receiver = bus.subscribe();
            tokio::time::timeout(
                Duration::from_secs(5),
                model.complete_streaming(&invocation, role, &[], &[], &bus),
            )
            .await
            .expect("HTTP completion timeout")
            .expect("model completion");
            bus.emit(Event::new(MessageEvent::MessageDelta {
                delta: String::new(),
                run_id: Some(invocation.run_id.clone()),
            }));
            let mut deltas = Vec::new();
            loop {
                match receiver.recv().await.expect("bus event").kind {
                    EventKind::Message(MessageEvent::MessageDelta { delta, run_id })
                        if run_id.as_deref() == Some(invocation.run_id.as_str()) =>
                    {
                        if delta.is_empty() {
                            break;
                        }
                        deltas.push(delta);
                    }
                    _ => {}
                }
            }
            observed.push((role, path, deltas));
        }
    }

    // Then: the wrapper must preserve the direct path's actual SSE request.
    let wire_streaming: Vec<_> = mock
        .recorded_requests()
        .iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| request.stream)
        .collect();
    assert_eq!(
        wire_streaming,
        vec![true; 4],
        "SwitchableModel must preserve streaming for both roles; bus deltas: {observed:?}"
    );
    for (role, path, deltas) in observed {
        assert_eq!(deltas, ["first ", "second"], "{role:?}/{path}");
    }
}
