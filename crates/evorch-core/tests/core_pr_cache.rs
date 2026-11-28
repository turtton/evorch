//! Pull request management through the production core never rewrites what a
//! running conversation already sent to the provider (ADR 0045, PR 15).
//!
//! One conversation runs on the production wire (routing, HTTP) with the shared
//! input-derived cache fixture. Between its turns the person links a pull
//! request, switches auto fix on, refreshes it (twice under one operation id,
//! then again), resets the budget and moves the destination, all through the
//! `pull_requests` commands. The only thing that reaches the model is the one
//! automatic notice the core decides to send, as an append after the previous
//! answer. Every request must extend the one before it with the same tools,
//! model and affinity key, the notice must appear once however many times the
//! pull request is refreshed, and the provider's cache must be reused.
//!
//! Nothing waits for time: each step is a gate of the scripted `gh` source, an
//! operation the core announces or a lifecycle event.
use std::path::Path;
use std::sync::Arc;

use event_bus::{AgentRunPhase, Event, EventKind, ProviderEvent, PullRequestEvent, ToolEvent};
use runtime::pull_requests::*;
use storage::pull_requests::*;
use workbench_host::bootstrap::{
    BootstrapOptions, BootstrappedHost, TestSeams, lane_pr::PullRequestAdapters,
};
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{
    Command, CommandDisposition, CommandResult, ComposerRole, SendChat,
};
use workbench_protocol::envelope::{Provenance, Surface};
use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
use workbench_protocol::ids::{
    ClientId, OperationId, ProjectId, SessionId, SubmissionId, ThreadId,
};
use workbench_protocol::namespaces::pull_requests::{
    ChangeAutoFixDestination, LinkPullRequest, PullRequestOperationView, PullRequestsCommand,
    PullRequestsEvent, PullRequestsQuery, PullRequestsReply, PullRequestsResult,
    RefreshPullRequest, RequestManualNotice, ResetAutoFixBudget, SetAutoFix, ThreadPullRequest,
};
use workbench_protocol::operation::OperationState;
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::wire::Revision;
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

#[allow(dead_code)]
#[path = "../../runtime/tests/mcp_cache_support/mod.rs"]
mod cache;

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

fn web() -> Provenance {
    Provenance::human(Surface::Web, ClientId::new("web-1").unwrap())
}

fn read(core: &BootstrappedHost) -> ThreadPullRequest {
    match core
        .host
        .lock()
        .unwrap()
        .handle_query(
            &SessionId::new("pr-cache").unwrap(),
            &Query::PullRequests(PullRequestsQuery::GetThreadPullRequest { thread: thread() }),
            &workbench_host::provenance::local_desktop(),
        )
        .unwrap()
    {
        QueryReply::PullRequests(PullRequestsReply::Thread(view)) => *view,
        other => panic!("{other:?}"),
    }
}

