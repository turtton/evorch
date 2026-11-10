//! ADR 0043 cache contract: watching tool output for pull requests, and the events that follow,
//! never touch what a model is sent. The observer reads events after the fact; it adds no tool,
//! rewrites no result and writes nothing into any history. A run observed by the pull request
//! service sends byte-for-byte the requests an unobserved run sends, and its provider wire
//! prefix stays append-only.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::{EventBus, EventKind, EventReceiver, PullRequestEvent};
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::pull_requests::{
    FakePullRequestSource, PullRequestService, ServiceParts, SystemWallClock, ThreadEnvironment,
};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RuntimeError};
use serde_json::{Value, json};
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};

const THREAD: &str = "thread-1";

/// A shell that "creates a pull request": its output names one, and it publishes a pull
/// request event while it runs, as the service would.
struct CreatePullRequest {
    bus: Arc<EventBus>,
}

#[async_trait]
impl Tool for CreatePullRequest {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn schema(&self) -> Value {
        json!({"type":"object", "required":["number"], "properties":{"number":{"type":"integer"}}})
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }

    async fn execute(&self, input: Value) -> Result<ToolResult, ToolError> {
        let number = input["number"].as_u64().unwrap();
        self.bus
            .emit(event_bus::Event::new(PullRequestEvent::Changed {
                thread_id: THREAD.into(),
                link_revision: 1,
                transition_seq: number,
                pull_request: event_bus::PullRequestRef {
                    provider: "github".into(),
                    host: "github.com".into(),
                    repository: "turtton/evorch".into(),
                    number,
                },
                fields: vec![event_bus::PullRequestChangedField::Checks],
                notifications: vec!["checks_failed".into()],
            }));
        Ok(ToolResult::success(format!(
            "Creating pull request for feat into main\n\nhttps://github.com/turtton/evorch/pull/{number}\n"
        )))
    }
}

struct EveryRunIsInOneThread;

impl ThreadEnvironment for EveryRunIsInOneThread {
    fn thread_of_run(&self, _run_id: &str) -> Option<String> {
        Some(THREAD.into())
    }

    fn checkout_directories(&self, _thread_id: &str) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}

/// Records every request's messages and offered tools; replies from a script.
struct Recording {
    script: Mutex<VecDeque<ChatResponse>>,
    requests: Mutex<Vec<(Vec<Message>, Vec<ToolSpec>)>>,
}

impl Recording {
    fn new(script: Vec<ChatResponse>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            requests: Mutex::default(),
        })
    }

    fn requests(&self) -> Vec<(Vec<Message>, Vec<ToolSpec>)> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl AgentModel for Recording {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.requests
            .lock()
            .unwrap()
            .push((messages.to_vec(), tools.to_vec()));
        Ok(self.script.lock().unwrap().pop_front().unwrap_or_else(|| {
            response(
                vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                FinishReason::Stop,
            )
        }))
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "pull-request-observer-cache".into()
    }
}

fn response(content: Vec<ContentBlock>, finish_reason: FinishReason) -> ChatResponse {
    ChatResponse {
        usage_reported: true,
        message: Message {
            role: MessageRole::Assistant,
            content,
        },
        finish_reason,
        usage: Usage::default(),
    }
}

fn call(id: &str, number: u64) -> ChatResponse {
    response(
        vec![ContentBlock::ToolUse {
            id: id.into(),
            name: "shell".into(),
            input: json!({ "number": number }),
        }],
        FinishReason::ToolUse,
    )
}

/// A runtime whose executor offers [`CreatePullRequest`], optionally watched by the service.
struct Fixture {
    runtime: AgentRuntime,
    events: EventReceiver,
    storage: storage::Storage,
    _service: Option<PullRequestService>,
    _owner: Arc<runtime::ownership::OwnerHost>,
    _directory: tempfile::TempDir,
}

fn fixture(model: Arc<dyn AgentModel>, observed: bool) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let storage = storage::Storage::open(storage::StorageConfig {
        db_path: directory.path().join("observer.db"),
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(4096));
    let owner = Arc::new(
        runtime::ownership::OwnerHost::open(
            &directory.path().join("owners"),
            Default::default(),
            bus.clone(),
        )
        .unwrap(),
    );
    owner.start(THREAD).unwrap();
    let events = bus.subscribe();
    let mut executor = ToolExecutor::new(bus.clone());
    executor
        .register(Arc::new(CreatePullRequest { bus: bus.clone() }))
        .unwrap();
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(executor), model);
    let service = observed.then(|| {
        PullRequestService::start(ServiceParts {
            storage: storage.handle(),
            source: Arc::new(FakePullRequestSource::new()),
            ownership: owner.clone(),
            environment: Arc::new(EveryRunIsInOneThread),
            bus,
            settings: config::PullRequestsConfig {
                enabled: true,
                ..Default::default()
            },
            clock: Arc::new(SystemWallClock),
        })
        .unwrap()
    });
    Fixture {
        runtime,
        events,
        storage,
        _service: service,
        _owner: owner,
        _directory: directory,
    }
}

async fn run_to_end(fixture: &Fixture) {
    let run = fixture.runtime.delegate_background(
        Role::Worker,
        "open the pull requests".into(),
        RunConfig::default(),
    );
    assert_eq!(
        fixture.runtime.wait(run).await,
        Ok(event_bus::AgentRunPhase::Done)
    );
}

