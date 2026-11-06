//! The world of the PR E end-to-end and cache tests (ADR 0043 Phase 2): the real runtime with
//! a conversation root, the real pull request service and coordinator, the real common outbox
//! delivery, and the GUI's command sink and window over them. Only what the world cannot reach
//! is scripted: the pull request facts, the CI failure logs and the model.
//!
//! Nothing here waits for a duration. A step waits for an event the runtime published (the
//! service's change, the delivery of a notice, the run going idle).
//!
//! The including test declares `pull_request_world` and `short_private_directory` at its
//! root, as this module borrows both.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use event_bus::{
    AgentRunPhase, Event, EventBus, EventKind, EventReceiver, LifecycleEvent, ProviderEvent,
    PullRequestEvent,
};
use gui::app::WorkbenchState;
use gui::fixture::DemoSource;
use gui::model::commands::{ChatSubmission, CommandSink, LoopEvent};
use gui::model::composer::ComposerRole;
use gui::model::pull_requests::PullRequestAction;
use gui::model::transcript::TranscriptEntry;
use gui::runtime_sink::RuntimeCommandSink;
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, RequestTurnContext, Role as MessageRole,
    ToolSpec, Usage,
};
use runtime::ownership::{OwnerHost, OwnerPermit};
use runtime::pull_requests::{
    CiLogBundle, CiLogRun, FakePullRequestLogSource, FakePullRequestSource, PullRequestService,
};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, ModelSource, Role, RuntimeComposition,
    RuntimeError, compose_runtime,
};
use serde_json::Value;
use storage::pull_request_auto_fix::PrNotice;

use crate::pull_request_world::{enabled, failing, key, window};
use crate::short_private_directory::ShortPrivateDirectory;

pub const THREAD: &str = "p2-thread";
pub const NUMBER: u64 = 42;
pub const MODEL: &str = "mock-model";
pub const EXCERPT: &str = "error[E0308]: mismatched types\n --> src/lib.rs:3:9";

/// A model that answers every call with a short text and keeps what it was asked.
#[derive(Default)]
pub struct Recorder {
    pub requests: Mutex<Vec<(Vec<Message>, Option<RequestTurnContext>)>>,
}

#[async_trait]
impl AgentModel for Recorder {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let mut requests = self.requests.lock().unwrap();
        requests.push((messages.to_vec(), invocation.turn.clone()));
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!("reply-{}", requests.len()),
                }],
            },
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "pull-request-phase2".into()
    }
}

pub fn text(words: &str) -> ScriptedResponse {
    ScriptedResponse::text_stream("response", MODEL, [words])
}

