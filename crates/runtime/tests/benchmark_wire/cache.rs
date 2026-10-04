//! Replay is a fresh-request boundary; subsequent turns must reuse its own prefix.
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use event_bus::{
    AgentRunPhase, Event, EventBus, EventKind, EventReceiver, LifecycleEvent, ProviderEvent,
};
use mock_openai::{
    ScriptedResponse, StreamingMockOpenAi,
    cache_contract::{CacheProtocol, assert_append_only},
};
use runtime::{
    AgentRuntime, ModelSource, Role, RunConfig, RunId, RuntimeComposition,
    benchmark::{BenchmarkCheckpoint, BenchmarkRecorder, BenchmarkSelector},
    snapshot::{SnapshotId, SnapshotStore},
};
use serde_json::{Value, json};

const MODEL: &str = "mock-model";

/// Exercise the executor's bounded-output path using real fixture bytes.
struct FixtureRead;
#[async_trait::async_trait]
impl tools::Tool for FixtureRead {
    fn name(&self) -> &'static str {
        "read"
    }
    fn schema(&self) -> Value {
        json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}},"additionalProperties":false})
    }
    fn permissions(&self) -> tools::Permissions {
        tools::Permissions::read_only()
    }
    async fn execute(&self, input: Value) -> Result<tools::ToolResult, tools::ToolError> {
        Ok(tools::ToolResult::success(
            std::fs::read_to_string(input["path"].as_str().unwrap()).unwrap(),
        ))
    }
}

struct Capture {
    checkpoint: Mutex<Option<BenchmarkCheckpoint>>,
    snapshot: Mutex<Option<SnapshotId>>,
    store: Mutex<SnapshotStore>,
}

#[async_trait::async_trait]
impl BenchmarkRecorder for Capture {
    async fn capture(&self, checkpoint: &BenchmarkCheckpoint) -> Result<(), String> {
        *self.snapshot.lock().unwrap() = Some(
            self.store
                .lock()
                .unwrap()
                .capture_all()
                .map_err(|error| error.to_string())?,
        );
        *self.checkpoint.lock().unwrap() = Some(checkpoint.clone());
        Ok(())
    }
}

fn compose(
    root: &Path,
    config_root: &Path,
    server: &StreamingMockOpenAi,
) -> (AgentRuntime, EventReceiver) {
    std::fs::create_dir_all(config_root.join(config::PROJECT_CONFIG_DIR)).unwrap();
    std::fs::write(
        config::project_main_config_path(config_root),
        format!(
            r#"
[providers.local]
type = "openai-compatible"
base_url = "{}"
api_key_env = "EVORCH_BENCHMARK_CACHE_KEY"
models = ["{MODEL}"]
default_model = "{MODEL}"
[[routing.routes.orchestrator]]
profile = "local"
[[routing.routes.worker]]
profile = "local"
"#,
            server.base_url()
        ),
    )
    .unwrap();
    let config = config::Config::load(&config::LoadOptions {
        project_dir: Some(config_root.into()),
        user_config_dir: Some(config_root.join("empty-user")),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let events = bus.subscribe();
    let mut executor = tools::ToolExecutor::new(bus.clone());
    executor.register(Arc::new(FixtureRead)).unwrap();
    executor.set_default_cwd(root.to_path_buf());
    let runtime = runtime::compose_runtime(RuntimeComposition {
        config: &config,
        user_config_dir: Some(config_root.join("empty-user")),
        bus,
        executor: Arc::new(executor),
        credential_store: Arc::new(
            sandbox::credential::FileCredentialStore::open(config_root.join("credentials"))
                .unwrap(),
        ),
        env: Arc::new(routing::MapEnv::from_iter([(
            "EVORCH_BENCHMARK_CACHE_KEY",
            "offline-key",
        )])),
        model_source: ModelSource::Configured,
        workspace: None,
    })
    .unwrap()
    .runtime;
    (runtime, events)
}

fn read(id: &str, path: &str) -> ScriptedResponse {
    ScriptedResponse::tool_call(id, MODEL, 0, id, "read", [json!({"path":path}).to_string()])
}

fn local_script() -> Vec<ScriptedResponse> {
    vec![
        read("large", "large.txt"),
        read("small", "small.txt"),
        ScriptedResponse::text_stream("done", MODEL, ["local result"]),
    ]
}

async fn through_terminal(events: &mut EventReceiver, run: RunId) -> Vec<Event> {
    let mut collected = Vec::new();
    loop {
        let event = events.recv().await.unwrap();
        let terminal = matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {run_id, to: AgentRunPhase::Done, ..}) if run_id == &run.to_string());
        collected.push(event);
        if terminal {
            return collected;
        }
    }
}

