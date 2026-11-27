//! Pull request management from the desktop window never rewrites what a running
//! conversation already sent to the provider (ADR 0043, ADR 0045).
//!
//! This is the cache contract of the embedded window's pull request tests
//! (`pull_requests_cache`, `pull_request_phase2_gui_cache`, `harness_notice_recovery`),
//! moved to the path the window has now: the real client actor (`gui::core_client`) talks
//! to the production core composition (`BootstrappedHost` with its pull request service
//! and a scripted `gh` source), the core calls the production routing and HTTP adapters,
//! and the requests are read back at the HTTP boundary of the offline mock, whose prompt
//! cache is derived from the request bytes.
//!
//! * The window links a pull request, switches auto fix on, refreshes it (twice under one
//!   operation id), resets the budget and moves the destination. Every operation is
//!   accepted and its result is waited for. The only thing that reaches the model is the
//!   one notice the core decides to send, appended after the previous answer; every
//!   request extends the one before it, and the notice is there once (or, with auto fix
//!   off, the one manual notice the person asks for, sent twice under one id).
//! * The notice is committed while no window is open (the announcement is lost to every
//!   client). A window that connects afterwards restores the history from the core: the
//!   answers come back in order, the notice is in the saved model input exactly once, and
//!   a second read and a reconnect change neither the history nor the saved input. (The
//!   core's history does not carry the notice as a card; the embedded window's card
//!   recovery has no counterpart in the protocol.)
//!
//! Every wait is for an event of the core's bus, the operation the core announces, or a
//! change of the client's state; a timeout only bounds a failing test.
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use event_bus::{Event, EventKind, LifecycleEvent, PullRequestEvent, ToolEvent};
use gui::core_client::{
    Shared,
    connection::{Action, serve},
};
use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use runtime::pull_requests::*;
use storage::pull_requests::*;
use workbench_host::bootstrap::{
    BootstrapOptions, BootstrappedHost, TestSeams, lane_pr::PullRequestAdapters,
};
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{Command, ComposerRole, SendChat};
use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
use workbench_protocol::ids::{OperationId, ProjectId, SessionId, SubmissionId, ThreadId};
use workbench_protocol::namespaces::ConversationCommand;
use workbench_protocol::namespaces::pull_requests::{
    ChangeAutoFixDestination, LinkPullRequest, PullRequestOperationView, PullRequestsCommand,
    PullRequestsEvent, PullRequestsQuery, PullRequestsReply, RefreshPullRequest,
    RequestManualNotice, ResetAutoFixBudget, SetAutoFix, ThreadPullRequest, UnlinkPullRequest,
};
use workbench_protocol::operation::OperationState;
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::wire::Revision;
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

const WAIT: Duration = Duration::from_secs(60);
const THREAD: &str = "t1";
const MARKER: &str = "[harness-notice kind=pr_checks_failed";

struct Unisolated;
impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

