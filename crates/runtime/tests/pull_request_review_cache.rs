//! ADR 0043 cache contract for the facts the escalation review is given about a thread's pull
//! request.
//!
//! The facts live only in the review's own one-shot input. Updating them must not touch what the
//! agent is sent (system, tools, settings, cache affinity key, the history prefix), and between
//! two reviews of one thread the model's input may differ only in the `harness_facts` value.
//! The web path takes the stored facts without starting anything.

#[path = "support/mod.rs"]
mod support;

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agents::Role;
use async_trait::async_trait;
use config::{EscalationApproval, PullRequestsConfig};
use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver};
use providers::{ContentBlock, FinishReason, Message, Role as MessageRole};
use runtime::ownership::OwnerHost;
use runtime::pull_requests::{
    FakePullRequestSource, FetchedPullRequest, OwnershipView, PullRequestService,
};
use runtime::{AgentRuntime, RunConfig, RunStore};
use sandbox::DirectSandbox;
use serde_json::{Value, json};
use storage::pull_requests::{
    CheckConclusion, CheckKind, CheckStatus, PullRequestCheck, PullRequestFacts, PullRequestKey,
    PullRequestState, ReviewDecision,
};
use support::{ScriptedModel, text_response, tool_response};
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};

const THREAD: &str = "thread-pr-review";
const REPOSITORY: &str = "turtton/evorch";
const DENY: &str = r#"{"approve":false,"reason":"not now","risk_level":"low","authorization_level":"none","procedure_step":"none"}"#;

fn key() -> PullRequestKey {
    PullRequestKey::github(REPOSITORY, 7).unwrap()
}

fn now_ns() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap()
}

