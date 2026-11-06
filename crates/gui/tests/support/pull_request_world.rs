//! The pieces the pull request GUI tests share: scripted pull request facts, a fake "process"
//! (storage, bus, runtime, owner host, scripted source and the service), and a window over it.
//!
//! A test drives time and ordering through the fake source's gates and the events the service
//! publishes; nothing here waits for a duration.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use event_bus::{Event, EventBus, EventKind, EventReceiver, PullRequestEvent};
use gui::app::WorkbenchState;
use gui::app::pull_requests::StoragePullRequestReader;
use gui::fixture::DemoSource;
use gui::model::commands::CommandSink;
use gui::model::composer::ProviderStatus;
use gui::model::pull_requests::PullRequestAction;
use gui::model::transcript::TranscriptEntry;
use gui::runtime_sink::RuntimeCommandSink;
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::ownership::{OwnerHost, OwnerPermit};
use runtime::pull_requests::{FakePullRequestSource, FetchedPullRequest, PullRequestService};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, Role, RunConfig, RunStore, RuntimeError,
};
use serde_json::{Value, json};
use storage::pull_requests::{
    CheckConclusion, CheckKind, CheckStatus, PullRequestCheck, PullRequestFacts, PullRequestKey,
    PullRequestState, ReviewDecision,
};
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

pub const THREAD: &str = "t1";
pub const REPOSITORY: &str = "turtton/evorch";

pub struct NoModel;

#[async_trait]
impl AgentModel for NoModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: MessageRole::Assistant,
                content: Vec::new(),
            },
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "pull-requests-headless".into()
    }
}

pub fn key(number: u64) -> PullRequestKey {
    PullRequestKey::github(REPOSITORY, number).unwrap()
}

pub fn check(name: &str, conclusion: Option<CheckConclusion>) -> PullRequestCheck {
    PullRequestCheck {
        provider_id: name.into(),
        kind: CheckKind::CheckRun,
        name: name.into(),
        status: if conclusion.is_some() {
            CheckStatus::Completed
        } else {
            CheckStatus::InProgress
        },
        conclusion,
        head_sha: sha('a'),
    }
}

pub fn sha(letter: char) -> String {
    letter.to_string().repeat(40)
}

pub fn facts(
    state: PullRequestState,
    head: char,
    checks: Vec<PullRequestCheck>,
) -> PullRequestFacts {
    PullRequestFacts {
        state,
        base_branch: "main".into(),
        head_repository: Some(REPOSITORY.into()),
        head_branch: "feat/gui".into(),
        head_sha: sha(head),
        checks,
        checks_complete: true,
        review_decision: ReviewDecision::None,
    }
}

pub fn fetched(number: u64, facts: PullRequestFacts) -> FetchedPullRequest {
    FetchedPullRequest {
        requested: key(number),
        canonical: key(number),
        url: format!("https://github.com/{REPOSITORY}/pull/{number}"),
        display: Default::default(),
        facts,
        fetched_at_ns: gui::model::pull_requests::now_ns(),
    }
}

pub fn running(number: u64) -> FetchedPullRequest {
    fetched(
        number,
        facts(PullRequestState::Open, 'a', vec![check("ci", None)]),
    )
}

pub fn passing(number: u64) -> FetchedPullRequest {
    fetched(
        number,
        facts(
            PullRequestState::Open,
            'a',
            vec![check("ci", Some(CheckConclusion::Success))],
        ),
    )
}

pub fn failing(number: u64, head: char) -> FetchedPullRequest {
    fetched(
        number,
        facts(
            PullRequestState::Open,
            head,
            vec![check("ci", Some(CheckConclusion::Failure))],
        ),
    )
}

pub fn merged(number: u64) -> FetchedPullRequest {
    fetched(number, facts(PullRequestState::Merged, 'a', Vec::new()))
}

pub fn settle(state: &mut WorkbenchState<DemoSource>) {
    for _ in 0..4 {
        state.pump_pull_requests();
    }
}

pub fn changed(thread: &'static str) -> impl Fn(&PullRequestEvent) -> bool {
    move |event| matches!(event, PullRequestEvent::Changed { thread_id, .. } if thread_id == thread)
}

pub fn link_action(locator: &str) -> PullRequestAction {
    PullRequestAction::Link {
        thread_id: THREAD.into(),
        locator: locator.into(),
    }
}

pub fn notices(state: &WorkbenchState<DemoSource>) -> Vec<String> {
    state
        .transcript()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Notice { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// A shell that "creates a pull request": its output names one, as `gh pr create` prints it.
pub struct CreatePullRequest;

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
        Ok(ToolResult::success(format!(
            "Creating pull request for feat into main\n\nhttps://github.com/{REPOSITORY}/pull/{number}\n"
        )))
    }
}

