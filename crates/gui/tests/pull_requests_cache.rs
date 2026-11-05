//! ADR 0043 cache contract for the GUI: pull request operations (Link, Refresh, a candidate
//! decision, Unlink, an inheritance) never change what the next model request carries. A
//! conversation operated on between its turns sends byte-for-byte the requests an untouched one
//! sends: the same system and tools, and every earlier message unchanged, in order.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent, PullRequestEvent};
use gui::model::commands::{
    ChatSubmission, CommandSink, LoopEvent, PullRequestDerivation, WorkbenchCommand,
};
use gui::model::composer::ComposerRole;
use gui::runtime_sink::RuntimeCommandSink;
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::ownership::OwnerHost;
use runtime::pull_requests::{FakePullRequestSource, FetchedPullRequest};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunStore, RuntimeError};
use storage::pull_requests::{
    CheckConclusion, CheckKind, CheckStatus, NewPullRequestCandidate, PullRequestCheck,
    PullRequestFacts, PullRequestKey, PullRequestState, ReviewDecision,
};

#[path = "support/short_private_directory.rs"]
mod short_private_directory;
use short_private_directory::ShortPrivateDirectory;

const THREAD: &str = "cache-thread";

type Request = (Vec<Message>, Vec<ToolSpec>);

struct Recording {
    script: Mutex<VecDeque<ChatResponse>>,
    requests: Mutex<Vec<Request>>,
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
        Ok(self.script.lock().unwrap().pop_front().expect("scripted"))
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "pull-requests-cache".into()
    }
}

fn reply(index: usize) -> ChatResponse {
    ChatResponse {
        usage_reported: true,
        message: Message {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::Text {
                text: format!("reply-{index}"),
            }],
        },
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    }
}

fn key(number: u64) -> PullRequestKey {
    PullRequestKey::github("turtton/evorch", number).unwrap()
}

fn fetched(number: u64, conclusion: Option<CheckConclusion>) -> FetchedPullRequest {
    FetchedPullRequest {
        requested: key(number),
        canonical: key(number),
        url: format!("https://github.com/turtton/evorch/pull/{number}"),
        facts: PullRequestFacts {
            state: PullRequestState::Open,
            base_branch: "main".into(),
            head_repository: Some("turtton/evorch".into()),
            head_branch: "feat/cache".into(),
            head_sha: "a".repeat(40),
            checks: vec![PullRequestCheck {
                provider_id: "ci".into(),
                kind: CheckKind::CheckRun,
                name: "ci".into(),
                status: if conclusion.is_some() {
                    CheckStatus::Completed
                } else {
                    CheckStatus::InProgress
                },
                conclusion,
                head_sha: "a".repeat(40),
            }],
            checks_complete: true,
            review_decision: ReviewDecision::None,
        },
        fetched_at_ns: 1_000,
    }
}

fn chat(text: &str) -> ChatSubmission {
    ChatSubmission {
        submission_id: String::new(),
        project_root: None,
        fork_seed: None,
        composer_role: ComposerRole::Worker,
        images: Vec::new(),
        thread_id: THREAD.into(),
        text: text.into(),
        model_preference: None,
        skills: Vec::new(),
    }
}