/// The pull request as the fake source reports it: an open one at `head` repeated, one passing
/// check, fetched at `fetched_at_ns`.
fn answer(head: char, fetched_at_ns: i64) -> FetchedPullRequest {
    FetchedPullRequest {
        requested: key(),
        canonical: key(),
        url: format!("https://github.com/{REPOSITORY}/pull/7"),
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

/// A runtime with the pull request service on, and its fake source.
struct World {
    runtime: AgentRuntime,
    service: PullRequestService,
    source: FakePullRequestSource,
    host: Arc<OwnerHost>,
    storage: storage::Storage,
    events: tokio::sync::Mutex<EventReceiver>,
    _directory: tempfile::TempDir,
}

fn world(model: Arc<dyn runtime::AgentModel>, executor: ToolExecutor, bus: Arc<EventBus>) -> World {
    let directory = tempfile::tempdir().unwrap();
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
    let host = Arc::new(
        OwnerHost::open(
            &directory.path().join("owners"),
            config::OwnershipConfig::default(),
            Arc::clone(&bus),
        )
        .unwrap(),
    );
    let source = FakePullRequestSource::new();
    let service = runtime
        .start_pull_requests(
            Arc::clone(&host) as Arc<dyn OwnershipView>,
            Arc::new(source.clone()),
            PullRequestsConfig {
                enabled: true,
                ..PullRequestsConfig::default()
            },
        )
        .unwrap();
    World {
        runtime,
        service,
        source,
        host,
        storage,
        events: tokio::sync::Mutex::new(events),
        _directory: directory,
    }
}

impl World {
    fn permit(&self) -> runtime::ownership::OwnerPermit {
        self.host
            .owned_permit(THREAD)
            .or_else(|_| self.host.start(THREAD))
            .unwrap()
    }

    /// Waits until the stored facts show `head`, woken by the service's own events.
    async fn head_is(&self, head: char) {
        let expected = head.to_string().repeat(40);
        let mut events = self.events.lock().await;
        while self.stored_head() != expected {
            let event = events.recv().await.unwrap();
            drop(event);
        }
    }

    /// Links the thread; the service fetches once, as scripted.
    async fn link(&self, first: FetchedPullRequest) {
        let head = first.facts.head_sha.chars().next().unwrap();
        self.source.push_ok(&key(), first);
        let revision = self
            .storage
            .handle()
            .pull_request_thread_state(THREAD)
            .unwrap()
            .lifecycle_revision;
        self.service
            .link(THREAD, &format!("{REPOSITORY}#7"), revision, &self.permit())
            .await
            .unwrap();
        self.head_is(head).await;
    }

    /// The tracker stores newer facts, as its poll or the update button would.
    async fn update_facts(&self, next: FetchedPullRequest) {
        let head = next.facts.head_sha.chars().next().unwrap();
        self.source.push_ok(&key(), next);
        self.service.refresh(THREAD, &self.permit()).await.unwrap();
        self.head_is(head).await;
    }

    fn stored_head(&self) -> String {
        self.storage
            .handle()
            .pull_request_snapshot(&key())
            .unwrap()
            .and_then(|snapshot| snapshot.facts)
            .map(|facts| facts.head_sha)
            .unwrap_or_default()
    }
}

fn text_of(message: &Message) -> String {
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
async fn reviews(model: &ScriptedModel, prefix: &str) -> Vec<(String, Value)> {
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

fn without_facts(mut payload: Value) -> Value {
    payload["context"]
        .as_object_mut()
        .unwrap()
        .remove("harness_facts")
        .expect("the review carried facts");
    payload
}

const REQUEST: &str = "Open the pull request and merge it after CI passes.";
const DELEGATION: &str = "Merge pull request 7 once its head is the reviewed one.";

fn merge_call(id: &str) -> providers::ChatResponse {
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

// Given: a thread whose pull request facts are updated between two reviews of the same kind /
// When: each review's model input is compared / Then: the instruction is byte-identical, the
// payloads differ only in `context.harness_facts`, and the agent's own requests are untouched
#[tokio::test]
async fn a_facts_update_changes_only_the_facts_in_the_next_review() {
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(DENY, FinishReason::Stop)),
        Ok(text_response(DENY, FinishReason::Stop)),
    ]));
    model
        .add_keyed(REQUEST, [Ok(text_response("ready", FinishReason::Stop))])
        .await;
    model
        .add_keyed(
            DELEGATION,
            [
                Ok(merge_call("merge")),
                Ok(text_response("stopped", FinishReason::Stop)),
                Ok(merge_call("merge")),
                Ok(text_response("stopped", FinishReason::Stop)),
            ],
        )
        .await;
    let bus = Arc::new(EventBus::new(4096));
    let executor = ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    );
    let world = world(model.clone(), executor, bus);
    world.link(answer('a', now_ns())).await;

    let root = world
        .runtime
        .delegate_chat(
            THREAD,
            Role::Orchestrator,
            REQUEST.into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(world.runtime.wait(root).await, Ok(AgentRunPhase::Done));
    let first = world
        .runtime
        .delegate_background_as_child(root, Role::Worker, DELEGATION, RunConfig::default())
        .expect("registered child");
    assert_eq!(world.runtime.wait(first).await, Ok(AgentRunPhase::Done));
    world.update_facts(answer('b', now_ns())).await;
    let second = world
        .runtime
        .delegate_background_as_child(root, Role::Worker, DELEGATION, RunConfig::default())
        .expect("registered child");
    assert_eq!(world.runtime.wait(second).await, Ok(AgentRunPhase::Done));

    let reviews = reviews(&model, "Review a shell sandbox permission request.").await;
    assert_eq!(
        reviews.len(),
        2,
        "both merges reached the automatic reviewer"
    );
    let heads: Vec<_> = reviews
        .iter()
        .map(|(_, payload)| {
            payload["context"]["harness_facts"]["pull_request"]["head_sha"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        heads,
        ["a".repeat(40), "b".repeat(40)],
        "the update reached the review"
    );
    assert_eq!(reviews[0].0, reviews[1].0, "the instruction is static");
    assert!(reviews[0].0.contains("harness_facts"));
    assert_eq!(
        without_facts(reviews[0].1.clone()),
        without_facts(reviews[1].1.clone()),
        "nothing but the facts differs between the two review inputs"
    );

    let observed = model.observed().await;
    let offered = model.observed_tools().await;
    let agent: Vec<usize> = (0..observed.len())
        .filter(|&index| {
            !observed[index]
                .iter()
                .any(|message| message.role == MessageRole::System)
                && text_of(&observed[index][0]).starts_with(DELEGATION)
        })
        .collect();
    assert_eq!(agent.len(), 4);
    let sent = |indexes: &[usize]| {
        serde_json::to_string(&(
            indexes.iter().map(|&i| &observed[i]).collect::<Vec<_>>(),
            indexes.iter().map(|&i| &offered[i]).collect::<Vec<_>>(),
        ))
        .unwrap()
    };
    assert_eq!(
        sent(&agent[..2]),
        sent(&agent[2..]),
        "the agent sends the same messages and tools before and after its pull request's facts moved"
    );
    assert!(observed[agent[1]].starts_with(&observed[agent[0]]));
    assert!(!offered[agent[0]].is_empty());
}

// Given: a linked thread whose stored facts are old / When: a web_fetch call is reviewed /
// Then: the review gets the stored facts marked stale and nothing is fetched
#[tokio::test]
async fn a_web_review_reads_the_stored_facts_and_starts_nothing() {
    const WEB_REQUEST: &str = "Read the release notes page and summarize it.";
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        DENY,
        FinishReason::Stop,
    ))]));
    model
        .add_keyed(
            WEB_REQUEST,
            [
                Ok(tool_response(
                    "fetch-1",
                    "web_fetch",
                    json!({"url": "https://example.invalid/release-notes"}),
                )),
                Ok(text_response("could not read it", FinishReason::Stop)),
            ],
        )
        .await;
    let bus = Arc::new(EventBus::new(4096));
    let mut executor = ToolExecutor::new(Arc::clone(&bus));
    executor.register(Arc::new(FakeWebFetch)).unwrap();
    let world = world(model.clone(), executor, bus);
    // Fetched long ago, so the facts are older than the review's limit.
    world.link(answer('a', 1_000)).await;
    let fetches = world.source.calls().len();

    let root = world
        .runtime
        .delegate_chat(
            THREAD,
            Role::Orchestrator,
            WEB_REQUEST.into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(world.runtime.wait(root).await, Ok(AgentRunPhase::Done));

    let reviews = reviews(&model, "Review one web_fetch request.").await;
    assert_eq!(reviews.len(), 1);
    let facts = &reviews[0].1["context"]["harness_facts"]["pull_request"];
    assert_eq!(facts["freshness"], "stale");
    assert_eq!(facts["stale_reason"], "too_old");
    assert_eq!(facts["head_sha"], "a".repeat(40));
    assert_eq!(
        world.source.calls().len(),
        fetches,
        "a web review never fetches"
    );
    assert!(reviews[0].0.contains("harness_facts"));
}