/// A disposable checkout whose head is the one the scripted pull request has.
fn checkout(root: &Path) -> String {
    git(root, &["init", "-b", "feature"]);
    // This local fixture is neither a repository change nor a published commit.
    git(
        root,
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    );
    git(
        root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

fn key() -> PullRequestKey {
    PullRequestKey::github("owner/repo", 7).unwrap()
}

fn failing(head: &str) -> FetchedPullRequest {
    FetchedPullRequest {
        requested: key(),
        canonical: key(),
        url: "https://github.com/owner/repo/pull/7".into(),
        display: Default::default(),
        fetched_at_ns: 1,
        facts: PullRequestFacts {
            state: PullRequestState::Open,
            base_branch: "main".into(),
            head_repository: Some("owner/repo".into()),
            head_branch: "feature".into(),
            head_sha: head.into(),
            checks: vec![PullRequestCheck {
                provider_id: "unit".into(),
                kind: CheckKind::CheckRun,
                name: "unit".into(),
                status: CheckStatus::Completed,
                conclusion: Some(CheckConclusion::Failure),
                head_sha: head.into(),
            }],
            checks_complete: true,
            review_decision: ReviewDecision::None,
        },
    }
}

fn op(id: &str) -> OperationId {
    OperationId::new(id).unwrap()
}

fn thread() -> ThreadId {
    ThreadId::new(THREAD).unwrap()
}

fn read(core: &BootstrappedHost) -> ThreadPullRequest {
    match core
        .host
        .lock()
        .unwrap()
        .handle_query(
            &SessionId::new("pr-cache").unwrap(),
            &Query::PullRequests(PullRequestsQuery::GetThreadPullRequest { thread: thread() }),
            &local_desktop(),
        )
        .unwrap()
    {
        QueryReply::PullRequests(PullRequestsReply::Thread(view)) => *view,
        other => panic!("{other:?}"),
    }
}

fn published(bus: &event_bus::EventBus, run: &str) {
    bus.emit(Event::new(ToolEvent::ToolStarted {
        tool_name: "shell".into(),
        call_id: "push".into(),
        input: Some(serde_json::json!({"command":"git push origin feature"})),
        run_id: Some(run.into()),
        parent_call_id: None,
    }));
    bus.emit(Event::new(ToolEvent::ToolCompleted {
        tool_name: "shell".into(),
        call_id: "push".into(),
        is_error: false,
        output: None,
        detail: Some(serde_json::json!({"exit_code":0})),
        run_id: Some(run.into()),
        parent_call_id: None,
    }));
    // The observer handles events in order; the durable candidate this makes is a
    // barrier after the publishing success, rather than a yield or a sleep.
    bus.emit(Event::new(ToolEvent::ToolCompleted {
        tool_name: "shell".into(),
        call_id: "marker".into(),
        is_error: false,
        output: Some("https://github.com/owner/repo/pull/8".into()),
        detail: None,
        run_id: Some(run.into()),
        parent_call_id: None,
    }));
}

fn marker_count(body: &serde_json::Value) -> usize {
    body["messages"].to_string().matches(MARKER).count()
}

/// A desktop window: the real client actor on its own connection to the core.
struct Window {
    shared: Arc<Mutex<Shared>>,
    actions: tokio::sync::mpsc::Sender<Action>,
    task: tokio::task::JoinHandle<()>,
}

impl Window {
    async fn open(socket: &Path, identity_dir: &Path) -> Self {
        let identity = core_instance::ConfigIdentity::new(identity_dir).unwrap();
        let launcher = core_launcher::Launcher::with_socket(
            identity,
            core_launcher::CoreBinary::explicit(&std::env::current_exe().unwrap()).unwrap(),
            gui::core_client::client_config(),
            socket.to_path_buf(),
        )
        .unwrap();
        let (actions, rx) = tokio::sync::mpsc::channel(64);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let task = {
            let shared = shared.clone();
            tokio::spawn(async move {
                serve(launcher, rx, shared, egui::Context::default()).await;
            })
        };
        let window = Self {
            shared,
            actions,
            task,
        };
        window.until(|state| state.connected).await;
        window
    }

    /// Until the client's state satisfies `done`.
    async fn until(&self, done: impl Fn(&Shared) -> bool) {
        let changed = self.shared.lock().unwrap().changed.clone();
        tokio::time::timeout(WAIT, async {
            loop {
                let notification = changed.notified();
                tokio::pin!(notification);
                notification.as_mut().enable();
                if done(&self.shared.lock().unwrap()) {
                    return;
                }
                notification.await;
            }
        })
        .await
        .expect("the client reaches the state");
    }

    async fn send(&self, command: Command) {
        self.actions
            .send(Action::Command {
                command,
                draft: None,
            })
            .await
            .unwrap();
    }

    /// The committed history of the thread, as the window restores it from the core.
    async fn history(&self) -> Vec<String> {
        // Forget what an earlier read left, so that only this read's answer satisfies the wait.
        self.shared.lock().unwrap().cache.history.remove(THREAD);
        self.actions
            .send(Action::History(thread(), None, false))
            .await
            .unwrap();
        self.until(|state| state.cache.history.contains_key(THREAD))
            .await;
        self.shared.lock().unwrap().cache.history[THREAD]
            .iter()
            .map(|message| serde_json::to_string(message).unwrap())
            .collect()
    }

    /// The window is closed: its actor ends, and nothing else does.
    async fn close(self) {
        self.actions.send(Action::Shutdown).await.unwrap();
        tokio::time::timeout(WAIT, self.task)
            .await
            .expect("the actor ends when the window is closed")
            .expect("the actor does not panic");
    }
}

fn chat(submission: &str, text: &str) -> SendChat {
    SendChat {
        project: ProjectId::new("p").unwrap(),
        thread: thread(),
        submission_id: SubmissionId::new(submission).unwrap(),
        composer_role: ComposerRole::Worker,
        text: text.into(),
        skills: vec![],
        attachments: vec![],
        model_preference: None,
        fork_seed: None,
    }
}

/// The run of the next turn the core completes.
async fn turn_completed(events: &mut event_bus::EventReceiver) -> String {
    tokio::time::timeout(WAIT, async {
        loop {
            match events.recv().await.unwrap().kind {
                EventKind::Lifecycle(LifecycleEvent::TurnCompleted { run_id, .. }) => {
                    return run_id;
                }
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    to: event_bus::AgentRunPhase::Error,
                    reason,
                    ..
                }) => panic!("run failed: {reason:?}"),
                _ => (),
            }
        }
    })
    .await
    .expect("a turn completes")
}

