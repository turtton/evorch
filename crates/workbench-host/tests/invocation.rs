//! CLI invocations in the host (ADR 0045 §6, §10): what an invocation owns,
//! what ending it stops, and how a restart is kept apart from it.
//!
//! The model never answers (see `common`), so a run stays in flight until the
//! test, or the invocation's end, stops it. Every wait is for the run's own
//! terminal state; the suite watchdog detects hangs.

mod common;

use common::{cli, host_with_project, runtime, runtime_with_store, send, web};
use event_bus::AgentRunPhase;
use workbench_host::CommandHost;
use workbench_protocol::commands::{Command, CommandReply, CommandResult};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::ids::{ClientId, InvocationId, SessionId};
use workbench_protocol::namespaces::execution::invocation::{
    EndInvocation, EndReason, GetInvocation, InvocationEnded, InvocationOwnership, InvocationRole,
    InvocationStarted, InvocationState, InvocationView, StartInvocation,
};
use workbench_protocol::namespaces::{
    ConversationCommand, ExecutionCommand, ExecutionQuery, ExecutionReply, ExecutionResult,
};
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::views::InputReceiptState;

fn cli_as(name: &str) -> Provenance {
    Provenance::human(
        workbench_protocol::envelope::Surface::Cli,
        ClientId::new(name).unwrap(),
    )
}

fn id(name: &str) -> InvocationId {
    InvocationId::new(name).unwrap()
}

fn start(name: &str, root: &std::path::Path, thread: Option<&str>) -> Command {
    Command::Execution(ExecutionCommand::StartInvocation(StartInvocation {
        invocation: id(name),
        project_root: root.to_string_lossy().into_owned(),
        role: InvocationRole::Worker,
        prompt: "do the thing".into(),
        thread: thread.map(|t| workbench_protocol::ids::ThreadId::new(t).unwrap()),
    }))
}

fn end(name: &str) -> Command {
    Command::Execution(ExecutionCommand::EndInvocation(EndInvocation {
        invocation: id(name),
        reason: EndReason::Interrupted,
    }))
}

fn started(reply: CommandReply) -> InvocationStarted {
    match reply.result.map(|result| *result) {
        Some(CommandResult::Execution(ExecutionResult::InvocationStarted(started))) => started,
        other => panic!("not an invocation start: {other:?}"),
    }
}

fn ended(reply: CommandReply) -> InvocationEnded {
    match reply.result.map(|result| *result) {
        Some(CommandResult::Execution(ExecutionResult::InvocationEnded(ended))) => ended,
        other => panic!("not an invocation end: {other:?}"),
    }
}

fn view(
    host: &CommandHost,
    name: &str,
    provenance: &Provenance,
) -> Result<InvocationView, workbench_protocol::WorkbenchError> {
    let query = Query::Execution(ExecutionQuery::GetInvocation(GetInvocation {
        invocation: id(name),
    }));
    match host.handle_query(&SessionId::new("session-1").unwrap(), &query, provenance)? {
        QueryReply::Execution(ExecutionReply::Invocation(view)) => Ok(*view),
        other => panic!("not an invocation view: {other:?}"),
    }
}

fn runtime_run(run: &workbench_protocol::ids::RunId) -> runtime::RunId {
    run.as_str().parse().unwrap()
}

fn settle(rt: &tokio::runtime::Runtime, host: &CommandHost, run: runtime::RunId) -> AgentRunPhase {
    let runtime = host.service().runtime().clone();
    rt.block_on(runtime.wait(run)).expect("the run is known")
}

fn is_live(host: &CommandHost, run: runtime::RunId) -> bool {
    matches!(
        host.service().runtime().inspect_agent(run).map(|a| a.phase),
        Ok(AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting)
    )
}