fn verify_local_prefix(
    server: &StreamingMockOpenAi,
    bodies: &[Value],
    events: &[Event],
    run: RunId,
    cold_start: bool,
) {
    let usage: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Provider(ProviderEvent::RequestCompleted {
                run_id: Some(id),
                input_tokens,
                cache_read_tokens,
                ..
            }) if id == &run.to_string() => Some((*input_tokens, *cache_read_tokens)),
            EventKind::Diagnostic(diagnostic) => {
                assert_ne!(diagnostic.code, "CacheRegression", "{diagnostic:?}");
                None
            }
            _ => None,
        })
        .collect();
    assert_eq!(bodies.len(), 3);
    assert_eq!(usage.len(), bodies.len());
    if cold_start {
        assert_eq!(
            usage[0].1, 0,
            "fresh replay has no previous requests in its mock cache"
        );
    }
    // OpenAI-compatible profiles currently omit this optional wire field. Both
    // its absence and a provider-supplied affinity must remain stable per run.
    let affinity = bodies[0].get("prompt_cache_key");
    assert!(
        bodies[0]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "system")
    );
    for index in 1..bodies.len() {
        // Independent wire assertions cover settings, ordered tools, system and
        // every already-sent tool result, rather than trusting reported tokens.
        assert_append_only(CacheProtocol::OpenAi, &bodies[index - 1], &bodies[index]).unwrap();
        assert_eq!(bodies[index].get("prompt_cache_key"), affinity);
        assert_eq!(
            usage[index].1,
            usage[index - 1].0,
            "input-derived cache must reuse the entire previous request"
        );
        assert!(usage[index].0 > usage[index].1 && usage[index].1 > 0);
    }
    assert!(
        bodies[1]["messages"]
            .to_string()
            .contains(".benchmark-tool-output")
    );
    assert!(
        !bodies
            .iter()
            .any(|body| body.to_string().contains("FUTURE_BASELINE_RESULT"))
    );
    assert_eq!(server.remaining_scripts(), 0);
}

fn requests(server: &StreamingMockOpenAi) -> Vec<Value> {
    server
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| request.body)
        .collect()
}

#[tokio::test]
async fn benchmark_record_and_fresh_replay_preserve_cache_after_bounded_tool_outputs() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("large.txt"),
        "日本語とASCII fixture output\n".repeat(2500),
    )
    .unwrap();
    std::fs::write(root.join("small.txt"), "second tool result").unwrap();
    let mut script = vec![ScriptedResponse::tool_call(
        "delegate",
        MODEL,
        0,
        "delegate",
        "delegate",
        [json!({"target":{"role":"worker"},"prompt":"DELEGATED_CACHE_INPUT"}).to_string()],
    )];
    script.extend(local_script());
    script.push(ScriptedResponse::text_stream(
        "whole",
        MODEL,
        ["FUTURE_BASELINE_RESULT"],
    ));
    let baseline = StreamingMockOpenAi::spawn_with_prompt_cache(script);
    let capture = Arc::new(Capture {
        checkpoint: Mutex::new(None),
        snapshot: Mutex::new(None),
        store: Mutex::new(SnapshotStore::open(&root, &directory.path().join("snapshots")).unwrap()),
    });
    let (runtime, mut events) =
        compose(&root, &directory.path().join("baseline-config"), &baseline);
    let runtime = runtime
        .with_benchmark_recorder(
            BenchmarkSelector {
                role: Role::Worker,
                category: None,
                occurrence: 1,
            },
            capture.clone(),
        )
        .unwrap();
    let run = runtime.delegate_background(
        Role::Orchestrator,
        "Delegate the fixed task".into(),
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let events = through_terminal(&mut events, run).await;
    let checkpoint = capture.checkpoint.lock().unwrap().clone().unwrap();
    let baseline_requests = requests(&baseline);
    assert_eq!(baseline_requests.len(), 5);
    let local = &baseline_requests[1..4];
    verify_local_prefix(&baseline, local, &events, checkpoint.origin_run_id, false);
    let old_artifacts = std::fs::read_dir(root.join(".benchmark-tool-output"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert!(!old_artifacts.is_empty());
    capture
        .store
        .lock()
        .unwrap()
        .restore_all(capture.snapshot.lock().unwrap().as_ref().unwrap())
        .unwrap();
    assert!(
        old_artifacts.iter().all(|path| !path.exists()),
        "future baseline artifacts must be removed with the snapshot restore"
    );
    drop(runtime);

    let candidate = StreamingMockOpenAi::spawn_with_prompt_cache(local_script());
    let (runtime, mut events) = compose(
        &root,
        &directory.path().join("candidate-config"),
        &candidate,
    );
    let replay = runtime
        .replay_benchmark(
            checkpoint.clone(),
            root.clone(),
            checkpoint.model.preference.clone(),
        )
        .unwrap();
    assert_eq!(runtime.wait(replay).await.unwrap(), AgentRunPhase::Done);
    let events = through_terminal(&mut events, replay).await;
    let candidate_requests = requests(&candidate);
    verify_local_prefix(&candidate, &candidate_requests, &events, replay, true);
    assert_ne!(
        replay, checkpoint.origin_run_id,
        "replay is a fresh run, not a continuation of the baseline child"
    );
    let mut expected = local[0].clone();
    if let Some(affinity) = candidate_requests[0].get("prompt_cache_key") {
        expected["prompt_cache_key"] = affinity.clone();
    }
    assert_eq!(
        candidate_requests[0], expected,
        "fresh A/A replay restores exact initial messages, settings and tool order"
    );
}