/// Waits for the observer to have offered `count` candidates, by their events.
async fn offered(fixture: &mut Fixture, count: usize) {
    let mut seen = 0;
    while seen < count {
        let event = fixture.events.recv().await.unwrap();
        if matches!(
            event.kind,
            EventKind::PullRequest(PullRequestEvent::CandidateChanged { .. })
        ) {
            seen += 1;
        }
    }
}

#[tokio::test]
async fn an_observed_run_sends_the_requests_an_unobserved_run_sends() {
    // Given: the same two pull requests created in two runs, one of them watched.
    let script = || vec![call("c1", 11), call("c2", 12)];
    let plain_model = Recording::new(script());
    let plain = fixture(plain_model.clone(), false);
    let watched_model = Recording::new(script());
    let mut watched = fixture(watched_model.clone(), true);

    // When: both run to the end, and the observer has offered the pull requests.
    run_to_end(&plain).await;
    run_to_end(&watched).await;
    offered(&mut watched, 2).await;

    // Then: every request is byte-identical: no tool result, history or tool list changed.
    let (plain_requests, watched_requests) = (plain_model.requests(), watched_model.requests());
    assert_eq!(plain_requests.len(), 3);
    assert_eq!(
        serde_json::to_string(&watched_requests).unwrap(),
        serde_json::to_string(&plain_requests).unwrap()
    );
    for pair in watched_requests.windows(2) {
        assert!(
            pair[1].0.starts_with(&pair[0].0),
            "a sent message was rewritten"
        );
    }
    let tool_output = watched_requests[1]
        .0
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(
                content
                    .iter()
                    .map(|part| part.text_or_image_note().into_owned())
                    .collect::<String>(),
            ),
            _ => None,
        })
        .unwrap();
    assert!(
        tool_output.contains("https://github.com/turtton/evorch/pull/11"),
        "{tool_output}"
    );
    assert!(!tool_output.contains("candidate"), "{tool_output}");
    let candidates = watched
        .storage
        .handle()
        .pull_request_candidates(THREAD, None)
        .unwrap();
    assert_eq!(candidates.len(), 2, "the observer did its work");
}

#[tokio::test]
async fn an_observed_run_keeps_the_provider_wire_prefix_and_the_input_derived_cache() {
    use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
    use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
    const MODEL: &str = "mock-model";

    // Given: a provider mock that derives cached tokens from the input it receives.
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::tool_call(
            "s1",
            MODEL,
            0,
            "create-1",
            "shell",
            [json!({"number": 21}).to_string()],
        ),
        ScriptedResponse::tool_call(
            "s2",
            MODEL,
            0,
            "create-2",
            "shell",
            [json!({"number": 22}).to_string()],
        ),
        ScriptedResponse::text_stream("s3", MODEL, ["done"]),
    ]);
    let config = config::Config {
        providers: std::collections::BTreeMap::from([(
            "local".into(),
            config::ProviderProfileConfig {
                provider_type: config::ProviderTypeConfig::OpenAiCompatible,
                api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
                base_url: mock.base_url(),
                credential: config::CredentialRefConfig::Env {
                    var: "EVORCH_PR_OBSERVER_CACHE_KEY".into(),
                },
                models: vec![config::ModelEntryConfig::enabled(MODEL)],
                default_model: MODEL.into(),
                excluded_models: vec![],
                edit_tool: None,
            },
        )]),
        routing: config::RoutingConfig {
            routes: std::collections::BTreeMap::from([(
                "worker".into(),
                vec![config::RouteCandidateConfig {
                    profile: "local".into(),
                    model: Some(MODEL.into()),
                    auto: false,
                }],
            )]),
            ..Default::default()
        },
        compaction: config::CompactionConfig {
            context_window_tokens: 1_000_000,
            ..Default::default()
        },
        ..Default::default()
    };
    let credentials_directory = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new(4096));
    let mut provider_events = bus.subscribe();
    let credentials = sandbox::credential::FileCredentialStore::open(
        credentials_directory.path().join("credentials"),
    )
    .unwrap();
    let model = runtime::compose::compose_routed_model(
        &config,
        routing::ComposeDeps {
            credential_store: Arc::new(credentials),
            event_bus: Some(bus),
            env: Arc::new(routing::MapEnv::from_iter([(
                "EVORCH_PR_OBSERVER_CACHE_KEY",
                "offline-key",
            )])),
            catalog: model::ModelCatalog::new(),
            factory: Default::default(),
        },
        Some(test_support::model_catalog_dir()),
    )
    .unwrap();
    let mut watched = fixture(model, true);

    // When: the watched run creates two pull requests.
    run_to_end(&watched).await;
    offered(&mut watched, 2).await;

    // Then: the wire prefix only grows, and the mock reuses the whole previous prefix.
    let requests: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| request.body)
        .collect();
    assert_eq!(requests.len(), 3);
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    let mut usage = Vec::new();
    while usage.len() < 3 {
        let event = provider_events.recv().await.unwrap();
        if let EventKind::Provider(event_bus::ProviderEvent::RequestCompleted {
            input_tokens,
            cache_read_tokens,
            ..
        }) = event.kind
        {
            usage.push((input_tokens, cache_read_tokens));
        }
    }
    for pair in usage.windows(2) {
        assert!(pair[0].0 > 0);
        assert_eq!(
            pair[1].1, pair[0].0,
            "the input-derived mock must reuse the full previous prefix"
        );
    }
}