/// Sends `command` and returns how its operation ended, as the core announces
/// it. The same command may be sent again: each send is answered by the one
/// operation.
async fn settle(
    core: &BootstrappedHost,
    events: &mut workbench_host::subscriptions::Subscription,
    command: PullRequestsCommand,
    provenance: Provenance,
) -> PullRequestOperationView {
    let id = command.operation_id().clone();
    core.host
        .lock()
        .unwrap()
        .handle_command(Command::PullRequests(command), provenance)
        .unwrap();
    loop {
        let event = events.recv().await.expect("the subscription is open").event;
        if let UiEvent::PullRequests(PullRequestsEvent::PullRequestOperationCompleted { operation }) =
            event
            && operation.operation.operation_id == id
        {
            return operation;
        }
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

fn marker_count(request: &serde_json::Value) -> usize {
    request["messages"].to_string().matches(MARKER).count()
}

#[tokio::test(flavor = "multi_thread")]
async fn pull_request_commands_and_one_auto_fix_notice_keep_the_sent_prefix_on_the_production_wire()
{
    scenario(false).await;
}

/// The same conversation with the global automatic switch off: the only notice
/// is the manual one the person asks for, sent twice under one operation id.
#[tokio::test(flavor = "multi_thread")]
async fn a_manual_notice_asked_for_twice_keeps_the_sent_prefix_on_the_production_wire() {
    scenario(true).await;
}

async fn scenario(manual: bool) {
    let project = cache::Project::new();
    let provider = cache::Provider::new(false).await;
    let config = cache::configuration(&provider);
    let root = project.root().canonicalize().unwrap();
    let head = checkout(&root);
    let user = root.join("user");
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(
        user.join("config.toml"),
        format!(
            "version = {}\n[pull_requests]\nenabled = true\nauto_fix_enabled = {}\n",
            config::CURRENT_VERSION,
            !manual
        ),
    )
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
    let provider_bus = Arc::new(event_bus::EventBus::new(4096));
    let mut provider_events = provider_bus.subscribe();
    let model = workbench_host::bootstrap::models::compose_production_model(
        &config,
        &workbench_host::bootstrap::models::ProductionModel {
            load_options: config::LoadOptions {
                user_config_dir: Some(user.clone()),
                ..Default::default()
            },
            credential_store: project.secrets.clone(),
            bus: provider_bus.clone(),
            env: Arc::new(routing::MapEnv::from_iter([(
                "MCP_CACHE_KEY",
                "fixture-key",
            )])),
            model_catalog_dir: Some(test_support::model_catalog_dir()),
        },
    )
    .unwrap();
    let source = Arc::new(FakePullRequestSource::new());
    let logs = Arc::new(FakePullRequestLogSource::new());
    let mut core = BootstrappedHost::open_with_service_seams(
        BootstrapOptions {
            config_dir: user.clone(),
            project_root: root.clone(),
            ownership_root: project.sockets().to_path_buf(),
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
    let mut h = cache::Harness {
        runtime: core.runtime.clone(),
        receiver: core.runtime.event_bus().subscribe(),
    };
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

    // Turn one.
    provider.enqueue(vec![cache::answer()]);
    let run: runtime::RunId = {
        let send = SendChat {
            project: ProjectId::new("p").unwrap(),
            thread: thread(),
            submission_id: SubmissionId::new("00000000-0000-4000-8000-000000000009").unwrap(),
            composer_role: ComposerRole::Worker,
            text: cache::GOAL.into(),
            skills: vec![],
            attachments: vec![],
            model_preference: None,
            fork_seed: None,
        };
        let mut host = core.host.lock().unwrap();
        host.reserve_new_chat_thread(&send).unwrap();
        host.handle_command(
            Command::Conversation(
                workbench_protocol::namespaces::ConversationCommand::SendChat(send),
            ),
            local_desktop(),
        )
        .unwrap()
        .run_id
        .unwrap()
        .as_str()
        .parse()
        .unwrap()
    };
    cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
    assert_eq!(provider.requests().len(), 1);
    let before = read(&core);

    // Link under a held fetch; the run's push is observed while it is held.
    let gate = source.push_gate(&key());
    let link = PullRequestsCommand::LinkPullRequest(LinkPullRequest {
        operation_id: op("link"),
        thread: thread(),
        locator: "owner/repo#7".into(),
        expected_revision: before.lifecycle_revision,
    });
    let linked = settle(&core, &mut events, link, local_desktop()).await;
    assert_eq!(linked.operation.state, OperationState::Ready);
    gate.arrived().await;
    published(&core.runtime.event_bus(), &run.to_string());
    loop {
        if matches!(
            bus_events.recv().await.unwrap().kind,
            EventKind::PullRequest(PullRequestEvent::CandidateChanged { .. })
        ) {
            break;
        }
    }
    assert_eq!(provider.requests().len(), 1, "linking sends nothing");

    // Auto fix is switched on from the other client while the fetch is held.
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
            settle(&core, &mut events, on, web()).await.operation.state,
            OperationState::Ready
        );
        assert_eq!(
            provider.requests().len(),
            1,
            "a policy change sends nothing"
        );
    }

    if manual {
        // The failing head is stored; with the switch off nothing is sent.
        gate.release_ok(failing(&head));
        loop {
            let facts = read(&core).snapshot.and_then(|snapshot| snapshot.facts);
            if facts.is_some() {
                break;
            }
            events.recv().await.expect("the subscription is open");
        }
        assert_eq!(provider.requests().len(), 1, "a stored head sends nothing");
    } else {
        // The failing head arrives: the core, and only the core, sends one notice.
        provider.enqueue(vec![cache::answer()]);
        gate.release_ok(failing(&head));
        cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        let spent = read(&core).auto_fix.expect("auto fix state").used;
        assert_eq!(spent, 1, "one notice spent one budget slot");
    }

    // Refreshes (one of them sent twice under its id), a reset and a move of the
    // destination do not reach the model, and they do not make a second notice.
    source.push_ok(&key(), failing(&head));
    source.repeat_last();
    let refresh = PullRequestsCommand::RefreshPullRequest(RefreshPullRequest {
        operation_id: op("refresh-1"),
        thread: thread(),
    });
    let first = settle(&core, &mut events, refresh.clone(), local_desktop()).await;
    // The same request again, from the other client, is the same operation: the
    // core answers with it and does not run it a second time.
    let again = core
        .host
        .lock()
        .unwrap()
        .handle_command(Command::PullRequests(refresh), web())
        .unwrap();
    assert_eq!(again.disposition, CommandDisposition::AlreadyApplied);
    let Some(CommandResult::PullRequests(PullRequestsResult::Operation(again))) =
        again.result.as_deref()
    else {
        panic!("the repeated command names its operation");
    };
    assert_eq!(first.operation.state, OperationState::Ready);
    assert_eq!(again.operation.state, OperationState::Ready);
    assert_eq!(first.operation.operation_id, again.operation.operation_id);
    let other = PullRequestsCommand::RefreshPullRequest(RefreshPullRequest {
        operation_id: op("refresh-2"),
        thread: thread(),
    });
    assert_eq!(
        settle(&core, &mut events, other, web())
            .await
            .operation
            .state,
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
            settle(&core, &mut events, reset, local_desktop())
                .await
                .operation
                .state,
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
            settle(&core, &mut events, destination, web())
                .await
                .operation
                .state,
            OperationState::Ready
        );
        assert_eq!(provider.requests().len(), 2, "commands send nothing");
    } else {
        // A manual notice asked for twice under one id (from two clients) is one
        // request appended after the previous answer, never two.
        provider.enqueue(vec![cache::answer()]);
        let notice = PullRequestsCommand::RequestManualNotice(RequestManualNotice {
            operation_id: op("manual"),
            thread: thread(),
            link_revision,
            expected_head: head.clone(),
        });
        let requested = settle(&core, &mut events, notice.clone(), local_desktop()).await;
        assert_eq!(requested.operation.state, OperationState::Ready);
        cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;
        let resent = core
            .host
            .lock()
            .unwrap()
            .handle_command(Command::PullRequests(notice), web())
            .unwrap();
        assert_eq!(resent.disposition, CommandDisposition::AlreadyApplied);
        assert_eq!(
            provider.requests().len(),
            2,
            "one manual notice made exactly one more request"
        );
    }

    // Turn three is an ordinary one.
    provider.enqueue(vec![cache::answer()]);
    core.runtime
        .send_message(run, "Continue after the pull request commands".into())
        .unwrap();
    cache::through(&mut h.receiver, run, AgentRunPhase::Waiting).await;

    let requests = provider.requests();
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
    cache::assert_frozen(&provider, &requests);
    for pair in requests.windows(2) {
        for field in ["model", "prompt_cache_key"] {
            assert_eq!(pair[0][field], pair[1][field], "{field}");
        }
    }
    let mut completions = Vec::new();
    while completions.len() < 3 {
        let event = provider_events.recv().await.unwrap();
        if matches!(
            event.kind,
            EventKind::Provider(ProviderEvent::RequestCompleted { .. })
        ) {
            completions.push(event);
        }
    }
    cache::assert_usage_reuse(&completions, 3);
    provider.assert_drained();
    core.runtime
        .stop(run, runtime::StopScope::SelfOnly)
        .unwrap();
    core.runtime.wait(run).await.unwrap();
    core.shutdown().await.unwrap();
}
