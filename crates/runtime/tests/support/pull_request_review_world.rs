//! The world the pull request review tests share: a runtime with the pull request service on
//! (or off), its mutable source and the storage, plus helpers to read the
//! escalation reviews the model was asked to make.

#![allow(dead_code, unused_imports)]

use crate::support::{ScriptedModel, text_response, tool_response};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agents::Role;
use async_trait::async_trait;
use config::{EscalationApproval, PullRequestsConfig};
use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver};
use providers::{ContentBlock, FinishReason, Message, Role as MessageRole};
use runtime::pull_requests::{
    FetchError, FetchedPullRequest, PullRequestService, PullRequestSource,
};
use runtime::{AgentRuntime, RunConfig, RunStore};
use sandbox::DirectSandbox;
use serde_json::{Value, json};
use storage::pull_requests::{
    CheckConclusion, CheckKind, CheckStatus, PullRequestCheck, PullRequestFacts, PullRequestKey,
    PullRequestState, ReviewDecision,
};
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};

pub const THREAD: &str = "thread-pr-review";
pub const REPOSITORY: &str = "turtton/evorch";
pub const DENY: &str = r#"{"approve":false,"reason":"not now","risk_level":"low","authorization_level":"none","procedure_step":"none"}"#;

pub fn key() -> PullRequestKey {
    PullRequestKey::github(REPOSITORY, 7).unwrap()
}

pub fn now_ns() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap()
}

/// What every [`answer`] title starts with, so a test can look for it in what an agent or a
/// review was sent.
pub const TITLE_PREFIX: &str = "Title for ";

/// A title for display only.
pub fn display(title: &str) -> storage::pull_requests::PullRequestDisplay {
    storage::pull_requests::PullRequestDisplay {
        title: storage::pull_requests::DisplayTitle::from_untrusted(title),
    }
}

/// The pull request as the fake source reports it: an open one at `head` repeated, one passing
/// check, fetched at `fetched_at_ns`.
pub fn answer(head: char, fetched_at_ns: i64) -> FetchedPullRequest {
    FetchedPullRequest {
        requested: key(),
        canonical: key(),
        url: format!("https://github.com/{REPOSITORY}/pull/7"),
        display: display(&format!("{TITLE_PREFIX}{head}")),
        facts: PullRequestFacts {
            state: PullRequestState::Open,
            base_branch: "main".into(),
            head_repository: Some(REPOSITORY.into()),
            head_branch: "feat".into(),
            head_sha: head.to_string().repeat(40),
            checks: vec![PullRequestCheck {
                provider_id: "ci".into(),
                kind: CheckKind::CheckRun,
                name: "ci".into(),
                status: CheckStatus::Completed,
                conclusion: Some(CheckConclusion::Success),
                head_sha: head.to_string().repeat(40),
            }],
            checks_complete: true,
            review_decision: ReviewDecision::None,
        },
        fetched_at_ns,
    }
}

/// [`answer`] with the one check failed: CI failed on `head`.
pub fn failing_answer(head: char, fetched_at_ns: i64) -> FetchedPullRequest {
    let mut failed = answer(head, fetched_at_ns);
    failed.facts.checks[0].conclusion = Some(CheckConclusion::Failure);
    failed
}

/// Reviews issue publishing shell calls, whose completion can trigger another
/// fetch even after a denied merge. Those fetches must see the current PR, not
/// consume the next test update or turn an exhausted script into NotFound.
#[derive(Clone, Default)]
pub struct ReviewSource {
    state: Arc<Mutex<SourceState>>,
    gate: Arc<SourceGate>,
}

/// Holds every fetch back while a link is being set up, and says when one has arrived.
struct SourceGate {
    closed: tokio::sync::watch::Sender<bool>,
    arrived: tokio::sync::Notify,
}

impl Default for SourceGate {
    fn default() -> Self {
        Self {
            closed: tokio::sync::watch::channel(false).0,
            arrived: tokio::sync::Notify::new(),
        }
    }
}