/// A `web_fetch` that must never run: the call is reviewed first, and the review denies it.
struct FakeWebFetch;

#[async_trait]
impl Tool for FakeWebFetch {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn schema(&self) -> Value {
        json!({"type": "object", "required": ["url"], "properties": {"url": {"type": "string"}}})
    }

    fn permissions(&self) -> Permissions {
        Permissions {
            fs_read: false,
            fs_write: false,
            process_spawn: false,
            network: true,
        }
    }

    async fn execute(&self, _input: Value) -> Result<ToolResult, ToolError> {
        panic!("a denied review must not reach the tool");
    }
}

/// A page whose outline names one file input, `e5`, inside a form served by
/// `https://forms.example.test`, and that records every upload.
#[derive(Default)]
struct UploadPage {
    uploads: std::sync::Mutex<Vec<Value>>,
}

#[async_trait]
impl browser::agent::CdpTarget for UploadPage {
    async fn call(&self, method: &str, params: Value) -> Result<Value, browser::agent::AgentError> {
        Ok(match method {
            "Accessibility.getFullAXTree" => json!({"nodes": [{
                "nodeId": "1", "role": {"value": "button"}, "name": {"value": "Attachment"},
                "backendDOMNodeId": 5
            }]}),
            "DOM.resolveNode" => json!({"object": {"objectId": "input-5"}}),
            "Runtime.callFunctionOn" => {
                json!({"result": {"value": "https://forms.example.test/vendor/upload"}})
            }
            "Runtime.evaluate" => json!({"result": {"value": "https://portal.example.test/apply"}}),
            "DOM.setFileInputFiles" => {
                self.uploads.lock().unwrap().push(params["files"].clone());
                json!({})
            }
            _ => json!({}),
        })
    }

