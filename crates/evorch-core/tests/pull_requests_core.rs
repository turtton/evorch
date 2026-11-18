//! The `pull_requests` namespace through the production core (ADR 0045, PR 15):
//! two clients that race on one thread, a command sent again, an operation the
//! core does not know, and a fetch that is cut off by the core stopping.
//!
//! The real bootstrap, SQLite, ownership and PR service run; only the `gh`
//! source is the scripted fake. Nothing waits for time: every step is a gate, a
//! barrier or an event the core publishes.
use std::sync::{Arc, Barrier};

use async_trait::async_trait;
use runtime::pull_requests::*;
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use storage::pull_requests::*;
use storage::{Storage, StorageConfig};
use workbench_host::bootstrap::{
    BootstrapOptions, BootstrappedHost, TestSeams, lane_pr::PullRequestAdapters,
};
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{Command, CommandDisposition, CommandResult};
use workbench_protocol::envelope::{Provenance, Surface};
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
use workbench_protocol::ids::{ClientId, OperationId, SessionId, ThreadId};
use workbench_protocol::namespaces::pull_requests::{
    LinkPullRequest, PullRequestOperationView, PullRequestsCommand, PullRequestsEvent,
    PullRequestsQuery, PullRequestsReply, PullRequestsResult, RefreshPullRequest, SetAutoFix,
    ThreadPullRequest, UnlinkPullRequest,
};
use workbench_protocol::operation::OperationState;
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::wire::Revision;

const THREAD: &str = "thread-a";

struct Idle;
#[async_trait]
impl AgentModel for Idle {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        std::future::pending().await
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "pull-requests-core".into()
    }
}

struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

struct Fixture {
    dir: test_socket_dir::SplitDir,
    storage: Storage,
    source: Arc<FakePullRequestSource>,
}

impl Fixture {
    fn new() -> Self {
        let dir = test_socket_dir::SplitDir::new();
        std::fs::write(
            dir.path().join("config.toml"),
            "[pull_requests]\nenabled = true\n",
        )
        .unwrap();
        let storage = Storage::open(StorageConfig {
            db_path: dir.path().join("evorch-events.db"),
            ..Default::default()
        })
        .unwrap();
        Self {
            dir,
            storage,
            source: Arc::new(FakePullRequestSource::new()),
        }
    }

    async fn core(&self) -> BootstrappedHost {
        BootstrappedHost::open_with_service_seams(
            BootstrapOptions {
                config_dir: self.dir.path().to_path_buf(),
                ownership_root: self.dir.sockets().to_path_buf(),
                project_root: self.dir.path().canonicalize().unwrap(),
            },
            TestSeams {
                model: Arc::new(Idle),
                sandbox: Arc::new(Direct),
            },
            PullRequestAdapters {
                source: self.source.clone(),
                logs: Arc::new(FakePullRequestLogSource::new()),
            },
        )
        .await
        .unwrap()
    }
}

fn key(number: u64) -> PullRequestKey {
    PullRequestKey::github("owner/repo", number).unwrap()
}