#[test]
fn cold_existing_thread_invocations_reject_a_different_project() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let (_, storage) = runtime_with_store(dir.path());
    let old = common::offline_question(&rt, dir.path(), &storage);
    storage
        .handle()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "thread-1".into(),
            project_root: Some(dir.path().to_string_lossy().into_owned()),
            root_run_id: Some(old.run_id.clone()),
            ..Default::default()
        }])
        .unwrap();
    let runtime = common::runtime_on_store(dir.path(), &storage);
    let sidebar = dir.path().join("sidebar.json");
    let service =
        workbench_host::commands::CommandService::new(runtime.clone(), rt.handle().clone())
            .with_sidebar_path(sidebar.clone());
    let mut host = CommandHost::new(service);
    let _connection = host.connections().open(ClientId::new("cli-1").unwrap());
    let mut document = workspace_state::SidebarDocument::default();
    let project = workspace_state::ProjectId::new("project-1");
    document
        .sidebar
        .add_project(project, "Current project", dir.path())
        .unwrap();
    std::fs::write(&sidebar, serde_json::to_vec(&document).unwrap()).unwrap();
    let wrong = host
        .handle_command(
            start("wrong-project", other.path(), Some("thread-1")),
            cli(),
        )
        .unwrap_err();
    assert_eq!(wrong.code, ErrorCode::PermissionDenied);
    assert!(runtime.list_agents().is_empty());
    assert_eq!(host.service().restart_gate().running_invocations(), 0);
    assert_eq!(
        storage::Database::open(&storage::StorageConfig {
            db_path: dir.path().join("host.db"),
            ..Default::default()
        })
        .unwrap()
        .thread("thread-1")
        .unwrap()
        .unwrap()
        .root_run_id,
        Some(old.run_id),
        "rejected clients must not replace the saved root"
    );
}

#[test]
fn a_new_cli_thread_still_starts_without_an_existing_sidebar_trust_record() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, _storage) = runtime_with_store(dir.path());
    let service =
        workbench_host::commands::CommandService::new(runtime.clone(), rt.handle().clone())
            .with_sidebar_path(dir.path().join("not-created-sidebar.json"));
    let mut host = CommandHost::new(service);
    let _connection = host.connections().open(ClientId::new("cli-1").unwrap());
    let reply = started(
        host.handle_command(start("new-thread", dir.path(), None), cli())
            .unwrap(),
    );
    assert_eq!(reply.ownership, InvocationOwnership::OwnsRun);
    assert_eq!(
        runtime
            .thread_of_live_run(runtime_run(&reply.run))
            .as_deref(),
        Some(reply.thread.as_str())
    );
    host.handle_command(end("new-thread"), cli()).unwrap();
    assert_eq!(
        rt.block_on(runtime.wait(runtime_run(&reply.run))).unwrap(),
        AgentRunPhase::Stopped
    );
}

#[test]
fn only_the_local_command_line_may_start_an_invocation() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    let _connection = host.connections().open(ClientId::new("web-1").unwrap());

    let error = host
        .handle_command(start("inv-1", dir.path(), None), web())
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::PermissionDenied);
    assert!(host.service().runtime().list_agents().is_empty());
    assert_eq!(host.service().restart_gate().running_invocations(), 0);
}

#[test]
fn an_invocation_needs_a_live_connection() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());

    let error = host
        .handle_command(start("inv-1", dir.path(), None), cli())
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::StorageUnavailable);
    assert!(host.service().runtime().list_agents().is_empty());
}

#[test]
fn ending_an_invocation_stops_its_run_and_nothing_else() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // Ending the invocation stops its run durably, so the runtime needs its store.
    let (runtime, _storage) = runtime_with_store(dir.path());
    let mut host = host_with_project(&rt, runtime, dir.path());
    let _gui = host.connections().open(ClientId::new("web-1").unwrap());
    let _cli = host.connections().open(ClientId::new("cli-1").unwrap());
    // A conversation another client started stays live whatever the CLI does.
    let gui_run = host
        .handle_command(
            Command::Conversation(ConversationCommand::SendChat(send(
                "gui-thread",
                "sub-1",
                "hello",
            ))),
            web(),
        )
        .unwrap()
        .run_id
        .unwrap();

    let first = started(
        host.handle_command(start("inv-1", dir.path(), None), cli())
            .unwrap(),
    );

    assert_eq!(first.ownership, InvocationOwnership::OwnsRun);
    assert!(!first.already_started);
    let run = runtime_run(&first.run);
    assert_eq!(host.service().restart_gate().running_invocations(), 1);
    let running = view(&host, "inv-1", &cli()).unwrap();
    assert!(matches!(
        running.state,
        InvocationState::Running | InvocationState::WaitingForInput
    ));
    assert_eq!(running.owned_run.as_ref(), Some(&first.run));
    // Another client cannot read or end the invocation.
    assert_eq!(
        view(&host, "inv-1", &cli_as("cli-2")).unwrap_err().code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        host.handle_command(end("inv-1"), cli_as("cli-2"))
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    assert!(is_live(&host, run));

    let done = ended(host.handle_command(end("inv-1"), cli()).unwrap());

    assert!(!done.already_ended);
    assert!(done.stopped_runs >= 1);
    assert_eq!(settle(&rt, &host, run), AgentRunPhase::Stopped);
    assert!(is_live(&host, runtime_run(&gui_run)));
    assert_eq!(host.service().restart_gate().running_invocations(), 0);
    // A repeat is not an error and stops nothing more.
    let again = ended(host.handle_command(end("inv-1"), cli()).unwrap());
    assert!(again.already_ended);
    assert_eq!(
        view(&host, "inv-1", &cli()).unwrap().state,
        InvocationState::Stopped
    );
}