    async fn navigate(&self, _url: &str) -> Result<(), browser::agent::AgentError> {
        Ok(())
    }
}

struct OnePage(Arc<browser::agent::PageAgent>);

#[async_trait]
impl browser::agent::PageProvider for OnePage {
    async fn page(
        &self,
        _key: &str,
    ) -> Result<Arc<browser::agent::PageAgent>, browser::agent::AgentError> {
        Ok(Arc::clone(&self.0))
    }

    async fn open_page(&self, _key: &str) -> Option<Arc<browser::agent::PageAgent>> {
        Some(Arc::clone(&self.0))
    }

    async fn close(&self, _key: &str) -> bool {
        false
    }

    fn release(&self, _key: &str) {}
}

const UPLOAD_REQUEST: &str = "Send docs/report.txt through the vendor form";
const UPLOAD_REVIEW: &str = "Review one browser_upload_file request";

/// A world whose executor offers the real `browser_upload_file` over a fake page, and a model
/// that asks for one upload and then finishes. The review answers come from `model`'s default
/// script.
async fn upload_world(model: Arc<ScriptedModel>) -> (World, Arc<UploadPage>, tempfile::TempDir) {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("docs")).unwrap();
    std::fs::write(root.join("docs/report.txt"), "quarterly numbers").unwrap();
    model
        .add_keyed(
            UPLOAD_REQUEST,
            [
                Ok(tool_response(
                    "upload-1",
                    "browser_upload_file",
                    json!({"ref": "e5", "paths": [root.join("docs/report.txt")]}),
                )),
                Ok(text_response("finished", FinishReason::Stop)),
            ],
        )
        .await;
    let page = Arc::new(UploadPage::default());
    let agent = Arc::new(browser::agent::PageAgent::new(page.clone()));
    agent.snapshot().await.unwrap();
    let bus = Arc::new(EventBus::new(4096));
    let executor = ToolExecutor::with_standard_tools_in(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
        Some(root.clone()),
    )
    .with_browser_provider(Arc::new(OnePage(agent)), Default::default());
    executor.set_file_boundary(&sandbox::SandboxBoundary::new(root));
    (world(model, executor, bus), page, workspace)
}

const APPROVE: &str = r#"{"approve":true,"reason":"requested","risk_level":"medium","authorization_level":"high","procedure_step":"none"}"#;

