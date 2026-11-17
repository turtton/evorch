//! The evaluation jobs (ADR 0045, stage 17c) through the host's command entry:
//! `eval run`, `eval report` and `eval report --rejudge` run as counted tasks of
//! the core, and a client only starts them, follows them and cancels them.
//!
//! A test holds a job at its start with a gate it opens, or waits for the job's
//! own settled event, so no test waits for time to pass. The replays themselves
//! (a model, a workspace, a judge) are covered by `core_eval_cache`, which runs
//! them through a real core.
#[path = "memory_reads/support.rs"]
mod support;

mod common;

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use support::{World, operation, world};
use workbench_host::areas::Areas;
use workbench_host::areas::evaluation::{EvaluationArea, JobHook, JobParts, JobSeams, JobStage};
use workbench_protocol::WorkbenchError;
use workbench_protocol::commands::{Command, CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
use workbench_protocol::ids::ThreadId;
use workbench_protocol::management::EvaluationJobKind;
use workbench_protocol::namespaces::diagnostics::OperationTarget;
use workbench_protocol::namespaces::evaluation::{
    CancelEvaluationJob, EvaluationCommand, EvaluationEvent, EvaluationJobEffect,
    EvaluationJobFailure, EvaluationJobView, EvaluationQuery, EvaluationReply, EvaluationResult,
    StartEvaluationReport, StartEvaluationRun,
};
use workbench_protocol::operation::{CancelReason, OperationState};
use workbench_protocol::queries::{Query, QueryReply};

/// A gate the test opens: every job counts itself in `reached` when its work
/// begins and waits until `release`.
struct Hold {
    reached: tokio::sync::Semaphore,
    open: tokio::sync::watch::Sender<bool>,
}

impl Hold {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            reached: tokio::sync::Semaphore::new(0),
            open: tokio::sync::watch::channel(false).0,
        })
    }

    fn hook(self: &Arc<Self>) -> JobHook {
        let hold = self.clone();
        Arc::new(move |stage, _| {
            let hold = hold.clone();
            Box::pin(async move {
                assert_eq!(stage, JobStage::Started);
                hold.reached.add_permits(1);
                let _ = hold.open.subscribe().wait_for(|open| *open).await;
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        })
    }

    async fn reached(&self) {
        self.reached.acquire().await.unwrap().forget();
    }

    fn release(&self) {
        self.open.send_replace(true);
    }
}

fn setup(hook: Option<JobHook>) -> World {
    world(|parts| {
        let config = parts.dir.join("config");
        std::fs::create_dir_all(&config).unwrap();
        Areas {
            evaluation: EvaluationArea::from_job_parts(JobParts {
                tasks: parts.tasks.clone(),
                source: parts.projection.source("evaluation").unwrap(),
                root: config.join("eval"),
                config_dir: config.clone(),
                database: parts.database.clone(),
                credentials: Arc::new(sandbox::FileCredentialStore::open(&config).unwrap()),
                seams: JobSeams {
                    hook,
                    load_catalog: false,
                    ..JobSeams::default()
                },
            }),
            ..Default::default()
        }
    })
}

fn root(w: &World) -> std::path::PathBuf {
    w.dir.path().join("config").join("eval")
}

fn report_of(id: &str, targets: &[&str], rejudge: bool) -> StartEvaluationReport {
    StartEvaluationReport {
        operation_id: operation(id),
        targets: targets.iter().map(|name| (*name).to_owned()).collect(),
        rejudge,
    }
}

fn run_of(id: &str, output: Option<&str>) -> StartEvaluationRun {
    StartEvaluationRun {
        operation_id: operation(id),
        thread: ThreadId::new("thread-missing").unwrap(),
        candidate: None,
        repeat: None,
        output: output.map(str::to_owned),
        problem: None,
        verify: None,
        verify_timeout_secs: None,
        keep_workspace: false,
        report: false,
    }
}

fn send(w: &mut World, command: EvaluationCommand) -> Result<CommandReply, WorkbenchError> {
    w.host
        .handle_command(Command::Evaluation(command), common::web())
}

fn start_report(
    w: &mut World,
    start: StartEvaluationReport,
) -> Result<CommandReply, WorkbenchError> {
    send(w, EvaluationCommand::StartEvaluationReport(start))
}

fn start_run(w: &mut World, start: StartEvaluationRun) -> Result<CommandReply, WorkbenchError> {
    send(w, EvaluationCommand::StartEvaluationRun(Box::new(start)))
}

fn cancel(w: &mut World, id: &str) -> Result<CommandReply, WorkbenchError> {
    send(
        w,
        EvaluationCommand::CancelEvaluationJob(CancelEvaluationJob {
            operation_id: operation(id),
        }),
    )
}

fn view_of(reply: &CommandReply) -> EvaluationJobView {
    match reply.result.as_deref() {
        Some(CommandResult::Evaluation(EvaluationResult::Job(view))) => (**view).clone(),
        other => panic!("not an evaluation job: {other:?}"),
    }
}

fn get(w: &World, id: &str) -> Result<EvaluationJobView, WorkbenchError> {
    match w.query(Query::Evaluation(EvaluationQuery::GetEvaluationJob(
        OperationTarget {
            operation_id: operation(id),
        },
    )))? {
        QueryReply::Evaluation(EvaluationReply::Job(view)) => Ok(*view),
        other => panic!("{other:?}"),
    }
}

fn subscribe(w: &World) -> workbench_host::subscriptions::Subscription {
    w.host
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap()
}

/// The settled event of `id`, after every event the job announced before it.
fn settled(
    w: &World,
    events: &mut workbench_host::subscriptions::Subscription,
    id: &str,
) -> EvaluationJobView {
    w.rt.block_on(async {
        loop {
            let UiEvent::Evaluation(event) =
                events.recv().await.expect("the subscription is open").event
            else {
                continue;
            };
            if let EvaluationEvent::EvaluationJobSettled { view } = event
                && view.operation.operation_id.as_str() == id
            {
                return *view;
            }
        }
    })
}

fn failed_with(view: &EvaluationJobView, code: ErrorCode) {
    assert_eq!(
        view.operation.state,
        OperationState::Failed { code },
        "{view:?}"
    );
}

/// No host path and no file name of the test's directory is in the view.
fn assert_no_host_path(view: &EvaluationJobView, dir: &Path) {
    let text = serde_json::to_string(view).unwrap();
    let dir = dir.to_string_lossy();
    assert!(!text.contains(&*dir), "the view names a host path: {text}");
}

#[test]
fn a_report_of_a_directory_that_is_not_there_fails_without_naming_a_host_path() {
    let mut w = setup(None);
    let mut events = subscribe(&w);

    let reply = start_report(&mut w, report_of("nothing", &["run-a"], false)).unwrap();
    assert_eq!(reply.disposition, CommandDisposition::Accepted);
    let accepted = view_of(&reply);
    assert_eq!(accepted.kind, EvaluationJobKind::GenerateReport);
    // The handler answers at once: the work is not done inside it.
    assert!(!accepted.operation.state.is_final());

    let done = settled(&w, &mut events, "nothing");
    failed_with(&done, ErrorCode::InvalidRequest);
    assert_eq!(done.failure, Some(EvaluationJobFailure::Usage));
    // Nothing was written: the directory was never there.
    assert_eq!(done.effect, EvaluationJobEffect::None);
    assert!(done.reports.is_empty());
    assert_no_host_path(&done, w.dir.path());
    assert_eq!(get(&w, "nothing").unwrap(), done);
    w.settle();
    assert!(!w.tasks.busy());
}

#[test]
fn a_rejudge_is_told_apart_from_a_report_in_the_job_kind() {
    let mut w = setup(None);
    let mut events = subscribe(&w);
    let reply = start_report(&mut w, report_of("again", &["run-a"], true)).unwrap();
    assert_eq!(view_of(&reply).kind, EvaluationJobKind::Rejudge);
    assert_eq!(
        settled(&w, &mut events, "again").kind,
        EvaluationJobKind::Rejudge
    );
}

#[test]
fn a_report_of_a_directory_without_replays_is_a_failure_the_view_explains() {
    let mut w = setup(None);
    std::fs::create_dir_all(root(&w).join("empty")).unwrap();
    let mut events = subscribe(&w);
    start_report(&mut w, report_of("empty-report", &["empty"], false)).unwrap();
    let done = settled(&w, &mut events, "empty-report");
    assert!(
        matches!(done.operation.state, OperationState::Failed { .. }),
        "{done:?}"
    );
    assert!(done.message.is_some());
    assert_no_host_path(&done, w.dir.path());
}

#[test]
fn a_run_of_a_thread_the_core_does_not_hold_fails_and_keeps_its_output_name() {
    let mut w = setup(None);
    let mut events = subscribe(&w);
    start_run(&mut w, run_of("replay", Some("out-1"))).unwrap();
    let done = settled(&w, &mut events, "replay");
    assert!(
        matches!(done.operation.state, OperationState::Failed { .. }),
        "{done:?}"
    );
    // The output is named by its name under the root, never by a path.
    assert_eq!(done.output.as_deref(), Some("out-1"));
    assert!(done.runs.is_empty());
    assert_no_host_path(&done, w.dir.path());
    w.settle();
    assert!(!w.tasks.busy());
}

#[test]
fn the_same_id_with_the_same_request_is_answered_from_the_table() {
    let mut w = setup(None);
    let mut events = subscribe(&w);
    let first = start_report(&mut w, report_of("once", &["run-a"], false)).unwrap();
    assert_eq!(first.disposition, CommandDisposition::Accepted);
    let done = settled(&w, &mut events, "once");

    let again = start_report(&mut w, report_of("once", &["run-a"], false)).unwrap();
    assert_eq!(again.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(view_of(&again), done);
}

#[test]
fn the_same_id_with_another_request_is_a_conflict() {
    let mut w = setup(None);
    let mut events = subscribe(&w);
    start_report(&mut w, report_of("same-id", &["run-a"], false)).unwrap();
    settled(&w, &mut events, "same-id");

    let other = start_report(&mut w, report_of("same-id", &["run-b"], false)).unwrap_err();
    assert_eq!(other.code, ErrorCode::Conflict);
    let kind = start_report(&mut w, report_of("same-id", &["run-a"], true)).unwrap_err();
    assert_eq!(kind.code, ErrorCode::Conflict);
    let run = start_run(&mut w, run_of("same-id", None)).unwrap_err();
    assert_eq!(run.code, ErrorCode::Conflict);
}

#[test]
fn two_jobs_never_use_the_same_directory_at_once_and_a_cancel_frees_it() {
    let hold = Hold::new();
    let mut w = setup(Some(hold.hook()));
    let mut events = subscribe(&w);
    start_report(&mut w, report_of("first", &["run-a"], false)).unwrap();
    w.rt.block_on(hold.reached());

    // The directory is in use: by a report, and by a run that names it.
    let busy = start_report(&mut w, report_of("second", &["run-a", "run-b"], false)).unwrap_err();
    assert_eq!(busy.code, ErrorCode::Busy);
    let busy = start_run(&mut w, run_of("third", Some("run-a"))).unwrap_err();
    assert_eq!(busy.code, ErrorCode::Busy);
    // A refused start leaves no job behind: its id is still free.
    assert_eq!(get(&w, "second").unwrap_err().code, ErrorCode::NotFound);

    let asked = cancel(&mut w, "first").unwrap();
    assert!(!view_of(&asked).operation.state.is_final());
    hold.release();
    let done = settled(&w, &mut events, "first");
    assert_eq!(
        done.operation.state,
        OperationState::Cancelled {
            reason: CancelReason::Requested
        }
    );
    assert_eq!(done.effect, EvaluationJobEffect::None);

    // Cancelling a job that ended answers its final view.
    let again = cancel(&mut w, "first").unwrap();
    assert_eq!(again.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(view_of(&again), done);
    assert_eq!(
        cancel(&mut w, "never-sent").unwrap_err().code,
        ErrorCode::NotFound
    );

    // The directory is free again.
    let next = start_report(&mut w, report_of("second", &["run-a"], false)).unwrap();
    assert_eq!(next.disposition, CommandDisposition::Accepted);
    settled(&w, &mut events, "second");
    w.settle();
    assert!(!w.tasks.busy());
}

#[test]
fn a_running_job_keeps_the_host_busy_until_it_settles() {
    let hold = Hold::new();
    let mut w = setup(Some(hold.hook()));
    let mut events = subscribe(&w);
    start_report(&mut w, report_of("held", &["run-a"], false)).unwrap();
    w.rt.block_on(hold.reached());
    // The task is counted: a planned restart waits for the job.
    assert!(w.tasks.busy());
    assert!(!get(&w, "held").unwrap().operation.state.is_final());

    hold.release();
    settled(&w, &mut events, "held");
    w.settle();
    assert!(!w.tasks.busy());
}

#[test]
fn a_job_the_core_does_not_hold_is_not_found_and_says_what_a_restart_means() {
    let w = setup(None);
    let unknown = get(&w, "lost").unwrap_err();
    assert_eq!(unknown.code, ErrorCode::NotFound);
    assert!(unknown.message.contains("restart"), "{}", unknown.message);
}

#[test]
fn a_host_without_jobs_refuses_to_start_one() {
    let mut w = world(|parts| Areas {
        evaluation: EvaluationArea::from_parts(
            parts.tasks,
            parts.projection.source("evaluation").unwrap(),
            parts.dir.join("eval"),
        ),
        ..Default::default()
    });
    let refused = start_report(&mut w, report_of("none", &["run-a"], false)).unwrap_err();
    assert_eq!(refused.code, ErrorCode::UnsupportedCapability);
}