fn fetched(number: u64) -> FetchedPullRequest {
    FetchedPullRequest {
        requested: key(number),
        canonical: key(number),
        url: format!("https://github.com/owner/repo/pull/{number}"),
        display: Default::default(),
        fetched_at_ns: 1,
        facts: PullRequestFacts {
            state: PullRequestState::Open,
            base_branch: "main".into(),
            head_repository: Some("owner/repo".into()),
            head_branch: "feature".into(),
            head_sha: "a".repeat(40),
            checks: vec![],
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

/// Two clients of the same core: the desktop window and a browser.
fn clients() -> [Provenance; 2] {
    [local_desktop(), web()]
}

fn send(
    core: &BootstrappedHost,
    command: PullRequestsCommand,
    provenance: Provenance,
) -> Result<workbench_protocol::commands::CommandReply, workbench_protocol::WorkbenchError> {
    core.host
        .lock()
        .unwrap()
        .handle_command(Command::PullRequests(command), provenance)
}

fn read(core: &BootstrappedHost) -> ThreadPullRequest {
    match core
        .host
        .lock()
        .unwrap()
        .handle_query(
            &SessionId::new("session").unwrap(),
            &Query::PullRequests(PullRequestsQuery::GetThreadPullRequest { thread: thread() }),
            &workbench_host::provenance::local_desktop(),
        )
        .unwrap()
    {
        QueryReply::PullRequests(PullRequestsReply::Thread(view)) => *view,
        other => panic!("{other:?}"),
    }
}

fn subscribe(core: &BootstrappedHost) -> workbench_host::subscriptions::Subscription {
    core.host
        .lock()
        .unwrap()
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap()
}

/// The end of each of `ids`, as the core announced it.
async fn settled(
    events: &mut workbench_host::subscriptions::Subscription,
    ids: &[&str],
) -> Vec<PullRequestOperationView> {
    let mut found: Vec<PullRequestOperationView> = Vec::new();
    while found.len() < ids.len() {
        let event = events.recv().await.expect("the subscription is open").event;
        if let UiEvent::PullRequests(PullRequestsEvent::PullRequestOperationCompleted { operation }) =
            event
            && ids.contains(&operation.operation.operation_id.as_str())
        {
            found.push(operation);
        }
    }
    found.sort_by(|a, b| {
        a.operation
            .operation_id
            .as_str()
            .cmp(b.operation.operation_id.as_str())
    });
    found
}

fn link(id: &str, locator: &str, expected: Revision) -> PullRequestsCommand {
    PullRequestsCommand::LinkPullRequest(LinkPullRequest {
        operation_id: op(id),
        thread: thread(),
        locator: locator.into(),
        expected_revision: expected,
    })
}

fn unlink(id: &str, expected: Revision) -> PullRequestsCommand {
    PullRequestsCommand::UnlinkPullRequest(UnlinkPullRequest {
        operation_id: op(id),
        thread: thread(),
        expected_revision: expected,
    })
}

fn refresh(id: &str) -> PullRequestsCommand {
    PullRequestsCommand::RefreshPullRequest(RefreshPullRequest {
        operation_id: op(id),
        thread: thread(),
    })
}

/// Both commands leave from a barrier, each from its own client.
async fn race(core: &BootstrappedHost, first: PullRequestsCommand, second: PullRequestsCommand) {
    let barrier = Arc::new(Barrier::new(2));
    let mut tasks = Vec::new();
    for (command, provenance) in [first, second].into_iter().zip(clients()) {
        let host = core.host.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::task::spawn_blocking(move || {
            barrier.wait();
            host.lock()
                .unwrap()
                .handle_command(Command::PullRequests(command), provenance)
                .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
}

fn stale() -> OperationState {
    OperationState::Failed {
        code: ErrorCode::StaleRevision,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_racing_to_link_and_unlink_end_with_one_winner_each() {
    let f = Fixture::new();
    f.source.push_ok(&key(7), fetched(7));
    f.source.push_ok(&key(8), fetched(8));
    f.source.repeat_last();
    let mut core = f.core().await;
    let _permit = core.owner.start(THREAD).unwrap();
    let mut events = subscribe(&core);
    let before = read(&core);
    assert!(before.owned_here && before.link.is_none());

    race(
        &core,
        link("link-web", "owner/repo#7", before.lifecycle_revision),
        link("link-cli", "owner/repo#8", before.lifecycle_revision),
    )
    .await;
    let linked = settled(&mut events, &["link-cli", "link-web"]).await;
    let states: Vec<_> = linked.iter().map(|v| v.operation.state).collect();
    assert!(
        states.contains(&OperationState::Ready) && states.contains(&stale()),
        "one winner and one stale loser: {states:?}"
    );
    let winner = linked
        .iter()
        .find(|v| v.operation.state == OperationState::Ready)
        .unwrap();
    let after = read(&core);
    let current = after.link.as_ref().expect("the winner's link").clone();
    assert_eq!(
        Some(after.lifecycle_revision),
        winner.lifecycle_revision,
        "the winner's receipt names the revision the next command quotes"
    );
    let expected_number = if winner.operation.operation_id.as_str() == "link-web" {
        7
    } else {
        8
    };
    assert_eq!(current.pull_request.number, expected_number);

    race(
        &core,
        unlink("unlink-web", after.lifecycle_revision),
        unlink("unlink-cli", after.lifecycle_revision),
    )
    .await;
    let unlinked = settled(&mut events, &["unlink-cli", "unlink-web"]).await;
    let states: Vec<_> = unlinked.iter().map(|v| v.operation.state).collect();
    assert_eq!(
        states
            .iter()
            .filter(|s| **s == OperationState::Ready)
            .count(),
        1,
        "{states:?}"
    );
    assert!(
        states
            .iter()
            .any(|s| matches!(s, OperationState::Failed { .. })),
        "{states:?}"
    );
    assert!(read(&core).link.is_none());
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_sent_again_is_one_operation_and_another_request_under_its_id_conflicts() {
    let f = Fixture::new();
    f.source.push_ok(&key(7), fetched(7));
    f.source.repeat_last();
    let mut core = f.core().await;
    let _permit = core.owner.start(THREAD).unwrap();
    let mut events = subscribe(&core);
    let revision = read(&core).lifecycle_revision;

    let first = send(
        &core,
        link("link-1", "owner/repo#7", revision),
        local_desktop(),
    )
    .unwrap();
    assert_eq!(first.disposition, CommandDisposition::Accepted);
    // The same request again, from another client, before and after it ended.
    let joined = send(&core, link("link-1", "owner/repo#7", revision), web()).unwrap();
    assert_eq!(joined.disposition, CommandDisposition::AlreadyApplied);
    let [done] = settled(&mut events, &["link-1"]).await.try_into().unwrap();
    assert_eq!(done.operation.state, OperationState::Ready);
    let again = send(
        &core,
        link("link-1", "owner/repo#7", revision),
        local_desktop(),
    )
    .unwrap();
    assert_eq!(again.disposition, CommandDisposition::AlreadyApplied);
    let Some(result) = again.result.as_deref() else {
        panic!("the repeated command names its operation");
    };
    let CommandResult::PullRequests(PullRequestsResult::Operation(view)) = result else {
        panic!("{result:?}");
    };
    assert_eq!(view.operation.state, OperationState::Ready);
    // Linking is applied once: the lifecycle revision moved once.
    assert_eq!(
        read(&core).link.as_ref().map(|l| l.link_revision),
        Some(Revision(1))
    );

    // Another request under the same id is refused, not merged.
    let conflict = send(&core, link("link-1", "owner/repo#8", revision), web()).unwrap_err();
    assert_eq!(conflict.code, ErrorCode::Conflict);

    // An id the core never accepted is unknown, which is not a success.
    let unknown = core
        .host
        .lock()
        .unwrap()
        .handle_query(
            &SessionId::new("session").unwrap(),
            &Query::PullRequests(PullRequestsQuery::GetPullRequestOperation {
                operation_id: op("never-sent"),
            }),
            &workbench_host::provenance::local_desktop(),
        )
        .unwrap_err();
    assert_eq!(unknown.code, ErrorCode::NotFound);
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_sent_again_while_a_fetch_runs_joins_one_follow_up() {
    let f = Fixture::new();
    let first = f.source.push_gate(&key(7));
    let follow_up = f.source.push_gate(&key(7));
    let mut core = f.core().await;
    let _permit = core.owner.start(THREAD).unwrap();
    let mut events = subscribe(&core);
    let revision = read(&core).lifecycle_revision;
    send(
        &core,
        link("link-1", "owner/repo#7", revision),
        local_desktop(),
    )
    .unwrap();
    first.arrived().await;
    // The first fetch is under way: five asks (one of them sent twice, from two
    // clients) join one follow-up, they never start parallel fetches.
    for (id, provenance) in [
        ("refresh-1", local_desktop()),
        ("refresh-1", web()),
        ("refresh-2", web()),
        ("refresh-3", local_desktop()),
    ] {
        send(&core, refresh(id), provenance).unwrap();
    }
    let ended = settled(
        &mut events,
        &["link-1", "refresh-1", "refresh-2", "refresh-3"],
    )
    .await;
    assert!(
        ended
            .iter()
            .all(|view| view.operation.state == OperationState::Ready),
        "{ended:?}"
    );
    assert_eq!(f.source.calls().len(), 1, "one fetch is under way");
    first.release_ok(fetched(7));
    follow_up.arrived().await;
    follow_up.release_ok(fetched(7));
    // Draining is the barrier after which no third fetch can have been started.
    core.shutdown().await.unwrap();
    assert_eq!(f.source.calls().len(), 2, "the asks became one follow-up");
    let operations = core.host.lock().unwrap().handle_query(
        &SessionId::new("session").unwrap(),
        &Query::PullRequests(PullRequestsQuery::ListPullRequestOperations {}),
        &workbench_host::provenance::local_desktop(),
    );
    if let Ok(QueryReply::PullRequests(PullRequestsReply::Operations { operations })) = operations {
        assert_eq!(
            operations
                .iter()
                .filter(|view| view.operation.operation_id.as_str() == "refresh-1")
                .count(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fetch_cut_off_by_the_core_stopping_is_never_committed() {
    let f = Fixture::new();
    let gate = f.source.push_gate(&key(7));
    let mut core = f.core().await;
    let _permit = core.owner.start(THREAD).unwrap();
    let mut events = subscribe(&core);
    let revision = read(&core).lifecycle_revision;
    send(
        &core,
        link("link-1", "owner/repo#7", revision),
        local_desktop(),
    )
    .unwrap();
    settled(&mut events, &["link-1"]).await;
    gate.arrived().await;
    core.shutdown().await.unwrap();
    // The answer arrives only after the core drained. It has nowhere to go.
    gate.release_ok(fetched(7));
    let snapshot = f.storage.handle().pull_request_snapshot(&key(7)).unwrap();
    assert!(
        snapshot.is_none_or(|snapshot| snapshot.facts.is_none()),
        "the stale fetch must not be stored"
    );

    // After a restart the link is still there and the facts are still unknown.
    let mut again = f.core().await;
    let view = read(&again);
    assert!(view.link.is_some(), "the link is durable");
    assert!(
        view.snapshot.as_ref().is_none_or(|s| s.facts.is_none()),
        "no facts came from the cut-off fetch"
    );
    again.shutdown().await.unwrap();
}

fn published_change(event: &UiEvent) -> bool {
    matches!(
        event,
        UiEvent::PullRequests(PullRequestsEvent::PullRequestChanged { .. })
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn the_limit_stop_is_announced_once_for_every_client_together() {
    let f = Fixture::new();
    f.source.push_ok(&key(7), fetched(7));
    f.source.repeat_last();
    let mut core = f.core().await;
    let _permit = core.owner.start(THREAD).unwrap();
    let mut desktop = subscribe(&core);
    let mut browser = subscribe(&core);
    let revision = read(&core).lifecycle_revision;
    send(
        &core,
        link("link-1", "owner/repo#7", revision),
        local_desktop(),
    )
    .unwrap();
    settled(&mut desktop, &["link-1"]).await;
    let view = read(&core);
    let link_revision = view.link.as_ref().unwrap().link_revision;
    send(
        &core,
        PullRequestsCommand::SetAutoFix(SetAutoFix {
            operation_id: op("auto-on"),
            thread: thread(),
            link_revision,
            policy_revision: view
                .auto_fix
                .as_ref()
                .map_or(Revision(0), |auto| auto.policy_revision),
            enabled: true,
        }),
        web(),
    )
    .unwrap();
    let [done] = settled(&mut desktop, &["auto-on"])
        .await
        .try_into()
        .unwrap();
    assert_eq!(done.operation.state, OperationState::Ready);

    // The third automatic notice was adopted: the budget is spent and the stop
    // stands for this thread, link and budget epoch.
    let db = rusqlite::Connection::open(f.dir.path().join("evorch-events.db")).unwrap();
    db.execute_batch(
        "UPDATE pr_auto_fix_state SET enabled=0,used=3,stop_reason='limit_reached',limit_reached_epoch=budget_epoch;
         INSERT INTO pr_auto_fix_limit_events(pr_key,budget_epoch,notice_id,at_ns) SELECT pr_key,budget_epoch,'third',1 FROM pr_auto_fix_state;",
    )
    .unwrap();
    // Several changes reach the core, as they would from several windows.
    for transition_seq in 0..6 {
        core.runtime.event_bus().emit(event_bus::Event::new(
            event_bus::PullRequestEvent::AutoFixChanged {
                thread_id: THREAD.into(),
                link_revision: link_revision.get(),
                transition_seq,
                pull_request: event_bus::PullRequestRef {
                    provider: "github".into(),
                    host: "github.com".into(),
                    repository: "owner/repo".into(),
                    number: 7,
                },
                reason: event_bus::PullRequestAutoFixChange::Adopted { automatic: true },
            },
        ));
    }
    for events in [&mut desktop, &mut browser] {
        let (mut changes, mut limits) = (0, 0);
        while changes < 6 {
            let event = events.recv().await.expect("the subscription is open").event;
            changes += usize::from(published_change(&event));
            limits += usize::from(matches!(
                event,
                UiEvent::PullRequests(PullRequestsEvent::AutoFixLimitReached { .. })
            ));
        }
        assert_eq!(limits, 1, "the stop is told exactly once to each client");
        let view = read(&core).auto_fix.unwrap();
        assert!(view.limit_reached && !view.enabled);
        assert_eq!(view.used, view.limit);
    }
    let told = f
        .storage
        .handle()
        .pr_auto_fix_limit_events(key(7), None)
        .unwrap();
    assert!(
        told[0].notified_at_ns.is_some(),
        "the core claimed the notification"
    );
    core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_request_sent_again_after_its_operation_left_the_table_does_not_run_again() {
    let f = Fixture::new();
    f.source.push_ok(&key(7), fetched(7));
    f.source.repeat_last();
    let mut core = f.core().await;
    let _permit = core.owner.start(THREAD).unwrap();
    let mut events = subscribe(&core);
    let revision = read(&core).lifecycle_revision;
    send(
        &core,
        link("link-1", "owner/repo#7", revision),
        local_desktop(),
    )
    .unwrap();
    settled(&mut events, &["link-1"]).await;
    // More finished operations than the table keeps push the first ones out.
    let ids: Vec<String> = (0..workbench_protocol::namespaces::pull_requests::MAX_OPERATIONS + 2)
        .map(|n| format!("refresh-{n}"))
        .collect();
    for id in &ids {
        send(&core, refresh(id), local_desktop()).unwrap();
        settled(&mut events, &[id]).await;
    }
    // Sent again, the evicted requests are recognised: neither runs a second
    // time, and neither is reported as done.
    for (id, command) in [
        ("link-1", link("link-1", "owner/repo#7", revision)),
        ("refresh-0", refresh("refresh-0")),
    ] {
        let again = send(&core, command, web()).unwrap();
        assert_eq!(
            again.disposition,
            CommandDisposition::AlreadyApplied,
            "{id}"
        );
        let Some(CommandResult::PullRequests(PullRequestsResult::Operation(view))) =
            again.result.as_deref()
        else {
            panic!("{id}: the repeated command names its operation");
        };
        assert!(view.outcome_unknown, "{id}");
        assert!(
            matches!(view.operation.state, OperationState::Failed { .. }),
            "{id}: {view:?}"
        );
    }
    // The id still belongs to the request it was first used for.
    let conflict = send(&core, link("link-1", "owner/repo#8", revision), web()).unwrap_err();
    assert_eq!(conflict.code, ErrorCode::Conflict);
    assert_eq!(
        read(&core).link.as_ref().map(|l| l.link_revision),
        Some(Revision(1)),
        "the link was applied once"
    );
    core.shutdown().await.unwrap();
}

/// The area remembers the id and digest of 8192 accepted commands for as long as
/// the core runs and never evicts one. Once that memory is full a new id is
/// refused before it is accepted, and every id it still knows is answered:
/// resent, it is never run again; used for another request, it conflicts.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_memory_of_accepted_commands_refuses_new_ids_and_still_answers_known_ones() {
    // Mirrors `MAX_SEEN` in the area: a mismatch fails one of the assertions below.
    const REMEMBERED: usize = 8192;
    let f = Fixture::new();
    f.source.push_ok(&key(7), fetched(7));
    f.source.repeat_last();
    let mut core = f.core().await;
    let _permit = core.owner.start(THREAD).unwrap();
    let mut events = subscribe(&core);
    let revision = read(&core).lifecycle_revision;
    send(
        &core,
        link("link-1", "owner/repo#7", revision),
        local_desktop(),
    )
    .unwrap();
    settled(&mut events, &["link-1"]).await;
    // Accepted up to the last one the memory holds, in batches the table can keep running.
    let ids: Vec<String> = (0..REMEMBERED - 1).map(|n| format!("r-{n}")).collect();
    for batch in ids.chunks(32) {
        for id in batch {
            send(&core, refresh(id), local_desktop()).unwrap();
        }
        let names: Vec<&str> = batch.iter().map(String::as_str).collect();
        settled(&mut events, &names).await;
    }
    // The memory is full: a new id is refused, and again, and nothing was run.
    for id in ["one-too-many", "and-another"] {
        let refused = send(&core, refresh(id), local_desktop()).unwrap_err();
        assert_eq!(refused.code, ErrorCode::LimitExceeded, "{id}");
    }
    // The oldest ids, long gone from the operation table, are still recognised.
    for (id, command) in [
        ("link-1", link("link-1", "owner/repo#7", revision)),
        ("r-0", refresh("r-0")),
        ("r-1", refresh("r-1")),
    ] {
        let again = send(&core, command, web()).unwrap();
        assert_eq!(
            again.disposition,
            CommandDisposition::AlreadyApplied,
            "{id}"
        );
        let Some(CommandResult::PullRequests(PullRequestsResult::Operation(view))) =
            again.result.as_deref()
        else {
            panic!("{id}: the repeated command names its operation");
        };
        assert!(view.outcome_unknown, "{id}");
        assert!(
            matches!(view.operation.state, OperationState::Failed { .. }),
            "{id}: {view:?}"
        );
    }
    // The newest id is still in the table and answers with its real outcome.
    let newest = format!("r-{}", REMEMBERED - 2);
    let again = send(&core, refresh(&newest), web()).unwrap();
    assert_eq!(again.disposition, CommandDisposition::AlreadyApplied);
    // An id belongs to the request it was first used for, full memory or not.
    let conflict = send(&core, link("link-1", "owner/repo#8", revision), web()).unwrap_err();
    assert_eq!(conflict.code, ErrorCode::Conflict);
    let conflict = send(&core, unlink("r-0", revision), web()).unwrap_err();
    assert_eq!(conflict.code, ErrorCode::Conflict);
    assert_eq!(
        read(&core).link.as_ref().map(|l| l.link_revision),
        Some(Revision(1)),
        "the link was applied once"
    );
    core.shutdown().await.unwrap();
}

/// What the host's restart gate does with pull request commands, driven by hand.
///
/// This test moves the gate itself (`transition`, `drained`); it does not run the
/// restart coordinator, and it drains no producer. It checks only the gate's
/// contract toward this area: a command already accepted keeps its restart permit
/// until its outcome is settled (even before its task was polled), new work is
/// refused once the gate leaves normal operation (`Preparing`, and `Sealing`
/// too), and a fetch that is still held when the core stops is never adopted
/// afterwards.
///
/// The coordinator's own order is: drain the producers, enter `Sealing`, then wait
/// for the host gate to drain. Here the gate is drained *before* `Sealing`, which
/// is not that order; the coordinator's ordering is proved by the restart tests of
/// `evorch-core` (the production M2 drain tests), not by this one.
#[tokio::test(flavor = "current_thread")]
async fn a_planned_restart_waits_for_accepted_commands_and_never_adopts_a_cut_off_fetch() {
    use workbench_host::bootstrap::restart::HostRestartPhase;

    let f = Fixture::new();
    let gate = f.source.push_gate(&key(7));
    let mut core = f.core().await;
    let _permit = core.owner.start(THREAD).unwrap();
    let mut events = subscribe(&core);
    let restart = core.host.lock().unwrap().service().restart_gate();
    let revision = read(&core).lifecycle_revision;

    // Accepted, answered pending, but not yet polled: nothing has run.
    send(
        &core,
        link("link-1", "owner/repo#7", revision),
        local_desktop(),
    )
    .unwrap();
    // The gate is moved directly: `Preparing` closes admission to new work while the
    // gate waits for what it already accepted. (The coordinator reaches `Sealing`
    // before it waits for the host gate; this test does not follow that order.)
    assert!(restart.transition(1, HostRestartPhase::Preparing));
    let refused = send(&core, refresh("refresh-late"), web())
        .expect_err("new work is refused while the host prepares to restart");
    assert_eq!(refused.code, ErrorCode::Busy);
    let mut drained = Box::pin(restart.drained());
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(drained.as_mut().poll(cx).is_pending()))
            .await,
        "the accepted command still holds the restart permit"
    );

    // Its outcome settles, and only then does the gate drain.
    let ended = settled(&mut events, &["link-1"]).await;
    assert_eq!(ended[0].operation.state, OperationState::Ready);
    drained.await;
    // `Sealing` after the drain (the coordinator seals first): only the refusal of
    // new work in this phase is checked here.
    assert!(restart.transition(1, HostRestartPhase::Sealing));
    let refused = send(&core, refresh("refresh-sealed"), web())
        .expect_err("a sealed host takes no new work either");
    assert_eq!(refused.code, ErrorCode::Busy);
    assert!(
        read(&core).link.is_some(),
        "the accepted command was applied"
    );

    // The fetch the link started is still held when the core stops.
    gate.arrived().await;
    core.shutdown().await.unwrap();
    gate.release_ok(fetched(7));
    let snapshot = f.storage.handle().pull_request_snapshot(&key(7)).unwrap();
    assert!(
        snapshot.is_none_or(|snapshot| snapshot.facts.is_none()),
        "a result that arrives after the seal is not adopted"
    );
    let mut again = f.core().await;
    assert!(read(&again).link.is_some(), "the link is durable");
    assert!(
        read(&again)
            .snapshot
            .as_ref()
            .is_none_or(|s| s.facts.is_none()),
        "no facts came from the cut-off fetch"
    );
    again.shutdown().await.unwrap();
}