/// An agent that calls [`CreatePullRequest`] once per queued number, then finishes.
pub struct CreatePullRequestModel {
    numbers: Mutex<VecDeque<u64>>,
}

impl CreatePullRequestModel {
    pub fn new(numbers: impl IntoIterator<Item = u64>) -> Arc<Self> {
        Arc::new(Self {
            numbers: Mutex::new(numbers.into_iter().collect()),
        })
    }
}

#[async_trait]
impl AgentModel for CreatePullRequestModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let answered = messages.last().is_some_and(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        });
        let next = if answered {
            None
        } else {
            self.numbers.lock().unwrap().pop_front()
        };
        let (content, finish_reason) = match next {
            Some(number) => (
                vec![ContentBlock::ToolUse {
                    id: format!("create-{number}"),
                    name: "shell".into(),
                    input: json!({ "number": number }),
                }],
                FinishReason::ToolUse,
            ),
            None => (
                vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                FinishReason::Stop,
            ),
        };
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: MessageRole::Assistant,
                content,
            },
            finish_reason,
            usage: Usage::default(),
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "pull-request-e2e".into()
    }
}

pub fn enabled() -> config::PullRequestsConfig {
    config::PullRequestsConfig {
        enabled: true,
        ..Default::default()
    }
}

/// Moves virtual time forward and lets the tasks that became due start.
pub async fn pass(seconds: u64) {
    tokio::time::advance(Duration::from_secs(seconds)).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

/// One "process" over a database: its own storage connection, bus, runtime, owner host
/// (sharing `owners` with the others, as processes share the registry directory), scripted
/// source and service.
pub struct Process {
    pub config: storage::StorageConfig,
    pub storage: storage::Storage,
    pub bus: Arc<EventBus>,
    pub runtime: AgentRuntime,
    pub host: Arc<OwnerHost>,
    pub source: Arc<FakePullRequestSource>,
    pub service: PullRequestService,
    pub events: EventReceiver,
    pub seen: Vec<Event>,
    sentinels: u64,
}

impl Process {
    pub fn open(
        db_path: &Path,
        owners: &Path,
        model: Arc<dyn AgentModel>,
        settings: config::PullRequestsConfig,
    ) -> Self {
        let config = storage::StorageConfig {
            db_path: db_path.to_path_buf(),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(4096));
        let events = bus.subscribe();
        let mut executor = ToolExecutor::new(bus.clone());
        executor.register(Arc::new(CreatePullRequest)).unwrap();
        let runtime = AgentRuntime::new(bus.clone(), Arc::new(executor), model)
            .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        let host = Arc::new(OwnerHost::open(owners, Default::default(), bus.clone()).unwrap());
        let source = Arc::new(FakePullRequestSource::new());
        let service = runtime
            .start_pull_requests(
                host.clone() as Arc<dyn runtime::pull_requests::OwnershipView>,
                source.clone(),
                settings,
            )
            .unwrap();
        Self {
            config,
            storage,
            bus,
            runtime,
            host,
            source,
            service,
            events,
            seen: Vec::new(),
            sentinels: 0,
        }
    }

    pub fn sink(&self) -> Box<dyn CommandSink> {
        Box::new(
            RuntimeCommandSink::new(self.runtime.clone(), tokio::runtime::Handle::current())
                .with_ownership(self.host.clone()),
        )
    }

    /// This process's permit for `thread`, taking the thread if nobody holds it.
    pub fn permit(&self, thread: &str) -> OwnerPermit {
        self.host
            .owned_permit(thread)
            .or_else(|_| self.host.start(thread))
            .unwrap()
    }

    pub fn revision(&self, thread: &str) -> u64 {
        self.storage
            .handle()
            .pull_request_thread_state(thread)
            .unwrap()
            .lifecycle_revision
    }

    /// The pull request the thread is linked to now.
    pub fn linked_to(&self, thread: &str) -> Option<PullRequestKey> {
        self.storage
            .handle()
            .pull_request_thread_state(thread)
            .unwrap()
            .active_link
            .map(|link| link.key)
    }

    /// How many times the source was asked for pull request `number`.
    pub fn fetches_of(&self, number: u64) -> usize {
        self.source
            .calls()
            .iter()
            .filter(|(requested, _)| *requested == key(number))
            .count()
    }

    pub fn database(&self) -> storage::Database {
        storage::Database::open_read_only(&self.config.db_path).unwrap()
    }

    /// Receives events until one matches; every event received is kept for the window.
    pub async fn until(&mut self, wanted: impl Fn(&PullRequestEvent) -> bool) {
        loop {
            let event = self.events.recv().await.unwrap();
            let done = matches!(&event.kind, EventKind::PullRequest(change) if wanted(change));
            self.seen.push(event);
            if done {
                return;
            }
        }
    }

    /// Hands the received events to the window and lets it read the database.
    pub fn show(&mut self, state: &mut WorkbenchState<DemoSource>) {
        state.apply_events(std::mem::take(&mut self.seen));
        settle(state);
    }

    /// Runs one agent turn on [`THREAD`] to its end.
    pub async fn run_agent(&self, request: &str) {
        let run = self
            .runtime
            .delegate_chat(THREAD, Role::Worker, request.into(), RunConfig::default())
            .unwrap();
        assert_eq!(
            self.runtime.wait(run).await,
            Ok(event_bus::AgentRunPhase::Done)
        );
    }

    /// Everything the service has published, once a marker published after it has come back.
    /// The marker is matched by its source, its code and a token unique to this call, so a
    /// diagnostic of the service's own can neither end the wait nor be taken for the marker.
    pub async fn quiet_events(&mut self) -> Vec<Event> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let token = format!(
            "marker-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        self.bus.emit(Event::new(event_bus::DiagnosticEvent {
            source: "pull-request-e2e".into(),
            severity: event_bus::DiagnosticSeverity::Info,
            code: "marker".into(),
            detail: token.clone(),
            run_id: None,
            thread_id: None,
            call_id: None,
        }));
        let mut events = Vec::new();
        loop {
            let event = self.events.recv().await.unwrap();
            if let EventKind::Diagnostic(diagnostic) = &event.kind
                && diagnostic.source == "pull-request-e2e"
                && diagnostic.code == "marker"
                && diagnostic.detail == token
            {
                return events;
            }
            events.push(event);
        }
    }

    /// Makes the scheduler go through one full round: a new thread links a pull request nobody
    /// else tracks and the answer is awaited. A fetch that was due before this call has been
    /// started by the time it returns, because one round plans every key that is due.
    pub async fn scheduler_round(&mut self) {
        self.sentinels += 1;
        let thread = format!("sentinel-{}", self.sentinels);
        let number = 9_000 + self.sentinels;
        self.source.push_ok(&key(number), running(number));
        let permit = self.permit(&thread);
        self.service
            .link(&thread, &format!("{REPOSITORY}#{number}"), 0, &permit)
            .await
            .unwrap();
        let wanted = thread.clone();
        self.until(move |event| {
            matches!(event, PullRequestEvent::Changed { thread_id, .. } if *thread_id == wanted)
        })
        .await;
    }

    /// Moves virtual time forward `step_seconds` at a time until the source has been asked for
    /// pull request `number` at least `count` times. A fetch is due a fixed time after the
    /// previous one ended, and when that was is not known here; the loop stops at the first
    /// step that reaches it.
    pub async fn advance_until_fetches(&self, number: u64, count: usize, step_seconds: u64) {
        while self.fetches_of(number) < count {
            pass(step_seconds).await;
        }
    }

    /// Waits until the facts of `number` are stored, without a clock.
    pub async fn facts_stored(&self, number: u64) -> PullRequestFacts {
        loop {
            let stored = self
                .storage
                .handle()
                .pull_request_snapshot(&key(number))
                .unwrap()
                .and_then(|snapshot| snapshot.facts);
            if let Some(facts) = stored {
                return facts;
            }
            tokio::task::yield_now().await;
        }
    }
}

/// A window over the database whose commands go to `sink`, showing `threads` (the first is
/// the current one). The reads the window starts run inline.
pub fn window(
    db_path: &Path,
    project_dir: &Path,
    host: &Arc<OwnerHost>,
    sink: Box<dyn CommandSink>,
    threads: &[&str],
) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", project_dir)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for thread in threads {
        sidebar
            .create_thread(ThreadId::new(*thread), project.clone(), "Thread")
            .unwrap();
    }
    sidebar.switch_thread(&ThreadId::new(threads[0])).unwrap();
    let probe_host = host.clone();
    let inline: gui::app::lanes::Spawner = Arc::new(|job| job());
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_provider_status(ProviderStatus::Configured)
        .with_ownership(host.clone())
        .with_command_sink(sink)
        .with_pull_requests(
            Arc::new(StoragePullRequestReader::new(db_path.to_path_buf())),
            Some(Arc::new(move |thread: &str| {
                probe_host.owned_permit(thread).is_ok()
            })),
        )
        .with_pull_request_spawner(inline)
}