#[test]
fn losing_the_connection_stops_what_that_client_started_and_nothing_else() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // Stopping is durable, so the runtime needs its store.
    let (runtime, _storage) = runtime_with_store(dir.path());
    let mut host = host_with_project(&rt, runtime, dir.path());
    let gone = host.connections().open(ClientId::new("cli-1").unwrap());
    let _other = host.connections().open(ClientId::new("cli-2").unwrap());
    let lost = started(
        host.handle_command(start("inv-1", dir.path(), None), cli())
            .unwrap(),
    );
    let kept = started(
        host.handle_command(start("inv-2", dir.path(), None), cli_as("cli-2"))
            .unwrap(),
    );
    assert_eq!(host.service().restart_gate().running_invocations(), 2);

    drop(gone);

    assert_eq!(
        settle(&rt, &host, runtime_run(&lost.run)),
        AgentRunPhase::Stopped
    );
    // The run can end before the disconnect operation finishes durable Stop.
    // Read the invocation's terminal observation before checking its lease.
    assert_eq!(
        view(&host, "inv-1", &cli()).unwrap().state,
        InvocationState::Stopped
    );
    assert!(is_live(&host, runtime_run(&kept.run)));
    assert_eq!(host.service().restart_gate().running_invocations(), 1);
}

#[test]
fn the_same_request_is_not_started_twice_and_another_one_conflicts() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    let _cli = host.connections().open(ClientId::new("cli-1").unwrap());
    let first = started(
        host.handle_command(start("inv-1", dir.path(), None), cli())
            .unwrap(),
    );
    let agents = host.service().runtime().list_agents().len();

    let again = started(
        host.handle_command(start("inv-1", dir.path(), None), cli())
            .unwrap(),
    );
    let other = host
        .handle_command(start("inv-1", dir.path(), Some("elsewhere")), cli())
        .unwrap_err();

    assert!(again.already_started);
    assert_eq!(again.run, first.run);
    assert_eq!(host.service().runtime().list_agents().len(), agents);
    assert_eq!(other.code, ErrorCode::Conflict);
    assert_eq!(host.service().restart_gate().running_invocations(), 1);
}

#[test]
fn a_restart_that_claimed_admission_refuses_new_invocations() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    let _cli = host.connections().open(ClientId::new("cli-1").unwrap());
    let gate = host.service().restart_gate();
    gate.claim_restart_admission().unwrap();

    let error = host
        .handle_command(start("inv-1", dir.path(), None), cli())
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::Busy);
    assert!(host.service().runtime().list_agents().is_empty());
    // Ending is still accepted while the host prepares (it is a stop).
    let missing = host.handle_command(end("inv-1"), cli()).unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
}

#[test]
fn input_to_a_run_the_invocation_does_not_own_is_withdrawn_and_the_run_left_alone() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, _storage) = runtime_with_store(dir.path());
    let mut host = host_with_project(&rt, runtime, dir.path());
    let _gui = host.connections().open(ClientId::new("web-1").unwrap());
    let _cli = host.connections().open(ClientId::new("cli-1").unwrap());
    let gui_run = host
        .handle_command(
            Command::Conversation(ConversationCommand::SendChat(send(
                "thread-1", "sub-1", "hello",
            ))),
            web(),
        )
        .unwrap()
        .run_id
        .unwrap();

    let input = started(
        host.handle_command(start("inv-1", dir.path(), Some("thread-1")), cli())
            .unwrap(),
    );

    assert_eq!(input.ownership, InvocationOwnership::InputOnly);
    assert_eq!(input.run, gui_run);
    let queued = view(&host, "inv-1", &cli()).unwrap();
    assert_eq!(queued.state, InvocationState::Running);
    assert_eq!(queued.owned_run, None);
    assert_eq!(queued.input.unwrap().receipt, InputReceiptState::Accepted);

    let done = ended(host.handle_command(end("inv-1"), cli()).unwrap());

    assert_eq!(done.withdrawn_inputs, 1);
    assert_eq!(done.detached_inputs, 0);
    assert_eq!(done.stopped_runs, 0);
    assert!(is_live(&host, runtime_run(&gui_run)));
    let after = view(&host, "inv-1", &cli()).unwrap();
    assert_eq!(after.input.unwrap().receipt, InputReceiptState::Withdrawn);
    assert_eq!(host.service().restart_gate().running_invocations(), 0);
}
