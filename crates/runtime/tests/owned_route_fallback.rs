//! Delegate fallback must preserve the open ownership turn across ModelSwitch.
mod support;

use std::sync::Arc;

use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent, ProviderEvent};
use mock_openai::{
    ScriptedResponse, StreamingMockOpenAi,
    cache_contract::{CacheProtocol, assert_append_only},
};
use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
use runtime::{AgentRuntime, ModelPreference, Role, RunConfig, RunStore};
use serde_json::json;
use tokio::time::{Duration, timeout};
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};

struct Read;

#[async_trait::async_trait]
impl Tool for Read {
    fn name(&self) -> &'static str {
        "read"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type":"object", "properties":{}})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    async fn execute(&self, _: serde_json::Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success("read result"))
    }
}

fn read(id: &str) -> ScriptedResponse {
    ScriptedResponse::tool_call(id, "mock-model", 0, id, "read", ["{}"])
}

async fn owned_fallback(as_child: bool) {
    let review_storage = support::ReviewStorage::new();
    // One successful turn pins the primary. Its exhausted script then returns HTTP 500,
    // forcing a ModelSwitch round trip rather than the unpinned, in-request fallback.
    let primary = StreamingMockOpenAi::spawn(vec![read("primary-read")]);
    let secondary = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        read("secondary-read"),
        ScriptedResponse::text_stream("done", "mock-model", ["recovered"]),
    ]);
    let parent_model = StreamingMockOpenAi::spawn(vec![
        ScriptedResponse::text_stream("parent", "mock-model", ["waiting for the child"]),
        ScriptedResponse::text_stream("noted", "mock-model", ["child result received"]),
    ]);
    let directory = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new(512));
    let mut receiver = bus.subscribe();
    let mut config = config::Config::default();
    for (name, server) in [
        ("primary", &primary),
        ("secondary", &secondary),
        ("parent", &parent_model),
    ] {
        config.providers.insert(
            name.into(),
            config::ProviderProfileConfig {
                provider_type: config::ProviderTypeConfig::OpenAiCompatible,
                api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
                base_url: server.base_url(),
                credential: config::CredentialRefConfig::Env { var: "KEY".into() },
                models: vec![config::types::provider::ModelEntryConfig::enabled(
                    "mock-model",
                )],
                default_model: "mock-model".into(),
                ..Default::default()
            },
        );
    }
    config.routing.routes.insert(
        "worker".into(),
        ["primary", "secondary"]
            .map(|name| config::RouteCandidateConfig {
                profile: name.into(),
                model: None,
                auto: false,
            })
            .to_vec(),
    );
    let model = runtime::compose::compose_routed_model(
        &config,
        routing::ComposeDeps {
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(
                    directory.path().join("credentials"),
                )
                .unwrap(),
            ),
            event_bus: Some(bus.clone()),
            env: Arc::new(routing::MapEnv::from_iter([("KEY", "fixture-key")])),
            catalog: model::ModelCatalog::new(),
            factory: Default::default(),
        },
    )
    .unwrap();
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(Read)).unwrap();
    let storage_config = storage::StorageConfig {
        db_path: directory.path().join("runs.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model)
        .with_run_store(RunStore::open(&storage_config, storage.handle()).unwrap());
    review_storage.attach(&runtime);
    let path = directory.path().join("owners.db");
    let mut registry = Registry::open(&path).unwrap();
    let owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "test-owner".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    registry.start(&owner).unwrap();
    let permit = OwnerPermit {
        registry_path: path,
        thread_id: owner.thread_id.clone(),
        lease: owner.lease,
        run_id: None,
    };
    let parent = if as_child {
        let parent = runtime.delegate_background(
            Role::Orchestrator,
            "parent".into(),
            RunConfig {
                ownership: Some(permit.clone()),
                interactive: true,
                model_preference: Some(ModelPreference {
                    profile: "parent".into(),
                    model: None,
                    reasoning_effort: None,
                }),
                ..Default::default()
            },
        );
        timeout(Duration::from_secs(10), async {
            loop {
                if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    run_id, to, ..
                }) = receiver.recv().await.unwrap().kind
                    && run_id == parent.to_string()
                    && matches!(
                        to,
                        AgentRunPhase::Waiting | AgentRunPhase::Done | AgentRunPhase::Error
                    )
                {
                    assert_eq!(to, AgentRunPhase::Waiting);
                    break;
                }
            }
        })
        .await
        .unwrap();
        Some(parent)
    } else {
        None
    };
    let run = if let Some(parent) = parent {
        // No explicit model preference: inherit ownership, not the parent's selection.
        runtime
            .delegate_background_as_child(parent, Role::Worker, "child", RunConfig::default())
            .unwrap()
    } else {
        runtime.delegate_background(
            Role::Worker,
            "root".into(),
            RunConfig {
                ownership: Some(permit),
                ..Default::default()
            },
        )
    };
    let phase = timeout(Duration::from_secs(10), runtime.wait(run))
        .await
        .unwrap()
        .unwrap();
    // The completion relay can wake the parent; it owns a separate turn that the
    // child's checkpoint must not close. Drain it before checking the whole thread.
    let owner = registry.attach("thread").unwrap();
    assert!(!owner.active_runs.contains(&run.to_string()));
    if let Some(parent) = parent {
        runtime.cancel(parent).unwrap();
        timeout(Duration::from_secs(10), runtime.wait(parent))
            .await
            .unwrap()
            .unwrap();
    }
    let mut fallback = None;
    let mut fallback_usage = Vec::new();
    let reason = timeout(Duration::from_secs(2), async {
        loop {
            match receiver.recv().await.unwrap().kind {
                EventKind::Provider(ProviderEvent::FallbackTriggered {
                    session_id,
                    from_provider,
                    to_provider,
                    ..
                }) if session_id == run.to_string() => {
                    fallback = Some((from_provider, to_provider));
                }
                EventKind::Provider(ProviderEvent::RequestCompleted {
                    profile,
                    input_tokens,
                    cache_read_tokens,
                    ..
                }) if profile.as_deref() == Some("secondary") => {
                    fallback_usage.push((input_tokens, cache_read_tokens));
                }
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    run_id,
                    to,
                    reason,
                    ..
                }) if run_id == run.to_string() && to == phase => break reason,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(phase, AgentRunPhase::Done, "{reason:?}");
    assert_eq!(fallback, Some(("primary".into(), "secondary".into())));
    assert_eq!(secondary.remaining_scripts(), 0);
    let requests: Vec<_> = secondary
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| request.body)
        .collect();
    assert_eq!(requests.len(), 2);
    assert_append_only(CacheProtocol::OpenAi, &requests[0], &requests[1]).unwrap();
    assert_eq!(fallback_usage.len(), 2);
    assert_eq!(fallback_usage[0].1, 0);
    assert!(fallback_usage[1].1 >= fallback_usage[0].0);
    let owner = registry.attach("thread").unwrap();
    assert!(!owner.active_turn);
    assert!(owner.active_runs.is_empty());
    let record = storage::Database::open(&storage_config)
        .unwrap()
        .run_context(&run.to_string())
        .unwrap()
        .unwrap();
    assert!(record.messages_json.contains("recovered"));
    assert_eq!(record.parent_run_id, parent.map(|run| run.to_string()));
}

#[tokio::test]
async fn delegated_run_keeps_ownership_turn_open_during_fallback() {
    owned_fallback(true).await;
}

#[tokio::test]
async fn root_run_keeps_ownership_turn_open_during_fallback() {
    owned_fallback(false).await;
}