#[derive(Default)]
struct SourceState {
    current: Option<FetchedPullRequest>,
    fetches: usize,
}

impl ReviewSource {
    fn set(&self, current: FetchedPullRequest) {
        self.state.lock().unwrap().current = Some(current);
    }

    pub fn fetches(&self) -> usize {
        self.state.lock().unwrap().fetches
    }
}

#[async_trait]
impl PullRequestSource for ReviewSource {
    async fn fetch(
        &self,
        key: &PullRequestKey,
        _: Duration,
    ) -> Result<FetchedPullRequest, FetchError> {
        let mut closed = self.gate.closed.subscribe();
        if *closed.borrow_and_update() {
            self.gate.arrived.notify_one();
            closed.wait_for(|closed| !*closed).await.unwrap();
        }
        let mut state = self.state.lock().unwrap();
        state.fetches += 1;
        let current = state
            .current
            .clone()
            .ok_or_else(|| FetchError::NotFound("no pull request set".into()))?;
        assert_eq!(key, &current.requested);
        Ok(current)
    }
}

/// Keep setup's first poll behind the complete link operation, not merely its database write.
/// Otherwise a poll can finish before `link` registers its forced refresh, and that refresh can
/// cross a test's fetch-count baseline. This gates only setup; real review lookups and refreshes
/// still use the enabled service and the same source.
struct LinkSetup<'a>(&'a SourceGate);

impl Drop for LinkSetup<'_> {
    /// Release the held fetch even if setup panics or its future is dropped.
    fn drop(&mut self) {
        self.0.closed.send_replace(false);
    }
}

impl ReviewSource {
    fn pause_polling(&self) -> LinkSetup<'_> {
        assert!(
            !self.gate.closed.send_replace(true),
            "link setup is not reentrant"
        );
        LinkSetup(&self.gate)
    }
}

/// A runtime with the pull request service on, and its fake source.
pub struct World {
    pub bus: Arc<EventBus>,
    pub runtime: AgentRuntime,
    pub service: PullRequestService,
    pub source: ReviewSource,
    pub storage: storage::Storage,
    pub events: tokio::sync::Mutex<EventReceiver>,
    pub _directory: test_socket_dir::SplitDir,
}

pub fn world(
    model: Arc<dyn runtime::AgentModel>,
    executor: ToolExecutor,
    bus: Arc<EventBus>,
) -> World {
    world_with(
        model,
        executor,
        bus,
        PullRequestsConfig {
            enabled: true,
            ..PullRequestsConfig::default()
        },
    )
}