// Given: a linked thread whose stored facts are old / When: an upload is reviewed through the real
// browser_upload_file path / Then: the review gets the stored facts marked stale, nothing is
// fetched, and the denied upload never reaches the page
#[tokio::test]
async fn an_upload_review_reads_the_stored_facts_and_starts_nothing() {
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        DENY,
        FinishReason::Stop,
    ))]));
    let (world, page, _workspace) = upload_world(model.clone()).await;
    world.link(answer('a', 1_000)).await;
    let fetches = world.source.calls().len();

    let root = world
        .runtime
        .delegate_chat(
            THREAD,
            Role::WebResearcher,
            UPLOAD_REQUEST.into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(world.runtime.wait(root).await, Ok(AgentRunPhase::Done));

    let reviews = reviews(&model, UPLOAD_REVIEW).await;
    assert_eq!(reviews.len(), 1);
    let facts = &reviews[0].1["context"]["harness_facts"]["pull_request"];
    assert_eq!(facts["freshness"], "stale");
    assert_eq!(facts["stale_reason"], "too_old");
    assert_eq!(
        reviews[0].1["upload"]["form_origin"],
        "https://forms.example.test"
    );
    assert!(reviews[0].0.contains("harness_facts"));
    assert_eq!(
        world.source.calls().len(),
        fetches,
        "an upload review never fetches"
    );
    assert!(page.uploads.lock().unwrap().is_empty());
}

// Given: an upload whose review approves / When: the thread is unlinked while the review runs /
// Then: the approval is not kept, so the page gets nothing
#[tokio::test]
async fn an_approved_upload_is_dropped_when_the_link_changes_during_its_review() {
    let release = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [Ok(text_response(APPROVE, FinishReason::Stop))],
        Arc::clone(&release),
    ));
    let (world, page, _workspace) = upload_world(model.clone()).await;
    world.link(answer('a', now_ns())).await;

    let root = world
        .runtime
        .delegate_chat(
            THREAD,
            Role::WebResearcher,
            UPLOAD_REQUEST.into(),
            RunConfig::default(),
        )
        .unwrap();
    let (phase, ()) = tokio::join!(world.runtime.wait(root), async {
        model.wait_for_request(0).await;
        release.notify_one();
        model.wait_for_request(1).await;
        let revision = world
            .storage
            .handle()
            .pull_request_thread_state(THREAD)
            .unwrap()
            .lifecycle_revision;
        world
            .service
            .unlink(THREAD, revision, &world.permit())
            .await
            .unwrap();
        release.notify_one();
        model.wait_for_request(2).await;
        release.notify_one();
    });

    assert_eq!(phase, Ok(AgentRunPhase::Done));
    assert!(
        page.uploads.lock().unwrap().is_empty(),
        "the upload was approved by a review that the unlink outdated"
    );
    let followup = format!("{:?}", model.observed().await[2]);
    assert!(
        followup.contains("review context changed during review"),
        "{followup}"
    );
}

/// Records the facts-update that happens while the agent runs, then answers with fixed text.
struct FactsTick {
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
async fn provider_run(heads: Option<[char; 2]>) -> (Vec<Value>, Vec<(u64, u64)>, String) {
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
    let world = Arc::new(world(model, executor, bus));
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

// Given: one agent run on a thread whose pull request facts are updated twice while it runs,
// and one on a thread that tracks nothing / When: the provider wire requests are compared /
// Then: they are the same bytes (so system, tools, settings and affinity key did not move), the
// prefix only grows, and the input-derived mock cache reuses the whole previous prefix
#[tokio::test]
async fn updating_facts_leaves_the_agents_wire_prefix_and_cache_untouched() {
    use mock_openai::cache_contract::{CacheProtocol, assert_append_only};

    let (plain, _, plain_head) = provider_run(None).await;
    let (tracked, usage, tracked_head) = provider_run(Some(['b', 'c'])).await;

    assert_eq!(plain_head, "", "the baseline tracks nothing");
    assert_eq!(
        tracked_head,
        "c".repeat(40),
        "the facts moved twice during the run"
    );
    assert_eq!(tracked.len(), 3);
    assert_eq!(
        serde_json::to_string(&tracked).unwrap(),
        serde_json::to_string(&plain).unwrap()
    );
    for pair in tracked.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    for pair in usage.windows(2) {
        assert!(pair[0].0 > 0);
        assert_eq!(
            pair[1].1, pair[0].0,
            "the input-derived mock must reuse the full previous prefix"
        );
    }
}