/// One conversation of two turns with `model`; `operate` has the GUI's pull request operations
/// run between them. Returns the bus, subscribed before anything ran.
async fn conversation(operate: bool, model: Arc<dyn AgentModel>) -> Arc<EventBus> {
    let directory = tempfile::tempdir().unwrap();
    let owners = ShortPrivateDirectory::new();
    let config = storage::StorageConfig {
        db_path: directory.path().join("cache.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(4096));
    let mut events = bus.subscribe();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        model,
    )
    .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let host = Arc::new(OwnerHost::open(owners.path(), Default::default(), bus.clone()).unwrap());
    let source = Arc::new(FakePullRequestSource::new());
    source.push_ok(&key(31), fetched(31, None));
    source.push_ok(&key(31), fetched(31, Some(CheckConclusion::Failure)));
    source.repeat_last();
    runtime
        .start_pull_requests(
            host.clone() as Arc<dyn runtime::pull_requests::OwnershipView>,
            source,
            config::PullRequestsConfig {
                enabled: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut sink = RuntimeCommandSink::new(runtime.clone(), tokio::runtime::Handle::current())
        .with_ownership(host.clone());
    let permit = host.start(THREAD).unwrap();

    let mut waiting = 0;
    let send = |sink: &mut RuntimeCommandSink, text: &str| {
        let events = sink.submit_chat_with_permit(chat(text), permit.clone());
        assert!(
            matches!(events.as_slice(), [LoopEvent::ChatAccepted { .. }]),
            "{events:?}"
        );
    };
    async fn until_waiting(
        events: &mut event_bus::EventReceiver,
        waiting: &mut usize,
        turns: usize,
    ) {
        while *waiting < turns {
            let event = events.recv().await.unwrap();
            if matches!(
                event.kind,
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    to: AgentRunPhase::Waiting,
                    ..
                })
            ) {
                *waiting += 1;
            }
        }
    }
    send(&mut sink, "first turn");
    until_waiting(&mut events, &mut waiting, 1).await;

    if operate {
        let candidate = storage
            .handle()
            .present_pull_request_candidate(NewPullRequestCandidate {
                thread_id: THREAD.into(),
                key: key(32),
                source_run_id: "run".into(),
                source_call_id: "call".into(),
                at_ns: 1,
            })
            .unwrap()
            .candidate;
        let mut command = |command: WorkbenchCommand| {
            let refused = sink.submit(command);
            assert!(refused.is_empty(), "{refused:?}");
        };
        command(WorkbenchCommand::LinkPullRequest {
            thread_id: THREAD.into(),
            locator: "turtton/evorch#31".into(),
            expected_revision: 0,
        });
        wait_pull_request(&mut events, |change| {
            matches!(change, PullRequestEvent::Changed { .. })
        })
        .await;
        command(WorkbenchCommand::RefreshPullRequest {
            thread_id: THREAD.into(),
        });
        wait_pull_request(&mut events, |change| {
            matches!(change, PullRequestEvent::Changed { .. })
        })
        .await;
        command(WorkbenchCommand::InheritPullRequest {
            parent_thread_id: THREAD.into(),
            child_thread_id: "cache-child".into(),
            kind: PullRequestDerivation::Fork,
        });
        command(WorkbenchCommand::DismissPullRequestCandidate {
            thread_id: THREAD.into(),
            candidate_id: candidate.candidate_id,
        });
        wait_pull_request(&mut events, |change| {
            matches!(change, PullRequestEvent::CandidateChanged { .. })
        })
        .await;
        command(WorkbenchCommand::UnlinkPullRequest {
            thread_id: THREAD.into(),
            expected_revision: 1,
        });
        wait_pull_request(&mut events, |change| {
            matches!(change, PullRequestEvent::Unlinked { .. })
        })
        .await;
    }

    send(&mut sink, "second turn");
    until_waiting(&mut events, &mut waiting, 2).await;
    bus
}

async fn recorded(operate: bool) -> Vec<Request> {
    let model = Arc::new(Recording {
        script: Mutex::new((1..=2).map(reply).collect()),
        requests: Mutex::default(),
    });
    conversation(operate, model.clone()).await;
    model.requests.lock().unwrap().clone()
}

async fn wait_pull_request(
    events: &mut event_bus::EventReceiver,
    wanted: impl Fn(&PullRequestEvent) -> bool,
) {
    loop {
        let event = events.recv().await.unwrap();
        if matches!(&event.kind, EventKind::PullRequest(change) if wanted(change)) {
            return;
        }
    }
}

// Given: two identical conversations, one operated on between its turns / Then: the model is sent
// the same bytes, and the second request extends the first
#[tokio::test]
async fn gui_pull_request_operations_leave_the_next_request_untouched() {
    let plain = recorded(false).await;
    let operated = recorded(true).await;

    assert_eq!(plain.len(), 2);
    assert_eq!(
        serde_json::to_string(&operated).unwrap(),
        serde_json::to_string(&plain).unwrap(),
        "an operation changed what the model is sent"
    );
    assert!(
        operated[1].0.starts_with(&operated[0].0),
        "a message sent before was rewritten"
    );
    assert_eq!(
        operated[0].1, operated[1].1,
        "the tools changed between turns"
    );
}

/// The routed model over a provider mock that derives cached tokens from the input it receives.
fn routed_model(mock_url: String, bus: Arc<EventBus>) -> Arc<dyn AgentModel> {
    const MODEL: &str = "mock-model";
    let config = config::Config {
        providers: std::collections::BTreeMap::from([(
            "local".into(),
            config::ProviderProfileConfig {
                provider_type: config::ProviderTypeConfig::OpenAiCompatible,
                api_protocol: config::ApiProtocolConfig::OpenAiCompletions,
                base_url: mock_url,
                credential: config::CredentialRefConfig::Env {
                    var: "EVORCH_PR_GUI_CACHE_KEY".into(),
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
    let credentials = sandbox::credential::FileCredentialStore::open(
        credentials_directory.path().join("credentials"),
    )
    .unwrap();
    // The store lives as long as the process: the model reads it per request.
    std::mem::forget(credentials_directory);
    runtime::compose::compose_routed_model(
        &config,
        routing::ComposeDeps {
            credential_store: Arc::new(credentials),
            event_bus: Some(bus),
            env: Arc::new(routing::MapEnv::from_iter([(
                "EVORCH_PR_GUI_CACHE_KEY",
                "offline-key",
            )])),
            catalog: model::ModelCatalog::new(),
            factory: Default::default(),
        },
        Some(test_support::model_catalog_dir()),
    )
    .unwrap()
}

// Given: a provider mock whose cache is derived from its input / When: the GUI operates on the
// thread between two turns / Then: the wire prefix only grows and the second request reuses the
// whole first one
#[tokio::test]
async fn gui_pull_request_operations_keep_the_wire_prefix_and_the_input_derived_cache() {
    use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
    use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
    const MODEL: &str = "mock-model";

    let mock = StreamingMockOpenAi::spawn_with_prompt_cache(vec![
        ScriptedResponse::text_stream("s1", MODEL, ["reply-1"]),
        ScriptedResponse::text_stream("s2", MODEL, ["reply-2"]),
    ]);
    let provider_bus = Arc::new(EventBus::new(4096));
    let mut provider_events = provider_bus.subscribe();
    let model = routed_model(mock.base_url(), provider_bus);

    conversation(true, model).await;

    let requests: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == "/v1/chat/completions")
        .map(|request| request.body)
        .collect();
    assert_eq!(requests.len(), 2);
    assert_append_only(CacheProtocol::OpenAi, &requests[0], &requests[1]).unwrap();
    let mut usage = Vec::new();
    while usage.len() < 2 {
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
    assert!(usage[0].0 > 0);
    assert_eq!(
        usage[1].1, usage[0].0,
        "the input-derived mock must reuse the full previous prefix"
    );
}