/// [`world`] with the given pull request settings.
pub fn world_with(
    model: Arc<dyn runtime::AgentModel>,
    executor: ToolExecutor,
    bus: Arc<EventBus>,
    settings: PullRequestsConfig,
) -> World {
    let directory = test_socket_dir::SplitDir::new();
    let storage_config = storage::StorageConfig {
        db_path: directory.path().join("review-cache.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let events = bus.subscribe();
    let executor = Arc::new(executor);
    let runtime = AgentRuntime::new(Arc::clone(&bus), Arc::clone(&executor), model)
        .with_run_store(RunStore::open(&storage_config, storage.handle()).unwrap());
    runtime.set_sandbox_escalation(EscalationApproval::Auto, false);
    executor.set_shell_escalation(
        runtime.shell_escalation_gate(),
        Arc::new(DirectSandbox::new_unchecked()),
    );
    let source = ReviewSource::default();
    let service = runtime
        .start_pull_requests(Arc::new(source.clone()), settings)
        .unwrap();
    World {
        bus: Arc::clone(&bus),
        runtime,
        service,
        source,
        storage,
        events: tokio::sync::Mutex::new(events),
        _directory: directory,
    }
}

impl World {
    pub fn db_path(&self) -> std::path::PathBuf {
        self._directory.path().join("review-cache.db")
    }

    /// Waits until the stored facts show `head`, woken by the service's own events.
    pub async fn head_is(&self, head: char) {
        let expected = head.to_string().repeat(40);
        let mut events = self.events.lock().await;
        while self.stored_head() != expected {
            let event = events.recv().await.unwrap();
            drop(event);
        }
    }

    /// Links the thread to the current PR state.
    pub async fn link(&self, first: FetchedPullRequest) {
        let setup = self.source.pause_polling();
        let head = first.facts.head_sha.chars().next().unwrap();
        self.source.set(first);
        let revision = self
            .storage
            .handle()
            .pull_request_thread_state(THREAD)
            .unwrap()
            .lifecycle_revision;
        let locator = format!("{REPOSITORY}#7");
        let mut events = self.bus.subscribe();
        let linking = self.service.link(THREAD, &locator, revision);
        tokio::pin!(linking);
        loop {
            tokio::select! {
                biased;
                event = events.recv() => {
                    if matches!(event.unwrap().kind, EventKind::PullRequest(
                        event_bus::PullRequestEvent::Linked { .. }
                    )) {
                        // Exercise the adversarial order: planning sees the saved link before
                        // the link future resumes to register its forced refresh.
                        self.service.notify_changed();
                        self.source.gate.arrived.notified().await;
                        linking.await.unwrap();
                        break;
                    }
                }
                result = &mut linking => {
                    result.unwrap();
                    break;
                }
            }
        }
        drop(setup);
        self.head_is(head).await;
    }

    /// The tracker stores newer facts, as its poll or the update button would.
    pub async fn update_facts(&self, next: FetchedPullRequest) {
        let head = next.facts.head_sha.chars().next().unwrap();
        self.source.set(next);
        self.service.refresh(THREAD).await.unwrap();
        self.head_is(head).await;
    }

    /// The tracker stores a new title for the same facts, as its poll would after an edit.
    pub async fn update_title(&self, next: FetchedPullRequest) {
        let expected = next.display.title.as_ref().map(|t| t.as_str().to_owned());
        self.source.set(next);
        self.service.refresh(THREAD).await.unwrap();
        let mut events = self.events.lock().await;
        while self.stored_title() != expected {
            let event = events.recv().await.unwrap();
            drop(event);
        }
    }

    pub fn stored_title(&self) -> Option<String> {
        self.storage
            .handle()
            .pull_request_snapshot(&key())
            .unwrap()
            .and_then(|snapshot| snapshot.display.title)
            .map(|title| title.as_str().to_owned())
    }

    pub fn stored_head(&self) -> String {
        self.storage
            .handle()
            .pull_request_snapshot(&key())
            .unwrap()
            .and_then(|snapshot| snapshot.facts)
            .map(|facts| facts.head_sha)
            .unwrap_or_default()
    }
}

pub fn text_of(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The system and user text of every request that is a review (its system message starts with
/// `prefix`).
pub async fn reviews(model: &ScriptedModel, prefix: &str) -> Vec<(String, Value)> {
    model
        .observed()
        .await
        .into_iter()
        .filter_map(|messages| {
            let system = messages
                .iter()
                .find(|message| message.role == MessageRole::System)
                .map(text_of)?;
            system.starts_with(prefix).then(|| {
                let user = messages
                    .iter()
                    .find(|message| message.role == MessageRole::User)
                    .map(text_of)
                    .unwrap();
                (system, serde_json::from_str(&user).unwrap())
            })
        })
        .collect()
}

pub fn without_facts(mut payload: Value) -> Value {
    payload["context"]
        .as_object_mut()
        .unwrap()
        .remove("harness_facts")
        .expect("the review carried facts");
    payload
}

pub const REQUEST: &str = "Open the pull request and merge it after CI passes.";
pub const DELEGATION: &str = "Merge pull request 7 once its head is the reviewed one.";

pub fn merge_call(id: &str) -> providers::ChatResponse {
    tool_response(
        id,
        "shell",
        json!({
            "command": "gh pr merge 7 --squash",
            "sandbox_access": "unsandboxed",
            "justification": "deliver the reviewed change",
        }),
    )
}

/// Records the facts-update that happens while the agent runs, then answers with fixed text.
pub struct FactsTick {
    world: std::sync::OnceLock<Arc<World>>,
    heads: std::sync::Mutex<Vec<char>>,
}

#[async_trait]
impl Tool for FactsTick {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }

    async fn execute(&self, _input: Value) -> Result<ToolResult, ToolError> {
        let next = self.heads.lock().unwrap().pop();
        if let (Some(world), Some(head)) = (self.world.get(), next) {
            world.update_facts(answer(head, now_ns())).await;
        }
        Ok(ToolResult::success("ticked"))
    }
}

/// One run of an agent that calls `facts_tick` twice against the input-derived mock provider.
/// With `heads` the thread is linked and the tracker stores a newer head at each call.
pub async fn provider_run(heads: Option<[char; 2]>) -> (Vec<Value>, Vec<(u64, u64)>, String) {
    provider_run_with(
        heads,
        PullRequestsConfig {
            enabled: true,
            ..PullRequestsConfig::default()
        },
    )
    .await
}

/// [`provider_run`] with the given pull request settings.
pub async fn provider_run_with(
    heads: Option<[char; 2]>,
    settings: PullRequestsConfig,
) -> (Vec<Value>, Vec<(u64, u64)>, String) {
    use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
    const MODEL: &str = "mock-model";
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::tool_call("s1", MODEL, 0, "tick-1", "shell", ["{}".to_owned()]),
        ScriptedResponse::tool_call("s2", MODEL, 0, "tick-2", "shell", ["{}".to_owned()]),
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
                    var: "EVORCH_PR_REVIEW_CACHE_KEY".into(),
                },
                models: vec![config::ModelEntryConfig::enabled(MODEL)],
                default_model: MODEL.into(),
                excluded_models: vec![],
                edit_tool: None,
            },
        )]),
        routing: config::RoutingConfig {
            routes: std::collections::BTreeMap::from([
                (
                    "orchestrator".into(),
                    vec![config::RouteCandidateConfig {
                        profile: "local".into(),
                        model: Some(MODEL.into()),
                        auto: false,
                    }],
                ),
                (
                    "worker".into(),
                    vec![config::RouteCandidateConfig {
                        profile: "local".into(),
                        model: Some(MODEL.into()),
                        auto: false,
                    }],
                ),
            ]),
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
            event_bus: Some(Arc::clone(&bus)),
            env: Arc::new(routing::MapEnv::from_iter([(
                "EVORCH_PR_REVIEW_CACHE_KEY",
                "offline-key",
            )])),
            catalog: model::ModelCatalog::new(),
            factory: Default::default(),
        },
        Some(test_support::model_catalog_dir()),
    )
    .unwrap();
    let tick = Arc::new(FactsTick {
        world: std::sync::OnceLock::new(),
        heads: std::sync::Mutex::new(
            heads
                .map(|heads| vec![heads[1], heads[0]])
                .unwrap_or_default(),
        ),
    });
    let mut executor = ToolExecutor::new(Arc::clone(&bus));
    executor.register(tick.clone()).unwrap();
    let world = Arc::new(world_with(model, executor, bus, settings));
    if heads.is_some() {
        world.link(answer('a', now_ns())).await;
        assert!(tick.world.set(Arc::clone(&world)).is_ok());
    }

    let run = world
        .runtime
        .delegate_chat(
            THREAD,
            Role::Worker,
            "tick twice".into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(world.runtime.wait(run).await, Ok(AgentRunPhase::Done));

    let requests: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| request.body)
        .collect();
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
    (requests, usage, world.stored_head())
}