/// Sends `command` from the window and returns how its operation ended, as the core
/// announces it. The same command may be sent again: each send is answered by the one
/// operation.
async fn settle(
    window: &Window,
    events: &mut workbench_host::subscriptions::Subscription,
    command: PullRequestsCommand,
) -> PullRequestOperationView {
    let id = command.operation_id().clone();
    window.send(Command::PullRequests(command)).await;
    tokio::time::timeout(WAIT, async {
        loop {
            let event = events.recv().await.expect("the subscription is open").event;
            if let UiEvent::PullRequests(PullRequestsEvent::PullRequestOperationCompleted {
                operation,
            }) = event
                && operation.operation.operation_id == id
            {
                return operation;
            }
        }
    })
    .await
    .expect("the core announces the operation")
}

fn saved_input(db: &Path, run: &str) -> (String, String) {
    let record = storage::Database::open_read_only(db)
        .unwrap()
        .run_context(run)
        .unwrap()
        .expect("the run saved its context");
    (record.messages_json, record.config_json)
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_request_operations_from_the_window_and_one_auto_fix_notice_keep_the_wire_prefix() {
    scenario(false).await;
}

/// The same conversation with the global automatic switch off: the only notice is the manual
/// one the person asks for, sent twice under one operation id.
#[tokio::test(flavor = "multi_thread")]
async fn a_manual_notice_asked_for_twice_from_the_window_keeps_the_wire_prefix() {
    scenario(true).await;
}

async fn scenario(manual: bool) {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    let head = checkout(&root);
    let mock = StreamingMockOpenAi::spawn_with_prompt_cache_models(
        vec![
            ScriptedResponse::text_stream("one", "mock-model", ["answer-one"]),
            ScriptedResponse::text_stream("two", "mock-model", ["answer-two"]),
            ScriptedResponse::text_stream("three", "mock-model", ["answer-three"]),
        ],
        vec!["mock-model".into()],
    );
    let user = root.join("user");
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(
        user.join("config.toml"),
        format!(
            r#"version = {version}
[providers.local]
type = "openai-compatible"
base_url = "{base}"
api_key_env = "EVORCH_TEST_KEY"
models = ["mock-model"]
default_model = "mock-model"
[[routing.routes.worker]]
profile = "local"
[compaction]
context_window_tokens = 1000000
[pull_requests]
enabled = true
auto_fix_enabled = {auto}
"#,
            version = config::CURRENT_VERSION,
            base = mock.base_url(),
            auto = !manual,
        ),
    )
    .unwrap();
    let config = config::Config::load(&config::LoadOptions {
        user_config_dir: Some(user.clone()),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    let store = SidebarStore::new(user.join("sidebar.json"));
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(StateProject::new("p"), "repo", &root)
        .unwrap();
    document.sidebar.threads.push(ThreadRecord::new(
        StateThread::new(THREAD),
        StateProject::new("p"),
        "first",
    ));
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    let model = workbench_host::bootstrap::models::compose_production_model(
        &config,
        &workbench_host::bootstrap::models::ProductionModel {
            load_options: config::LoadOptions {
                user_config_dir: Some(user.clone()),
                ..Default::default()
            },
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(dir.path().join("credentials"))
                    .unwrap(),
            ),
            bus: Arc::new(event_bus::EventBus::new(4096)),
            env: Arc::new(routing::MapEnv::from_iter([(
                "EVORCH_TEST_KEY",
                "offline-test-key",
            )])),
            model_catalog_dir: Some(test_support::model_catalog_dir()),
        },
    )
    .unwrap();
    let source = Arc::new(FakePullRequestSource::new());
    let logs = Arc::new(FakePullRequestLogSource::new());
    let core = BootstrappedHost::open_with_service_seams(
        BootstrapOptions {
            config_dir: user.clone(),
            project_root: root.clone(),
            ownership_root: dir.sockets().to_path_buf(),
        },
        TestSeams {
            model,
            sandbox: Arc::new(Unisolated),
        },
        PullRequestAdapters {
            source: source.clone(),
            logs: logs.clone(),
        },
    )
    .await
    .unwrap();
    let mut turns = core.runtime.event_bus().subscribe();
    let mut bus_events = core.runtime.event_bus().subscribe();
    let mut events = core
        .host
        .lock()
        .unwrap()
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap();

    // The core serves its host on a Unix socket; windows are clients of it.
    let identity_dir = root.join("gui-identity");
    let identity = core_instance::ConfigIdentity::new(&identity_dir).unwrap();
    let instance = evorch_core::instance::Instance::acquire(
        &core_instance::LaunchGuard::acquire(&identity).unwrap(),
        dir.sockets(),
    )
    .unwrap();
    let backend = Arc::new(evorch_core::backend::HostBackend::new(
        core.host.clone(),
        instance.identity.clone(),
    ));
    let listener = instance.bind().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(async move {
        let _keep = stop;
        while let Ok((stream, _)) = listener.accept().await {
            let (backend, stopped) = (backend.clone(), stopped.clone());
            tokio::spawn(async move {
                let _ = evorch_core::unix::connection(stream, backend, stopped).await;
            });
        }
    });
    let socket = instance.socket.clone();
    let db = user.join("evorch-events.db");

    // Turn one, typed in the first window.
    let window = Window::open(&socket, &identity_dir).await;
    {
        let send = chat("00000000-0000-4000-8000-000000000009", "Start");
        core.host
            .lock()
            .unwrap()
            .reserve_new_chat_thread(&send)
            .unwrap();
        window
            .send(Command::Conversation(ConversationCommand::SendChat(send)))
            .await;
    }
    let run = turn_completed(&mut turns).await;
    let before = read(&core);

    // Link under a held fetch; the run's push is observed while it is held.
    let gate = source.push_gate(&key());
    let linked = settle(
        &window,
        &mut events,
        PullRequestsCommand::LinkPullRequest(LinkPullRequest {
            operation_id: op("link"),
            thread: thread(),
            locator: "owner/repo#7".into(),
            expected_revision: before.lifecycle_revision,
        }),
    )
    .await;
    assert_eq!(linked.operation.state, OperationState::Ready);
    gate.arrived().await;
    published(&core.runtime.event_bus(), &run);
    tokio::time::timeout(WAIT, async {
        loop {
            if matches!(
                bus_events.recv().await.unwrap().kind,
                EventKind::PullRequest(PullRequestEvent::CandidateChanged { .. })
            ) {
                break;
            }
        }
    })
    .await
    .expect("the candidate is recorded");
    assert_eq!(completions(&mock).len(), 1, "linking sends nothing");

    if !manual {
        let view = read(&core);
        let state = view.link.as_ref().expect("linked");
        let on = PullRequestsCommand::SetAutoFix(SetAutoFix {
            operation_id: op("auto-on"),
            thread: thread(),
            link_revision: state.link_revision,
            policy_revision: view
                .auto_fix
                .as_ref()
                .map_or(Revision(0), |auto| auto.policy_revision),
            enabled: true,
        });
        assert_eq!(
            settle(&window, &mut events, on).await.operation.state,
            OperationState::Ready
        );
        assert_eq!(completions(&mock).len(), 1, "a policy change sends nothing");
    }

    // The window is closed before the failing head arrives: the notice is committed with
    // no window open, so no client is told of it.
    window.close().await;
    let answered = saved_input(&db, &run);
    if manual {
        gate.release_ok(failing(&head));
        loop {
            let facts = read(&core).snapshot.and_then(|snapshot| snapshot.facts);
            if facts.is_some() {
                break;
            }
            events.recv().await.expect("the subscription is open");
        }
        assert_eq!(completions(&mock).len(), 1, "a stored head sends nothing");
    } else {
        gate.release_ok(failing(&head));
        turn_completed(&mut turns).await;
        let spent = read(&core).auto_fix.expect("auto fix state").used;
        assert_eq!(spent, 1, "one notice spent one budget slot");
    }

    // A window opened now restores the conversation from the core. The announcement of the
    // notice reached no client, and the history the core serves does not depend on it: the
    // answers are restored in order, the notice is in the saved model input exactly once,
    // and reading again or after a reconnect changes neither.
    let window = Window::open(&socket, &identity_dir).await;
    if !manual {
        let saved = saved_input(&db, &run);
        assert_eq!(
            saved.0.matches(MARKER).count(),
            1,
            "the committed notice is in the saved model input once"
        );
        assert_ne!(saved, answered, "the notice turn saved its input");
        let history = window.history().await;
        let position = |needle: &str| history.iter().position(|row| row.contains(needle));
        assert!(
            position("answer-one").unwrap() < position("answer-two").unwrap(),
            "the answers are restored in order: {history:#?}"
        );
        assert!(
            history.iter().filter(|row| row.contains(MARKER)).count() <= 1,
            "a notice shown in the history is shown once: {history:#?}"
        );
        assert_eq!(window.history().await, history, "a second read is the same");
        let projections = window.shared.lock().unwrap().projection;
        window.actions.send(Action::Reconnect).await.unwrap();
        window
            .until(|state| state.connected && state.projection > projections)
            .await;
        assert_eq!(
            window.history().await,
            history,
            "a reconnect restores the same history"
        );
        assert_eq!(
            saved_input(&db, &run),
            saved,
            "restoring the history does not rewrite the saved model input"
        );
    }

    // Refreshes (one of them sent twice under its id), a reset and a move of the
    // destination do not reach the model, and they do not make a second notice.
    source.push_ok(&key(), failing(&head));
    source.repeat_last();
    let refresh = PullRequestsCommand::RefreshPullRequest(RefreshPullRequest {
        operation_id: op("refresh-1"),
        thread: thread(),
    });
    let first = settle(&window, &mut events, refresh.clone()).await;
    assert_eq!(first.operation.state, OperationState::Ready);
    window.send(Command::PullRequests(refresh)).await;
    let other = PullRequestsCommand::RefreshPullRequest(RefreshPullRequest {
        operation_id: op("refresh-2"),
        thread: thread(),
    });
    assert_eq!(
        settle(&window, &mut events, other).await.operation.state,
        OperationState::Ready
    );
    let view = read(&core);
    let link_revision = view.link.as_ref().unwrap().link_revision;
    let policy = |view: &ThreadPullRequest| {
        view.auto_fix
            .as_ref()
            .map_or(Revision(0), |auto| auto.policy_revision)
    };
    if !manual {
        let reset = PullRequestsCommand::ResetAutoFixBudget(ResetAutoFixBudget {
            operation_id: op("reset"),
            thread: thread(),
            link_revision,
            policy_revision: policy(&view),
            confirmed_previous: None,
        });
        assert_eq!(
            settle(&window, &mut events, reset).await.operation.state,
            OperationState::Ready
        );
        let view = read(&core);
        let destination = PullRequestsCommand::ChangeAutoFixDestination(ChangeAutoFixDestination {
            operation_id: op("destination"),
            thread: thread(),
            link_revision,
            policy_revision: policy(&view),
            confirmed_previous: Some(thread()),
        });
        assert_eq!(
            settle(&window, &mut events, destination)
                .await
                .operation
                .state,
            OperationState::Ready
        );
        assert_eq!(completions(&mock).len(), 2, "commands send nothing");
    } else {
        // A manual notice asked for twice under one id is one request appended after the
        // previous answer, never two.
        let notice = PullRequestsCommand::RequestManualNotice(RequestManualNotice {
            operation_id: op("manual"),
            thread: thread(),
            link_revision,
            expected_head: head.clone(),
        });
        let requested = settle(&window, &mut events, notice.clone()).await;
        assert_eq!(requested.operation.state, OperationState::Ready);
        turn_completed(&mut turns).await;
        window.send(Command::PullRequests(notice)).await;
        // A later operation of the same window is answered after the repeated one was handled.
        let barrier = PullRequestsCommand::RefreshPullRequest(RefreshPullRequest {
            operation_id: op("refresh-3"),
            thread: thread(),
        });
        assert_eq!(
            settle(&window, &mut events, barrier).await.operation.state,
            OperationState::Ready
        );
        assert_eq!(
            completions(&mock).len(),
            2,
            "one manual notice made exactly one more request"
        );
    }

    // The link is ended from the window: the operation succeeds, the thread has no link
    // afterwards, and nothing is sent to the model for it.
    let linked = read(&core);
    assert!(linked.link.is_some());
    let unlink = PullRequestsCommand::UnlinkPullRequest(UnlinkPullRequest {
        operation_id: op("unlink"),
        thread: thread(),
        expected_revision: linked.lifecycle_revision,
    });
    assert_eq!(
        settle(&window, &mut events, unlink).await.operation.state,
        OperationState::Ready
    );
    assert!(read(&core).link.is_none(), "the link is gone");
    assert_eq!(completions(&mock).len(), 2, "ending the link sends nothing");

    // Turn three is an ordinary one, typed in the window.
    window
        .send(Command::Conversation(ConversationCommand::SendChat(chat(
            "00000000-0000-4000-8000-00000000000a",
            "Continue after the pull request commands",
        ))))
        .await;
    turn_completed(&mut turns).await;

    let requests: Vec<_> = mock
        .recorded_requests()
        .into_iter()
        .filter(|r| r.path.contains("completions"))
        .map(|r| r.body)
        .collect();
    assert_eq!(
        requests.len(),
        3,
        "one notice made exactly one extra model request"
    );
    assert_eq!(marker_count(&requests[0]), 0);
    assert_eq!(marker_count(&requests[1]), 1, "the notice is in the second");
    assert_eq!(
        marker_count(&requests[2]),
        1,
        "the third carries the same single notice"
    );
    assert_eq!(logs.requests().len(), 1, "one log request for one notice");
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
        assert!(pair[1].to_string().len() > pair[0].to_string().len());
        for field in ["model", "prompt_cache_key"] {
            assert_eq!(pair[0][field], pair[1][field], "{field}");
        }
    }

    window.close().await;
    server.abort();
    let mut core = core;
    core.runtime
        .stop(run.parse().unwrap(), runtime::StopScope::SelfOnly)
        .unwrap();
    core.runtime.wait(run.parse().unwrap()).await.unwrap();
    core.shutdown().await.unwrap();
}

fn completions(mock: &StreamingMockOpenAi) -> Vec<mock_openai::RecordedRequest> {
    mock.recorded_requests()
        .into_iter()
        .filter(|r| r.path.contains("completions"))
        .collect()
}