pub struct World {
    pub dir: tempfile::TempDir,
    pub owners: ShortPrivateDirectory,
    pub mock: Option<StreamingMockOpenAi>,
    pub recorder: Option<Arc<Recorder>>,
    pub runtime: AgentRuntime,
    pub bus: Arc<EventBus>,
    pub events: EventReceiver,
    pub store: storage::Storage,
    pub owner: Arc<OwnerHost>,
    pub source: Arc<FakePullRequestSource>,
    pub logs: FakePullRequestLogSource,
    pub service: PullRequestService,
    pub window: WorkbenchState<DemoSource>,
    pub permit: OwnerPermit,
    pub run: String,
    /// The events received and not yet shown by the window.
    pub seen: Vec<Event>,
    /// Every event received, for the assertions about what was published.
    pub history: Vec<Event>,
    pub cached: Vec<u64>,
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

impl World {
    /// A runtime whose model is a recorder; the conversation has had its first turn.
    pub async fn recorded() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Arc::new(Recorder::default());
        let config = storage::StorageConfig {
            db_path: dir.path().join("phase2.db"),
            ..Default::default()
        };
        let store = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(8192));
        let events = bus.subscribe();
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            recorder.clone(),
        )
        .with_run_store(runtime::RunStore::open(&config, store.handle()).unwrap());
        Self::start(dir, None, Some(recorder), runtime, store, bus, events).await
    }

    /// A runtime that talks to a mock OpenAI server (real wire bytes and an input-derived
    /// cache); the conversation has had its first turn.
    pub async fn mocked() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
            vec![text("initial answer")],
            vec![MODEL.into()],
        );
        std::fs::create_dir_all(dir.path().join(config::PROJECT_CONFIG_DIR)).unwrap();
        std::fs::write(
            config::project_main_config_path(dir.path()),
            format!(
                r#"
[providers.local]
type="openai-compatible"
base_url="{}"
api_key_env="PR_PHASE2_TEST_KEY"
models=["{MODEL}"]
default_model="{MODEL}"
[[routing.routes.worker]]
profile="local"
[compaction]
context_window_tokens=1000000
threshold=0.95
"#,
                mock.base_url()
            ),
        )
        .unwrap();
        let loaded = config::Config::load(&config::LoadOptions {
            project_dir: Some(dir.path().into()),
            user_config_dir: Some(dir.path().join(config::PROJECT_CONFIG_DIR)),
            read_env: false,
            ..Default::default()
        })
        .unwrap();
        let bus = Arc::new(EventBus::new(8192));
        let events = bus.subscribe();
        let runtime = compose_runtime(RuntimeComposition {
            config: &loaded,
            bus: bus.clone(),
            executor: Arc::new(tools::ToolExecutor::new(bus.clone())),
            workspace: None,
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(dir.path().join("credentials"))
                    .unwrap(),
            ),
            env: Arc::new(routing::MapEnv::from_iter([(
                "PR_PHASE2_TEST_KEY",
                "offline-test",
            )])),
            model_source: ModelSource::Configured,
            model_catalog_dir: Some(test_support::model_catalog_dir()),
            user_config_dir: Some(dir.path().join("empty-user")),
        })
        .unwrap()
        .runtime;
        let config = storage::StorageConfig {
            db_path: dir.path().join("phase2.db"),
            ..Default::default()
        };
        let store = storage::Storage::open(config.clone()).unwrap();
        let runtime =
            runtime.with_run_store(runtime::RunStore::open(&config, store.handle()).unwrap());
        Self::start(dir, Some(mock), None, runtime, store, bus, events).await
    }

    async fn start(
        dir: tempfile::TempDir,
        mock: Option<StreamingMockOpenAi>,
        recorder: Option<Arc<Recorder>>,
        runtime: AgentRuntime,
        store: storage::Storage,
        bus: Arc<EventBus>,
        events: EventReceiver,
    ) -> Self {
        let owners = ShortPrivateDirectory::new();
        let owner =
            Arc::new(OwnerHost::open(owners.path(), Default::default(), bus.clone()).unwrap());
        let source = Arc::new(FakePullRequestSource::new());
        let logs = FakePullRequestLogSource::default();
        let service = runtime
            .start_pull_requests_with_log_source(
                owner.clone() as Arc<dyn runtime::pull_requests::OwnershipView>,
                source.clone(),
                enabled(),
                Arc::new(logs.clone()),
            )
            .unwrap();
        let permit = owner.start(THREAD).unwrap();
        let mut sink = RuntimeCommandSink::new(runtime.clone(), tokio::runtime::Handle::current())
            .with_ownership(owner.clone());
        let accepted = sink.submit_chat_with_permit(chat("fix the pull request"), permit.clone());
        let run = match accepted.as_slice() {
            [LoopEvent::ChatAccepted { run_id, .. }] => run_id.clone(),
            other => panic!("the first turn was not accepted: {other:?}"),
        };
        let sink = sink.with_lane_delivery();
        let window = window(
            &dir.path().join("phase2.db"),
            dir.path(),
            &owner,
            Box::new(sink),
            &[THREAD],
        );
        let mut world = Self {
            dir,
            owners,
            mock,
            recorder,
            runtime,
            bus,
            events,
            store,
            owner,
            source,
            logs,
            service,
            window,
            permit,
            run,
            seen: Vec::new(),
            history: Vec::new(),
            cached: Vec::new(),
        };
        world.idle().await;
        world
    }

    fn take(&mut self, event: Event) {
        if let EventKind::Provider(ProviderEvent::RequestCompleted {
            run_id: Some(id),
            cache_read_tokens,
            ..
        }) = &event.kind
            && *id == self.run
        {
            self.cached.push(*cache_read_tokens);
        }
        self.history.push(event.clone());
        self.seen.push(event);
    }

    /// Receives events until one matches; every event received is kept for the window.
    pub async fn until(&mut self, wanted: impl Fn(&Event) -> bool) {
        loop {
            let event = self.events.recv().await.unwrap();
            let done = wanted(&event);
            self.take(event);
            if done {
                return;
            }
        }
    }

    /// Until the conversation root has gone idle.
    pub async fn idle(&mut self) {
        let run = self.run.clone();
        self.until(move |event| {
            matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id, to, reason, ..
            }) if {
                assert_ne!(*to, AgentRunPhase::Error, "{reason:?}");
                *run_id == run && *to == AgentRunPhase::Waiting
            })
        })
        .await;
    }

    pub async fn until_pr(&mut self, wanted: impl Fn(&PullRequestEvent) -> bool) {
        self.until(
            move |event| matches!(&event.kind, EventKind::PullRequest(change) if wanted(change)),
        )
        .await;
    }

    /// The service's change for the thread (a fetch that found something new).
    pub async fn changed(&mut self) {
        self.until_pr(|change| {
            matches!(change, PullRequestEvent::Changed { thread_id, .. } if thread_id == THREAD)
        })
        .await;
    }

    /// The next notice was committed into the conversation.
    pub async fn delivered(&mut self) -> String {
        let mut id = String::new();
        let slot = &mut id;
        loop {
            let event = self.events.recv().await.unwrap();
            let found = match &event.kind {
                EventKind::Lifecycle(LifecycleEvent::HarnessNoticeDelivered {
                    notification_id,
                    ..
                }) => Some(notification_id.clone()),
                _ => None,
            };
            self.take(event);
            if let Some(found) = found {
                *slot = found;
                return id;
            }
        }
    }

    /// Hands the received events to the window and lets it read the database.
    pub fn show(&mut self) {
        self.window.apply_events(std::mem::take(&mut self.seen));
        self.settle();
    }

    pub fn settle(&mut self) {
        for _ in 0..4 {
            self.window.pump_pull_requests();
            self.window.poll_command_sink();
        }
    }

    pub fn click(&mut self, action: PullRequestAction) {
        self.window.apply_pull_request_action(action);
    }

    /// Links the pull request with `facts` fetched and waits until the window shows it.
    pub async fn link(&mut self, fetched: runtime::pull_requests::FetchedPullRequest) {
        self.source.push_ok(&key(NUMBER), fetched);
        self.service
            .link(
                THREAD,
                &format!("https://github.com/turtton/evorch/pull/{NUMBER}"),
                0,
                &self.permit,
            )
            .await
            .unwrap();
        self.changed().await;
        self.show();
    }

    /// Moves the pull request to `fetched` and waits for the service to say so. The window is
    /// not shown the news: a test that wants a stale screen leaves it so.
    pub async fn move_to(&mut self, fetched: runtime::pull_requests::FetchedPullRequest) {
        self.source.push_ok(&key(NUMBER), fetched);
        self.service.refresh(THREAD, &self.permit).await.unwrap();
        self.changed().await;
    }

    pub fn failing(&self, head: char) -> runtime::pull_requests::FetchedPullRequest {
        failing(NUMBER, head)
    }

    pub fn push_logs(&self, head: char) {
        self.logs.push(CiLogBundle {
            head_sha: head.to_string().repeat(40),
            runs: vec![CiLogRun {
                run_id: 5,
                checks: vec!["ci".into()],
                excerpt: EXCERPT.into(),
                lines_omitted: 0,
                raw_truncated: false,
                line_cut: false,
                lossy_utf8: false,
                redactions: 0,
            }],
            skipped: Vec::new(),
            truncated: false,
        });
    }

    pub fn notices(&self) -> Vec<PrNotice> {
        self.store
            .handle()
            .pr_notices(key(NUMBER), String::new())
            .unwrap()
    }

    /// The harness cards the window's transcript shows, as `(notice ID, card read)`.
    pub fn cards(&self) -> Vec<(String, bool)> {
        cards_of(&self.window)
    }

    pub fn requests(&self) -> Vec<Value> {
        self.mock
            .as_ref()
            .expect("a mocked world")
            .recorded_requests()
            .into_iter()
            .filter(|request| request.path == "/v1/chat/completions")
            .map(|request| request.body)
            .collect()
    }

    pub fn issued_commands(&self) -> usize {
        self.window.issued().len()
    }

    pub fn shutdown(&self) {
        self.service.shutdown();
    }
}

pub fn cards_of(window: &WorkbenchState<DemoSource>) -> Vec<(String, bool)> {
    window
        .transcript()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::HarnessNotice { notice_id, card } => {
                Some((notice_id.clone(), card.is_some()))
            }
            _ => None,
        })
        .collect()
}
