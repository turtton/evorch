//! The evaluation jobs of the core (ADR 0045, stage 17c): `eval run`, `eval
//! report` and `eval report --rejudge` run here, in the process that owns the
//! configuration, never in the client that asked.
//!
//! * **Handler and task.** A start only consults the table, inserts the
//!   pending job and hands the work to a counted task that keeps the command's
//!   restart permit until the job is settled. The handler never waits for a
//!   replay. The task is counted, so the host's busy decision sees a running
//!   evaluation without extra wiring.
//! * **Names, not paths.** A job names directories by their name under the
//!   evaluation root; the core joins them to the root it owns and refuses a
//!   name that is not a plain directory there. The overlay of a run arrives as
//!   text and is handed to the runner through a private file that the job
//!   removes when it ends.
//! * **The source database.** The runner reads the thread from the database the
//!   core serves (read-only, as the Evaluator does); no client names one.
//! * **Idempotency and exclusion.** The same id with the same request is
//!   answered from the table; another request under the id is a `Conflict`.
//!   Jobs run side by side (the Evaluator starts a group of runs), except that
//!   two jobs never use the same directory name at once: that start is `Busy`.
//! * **Cancel and stop.** A requested cancel ends the job `cancelled(requested)`,
//!   a stop of the host `cancelled(shutdown)`. Both drop the job's work where it
//!   is: files already written stay and the workspaces are removed with
//!   `evorch eval clean`. A task dropped without an outcome settles as
//!   `cancelled(shutdown)`, never as a success.
//! * **Effect.** What the job left is read from the directory when it settles
//!   (a run that wrote anything into its output is `partial`); a job whose work
//!   panicked says `unknown`.
//! * **Secrets.** A failure message is the runner's own text with every path
//!   the core knows replaced and secrets masked, cut to a bound.
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use eval::output::PinnedDir;
use eval::runner::ModelFactory;
use eval::{EvalArgs, EvalError, FixedModels, Isolation, ReportDeps, RunReport, RunnerDeps};
use tokio::sync::watch;
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::events::UiEvent;
use workbench_protocol::ids::{ClientId, OperationId};
use workbench_protocol::limits::{MAX_NAME_BYTES, MAX_TITLE_BYTES};
use workbench_protocol::management::EvaluationJobKind;
use workbench_protocol::namespaces::evaluation::{
    EvaluationEvent, EvaluationJobEffect, EvaluationJobFailure, EvaluationJobView,
    EvaluationRunLine, EvaluationRunStatus, MAX_EVALUATION_JOBS, MAX_JOB_MESSAGE_BYTES,
    MAX_JOB_RUNS, MAX_JOB_WARNINGS, StartEvaluationReport, StartEvaluationRun,
};
use workbench_protocol::operation::{CancelReason, OperationState, OperationView};

use crate::areas::settings::operations::{Lookup, Operations, Tracked, digest_of};
use crate::areas::tasks::SpawnError;
use crate::areas::{AreaTasks, ProjectionSource, StopSignal};
use crate::bootstrap::BootstrapError;
use crate::bootstrap::restart::HostCommandPermit;
use crate::queries::{error, masked};

/// Where a test may hold a job.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStage {
    /// The job's work has begun; nothing was done. A cancel or a stop that
    /// comes while the hook holds the job drops the work here.
    Started,
}

/// A seam for tests: awaited at each [`JobStage`] of every job. Production
/// installs none.
#[doc(hidden)]
pub type JobHook =
    Arc<dyn Fn(JobStage, &OperationId) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// What a job asks for its price catalog instead of loading it.
#[doc(hidden)]
pub type CatalogSource = Arc<dyn Fn() -> Option<Arc<catalog::ModelCatalog>> + Send + Sync>;

/// The test seams of the jobs; the default is production behaviour.
#[doc(hidden)]
#[derive(Clone)]
pub struct JobSeams {
    pub hook: Option<JobHook>,
    /// How the replays isolate their tools.
    pub isolation: Isolation,
    /// Models that replace the configured providers of the replays.
    pub models: Option<FixedModels>,
    /// A judge that replaces the configured one.
    pub judge: Option<ModelFactory>,
    /// Load (and refresh) the models.dev catalog the prices come from. A test
    /// turns it off so that no job reaches for the network.
    pub load_catalog: bool,
    /// Replaces the catalog load, asked once for each job.
    pub catalog_source: Option<CatalogSource>,
}

impl Default for JobSeams {
    fn default() -> Self {
        Self {
            hook: None,
            isolation: Isolation::Production,
            models: None,
            judge: None,
            load_catalog: true,
            catalog_source: None,
        }
    }
}

/// What the jobs need of the host, all of it owned by the bootstrap.
pub struct JobParts {
    pub tasks: AreaTasks,
    pub source: ProjectionSource,
    /// The evaluation root: `<config dir>/eval`.
    pub root: PathBuf,
    pub config_dir: PathBuf,
    /// The event database the replayed threads are read from.
    pub database: PathBuf,
    pub credentials: Arc<dyn sandbox::CredentialStore>,
    pub seams: JobSeams,
}

/// An accepted request.
pub struct Accepted {
    pub view: EvaluationJobView,
    /// The request was already accepted under this id: nothing new runs.
    pub already_applied: bool,
}

pub(super) struct Jobs {
    state: Mutex<State>,
    parts: JobParts,
}

struct State {
    table: Operations<EvaluationJobView>,
    running: HashMap<OperationId, Running>,
}

struct Running {
    cancel: watch::Sender<bool>,
    /// The client that started the job; it stops with that client's last
    /// connection.
    owner: Option<ClientId>,
    /// The directory names the job uses.
    names: Vec<String>,
}

impl Tracked for EvaluationJobView {
    fn operation(&self) -> &OperationView {
        &self.operation
    }

    fn operation_mut(&mut self) -> &mut OperationView {
        &mut self.operation
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a started job does, resolved by the handler.
enum Work {
    Run(Box<StartEvaluationRun>),
    Report { targets: Vec<String>, rejudge: bool },
}

/// How a job ended.
enum Ended {
    Completed,
    Failed {
        failure: EvaluationJobFailure,
        code: ErrorCode,
        message: String,
        /// The work broke off and does not say what it wrote.
        unknown: bool,
    },
    Cancelled(CancelReason),
}

impl Ended {
    fn failed(failure: EvaluationJobFailure, code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Failed {
            failure,
            code,
            message: message.into(),
            unknown: false,
        }
    }

    fn usage(message: impl Into<String>) -> Self {
        Self::failed(
            EvaluationJobFailure::Usage,
            ErrorCode::InvalidRequest,
            message,
        )
    }

    fn io(message: impl Into<String>) -> Self {
        Self::failed(
            EvaluationJobFailure::Io,
            ErrorCode::StorageUnavailable,
            message,
        )
    }

    fn internal() -> Self {
        Self::Failed {
            failure: EvaluationJobFailure::Internal,
            code: ErrorCode::Internal,
            message: "the evaluation task did not finish".into(),
            unknown: true,
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn unknown() -> WorkbenchError {
    error(
        ErrorCode::NotFound,
        "unknown evaluation job; if it was started before a restart, look at the evaluation directory before running it again",
    )
}

fn initial_view(
    id: &OperationId,
    kind: EvaluationJobKind,
    revision: workbench_protocol::wire::Revision,
) -> EvaluationJobView {
    EvaluationJobView {
        operation: OperationView {
            operation_id: id.clone(),
            state: OperationState::Pending,
            revision,
            result: None,
        },
        kind,
        runs: Vec::new(),
        runs_omitted: 0,
        output: None,
        reports: Vec::new(),
        cross_thread: None,
        effect: EvaluationJobEffect::None,
        failure: None,
        message: None,
        warnings: Vec::new(),
    }
}

impl Jobs {
    pub(super) fn new(parts: JobParts) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                table: Operations::new(MAX_EVALUATION_JOBS),
                running: HashMap::new(),
            }),
            parts,
        })
    }

    pub(super) fn start_run(
        self: &Arc<Self>,
        start: &StartEvaluationRun,
        owner: Option<&ClientId>,
        permit: HostCommandPermit,
    ) -> Result<Accepted, WorkbenchError> {
        let digest = digest_of(&[b"run", &encode(start)?]);
        let names = start.output.iter().cloned().collect();
        self.accept(
            JobRequest {
                id: &start.operation_id,
                kind: EvaluationJobKind::Run,
                digest,
                names,
            },
            Work::Run(Box::new(start.clone())),
            owner,
            permit,
        )
    }

    pub(super) fn start_report(
        self: &Arc<Self>,
        start: &StartEvaluationReport,
        owner: Option<&ClientId>,
        permit: HostCommandPermit,
    ) -> Result<Accepted, WorkbenchError> {
        let digest = digest_of(&[b"report", &encode(start)?]);
        self.accept(
            JobRequest {
                id: &start.operation_id,
                kind: start.kind(),
                digest,
                names: start.targets.clone(),
            },
            Work::Report {
                targets: start.targets.clone(),
                rejudge: start.rejudge,
            },
            owner,
            permit,
        )
    }

    fn accept(
        self: &Arc<Self>,
        JobRequest {
            id,
            kind,
            digest,
            names,
        }: JobRequest<'_>,
        work: Work,
        owner: Option<&ClientId>,
        permit: HostCommandPermit,
    ) -> Result<Accepted, WorkbenchError> {
        let (view, cancel) = {
            let mut state = lock(&self.state);
            match state.table.lookup(id, &digest) {
                Lookup::Same(view) => {
                    return Ok(Accepted {
                        view,
                        already_applied: true,
                    });
                }
                Lookup::Conflict => {
                    return Err(WorkbenchError::new(
                        ErrorCode::Conflict,
                        "this operation id was used for a different request",
                        RetryAdvice::Never,
                    ));
                }
                Lookup::New => {}
            }
            if state
                .running
                .values()
                .any(|running| running.names.iter().any(|name| names.contains(name)))
            {
                return Err(WorkbenchError::new(
                    ErrorCode::Busy,
                    "another evaluation job is using this directory; read it with get_evaluation_job",
                    RetryAdvice::UserActionRequired,
                ));
            }
            // A full table never drops an entry: dropping a finished one would
            // let its id run again when a client resends it.
            if state.table.list().len() >= MAX_EVALUATION_JOBS {
                return Err(WorkbenchError::new(
                    ErrorCode::LimitExceeded,
                    "the core holds as many evaluation jobs as it can remember; restart the core to start a new one",
                    RetryAdvice::UserActionRequired,
                ));
            }
            let revision = state.table.next_revision();
            let mut view = initial_view(id, kind, revision);
            if let Work::Run(run) = &work {
                view.output.clone_from(&run.output);
            }
            state.table.insert(view.clone(), digest)?;
            let (cancel, observed) = watch::channel(false);
            state.running.insert(
                id.clone(),
                Running {
                    cancel,
                    owner: owner.cloned(),
                    names,
                },
            );
            (view, observed)
        };
        let this = self.clone();
        let job = id.clone();
        let spawned =
            self.parts
                .tasks
                .spawn_counted_tracked("evaluation", Some(permit), move |stop| {
                    this.run(job, work, stop, cancel)
                });
        if let Err(refused) = spawned {
            let mut state = lock(&self.state);
            state.table.forget(id);
            state.running.remove(id);
            let (code, advice) = match refused {
                SpawnError::Closed => (ErrorCode::Draining, RetryAdvice::AfterReconnect),
                SpawnError::Full => (ErrorCode::Busy, RetryAdvice::UserActionRequired),
            };
            return Err(WorkbenchError::new(code, refused.to_string(), advice));
        }
        Ok(Accepted {
            view,
            already_applied: false,
        })
    }

    /// Ask a running job to stop where it is; the final view of one that
    /// already ended. The job settles by itself.
    ///
    /// # Errors
    /// `NotFound` for a job the core does not hold.
    pub(super) fn cancel(&self, id: &OperationId) -> Result<Accepted, WorkbenchError> {
        let state = lock(&self.state);
        let view = state.table.get(id).cloned().ok_or_else(unknown)?;
        let already_applied = match state.running.get(id) {
            Some(running) => running.cancel.send_replace(true),
            None => true,
        };
        Ok(Accepted {
            view,
            already_applied,
        })
    }

    /// Ask every running job `client` started to stop, as a cancel does. A
    /// job of another client, or one nobody owns, is left alone.
    pub(super) fn stop_jobs_of(&self, client: &ClientId) {
        for running in lock(&self.state).running.values() {
            if running.owner.as_ref() == Some(client) {
                running.cancel.send_replace(true);
            }
        }
    }

    /// One job, running or finished.
    ///
    /// # Errors
    /// `NotFound` for a job the core does not hold.
    pub(super) fn get(&self, id: &OperationId) -> Result<EvaluationJobView, WorkbenchError> {
        lock(&self.state).table.get(id).cloned().ok_or_else(unknown)
    }

    async fn at(&self, stage: JobStage, id: &OperationId) {
        if let Some(hook) = &self.parts.seams.hook {
            hook(stage, id).await;
        }
    }

    async fn run(
        self: Arc<Self>,
        id: OperationId,
        work: Work,
        mut stop: StopSignal,
        cancel: watch::Receiver<bool>,
    ) -> Result<(), BootstrapError> {
        let mut guard = Settle {
            jobs: self.clone(),
            id: id.clone(),
            settled: false,
        };
        // The work is a task of its own so that a panic in it is seen as one
        // (`unknown`). A stop is asked of the work, which stops what it
        // started (the replay's runs and their processes) and ends; the task
        // is dropped where it stands only when it does not within the grace.
        let (stop_work, work_cancel) = watch::channel(false);
        let mut handle = tokio::spawn(self.clone().work(id.clone(), work, work_cancel));
        let _abort = AbortOnDrop(handle.abort_handle());
        let reason = tokio::select! {
            biased;
            () = stop.stopped() => Some(CancelReason::Shutdown),
            () = cancelled(&cancel) => Some(CancelReason::Requested),
            joined = &mut handle => {
                guard.finish(match joined {
                    Ok(ended) => ended,
                    Err(error) if error.is_cancelled() => Ended::Cancelled(CancelReason::Shutdown),
                    Err(_) => Ended::internal(),
                });
                return Ok(());
            }
        };
        let reason = reason.unwrap_or(CancelReason::Shutdown);
        stop_work.send_replace(true);
        if tokio::time::timeout(STOP_GRACE, &mut handle).await.is_err() {
            handle.abort();
            let _ = (&mut handle).await;
        }
        let ended = Ended::Cancelled(reason);
        guard.finish(ended);
        Ok(())
    }

    async fn work(
        self: Arc<Self>,
        id: OperationId,
        work: Work,
        cancel: watch::Receiver<bool>,
    ) -> Ended {
        self.at(JobStage::Started, &id).await;
        match work {
            Work::Run(run) => self.run_job(&id, *run, cancel).await,
            Work::Report { targets, rejudge } => {
                // A report has no process to stop: dropping it between its
                // awaits leaves only whole files, and each finished one is
                // already recorded in the view.
                tokio::select! {
                    biased;
                    () = cancelled(&cancel) => Ended::Cancelled(CancelReason::Requested),
                    ended = self.report_job(&id, targets, rejudge) => ended,
                }
            }
        }
    }

    // ---- run ----------------------------------------------------------------

    async fn run_job(
        &self,
        id: &OperationId,
        run: StartEvaluationRun,
        cancel: watch::Receiver<bool>,
    ) -> Ended {
        let root = self.parts.root.clone();
        let thread = run.thread.as_str().to_owned();
        let (output, candidate) = (run.output.clone(), run.candidate.clone());
        let prepared =
            tokio::task::spawn_blocking(move || prepare_run(&root, &thread, output, candidate))
                .await;
        let prepared = match prepared {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(ended)) => return ended,
            Err(_) => return Ended::internal(),
        };
        self.set_output(id, &prepared.name);
        let args = EvalArgs {
            thread_id: run.thread.as_str().to_owned(),
            candidate: prepared.candidate.as_ref().map(|file| file.0.clone()),
            repeat: run.repeat,
            output: Some(prepared.dir.clone()),
            db: Some(self.parts.database.clone()),
            user_config_dir: Some(self.parts.config_dir.clone()),
            problem: run.problem.clone(),
            verify: run.verify.clone(),
            verify_timeout: run
                .verify_timeout_secs
                .map(|secs| Duration::from_secs(u64::from(secs))),
            keep_workspace: run.keep_workspace,
        };
        let deps = RunnerDeps {
            env: Arc::new(routing::ProcessEnv),
            isolation: self.parts.seams.isolation,
            models: self.parts.seams.models.clone(),
            credentials: Some(self.parts.credentials.clone()),
            catalog: self.catalog().await,
            cancel: Some(cancel.clone()),
        };
        let result = eval::runner::run_with_cleanup(
            args,
            deps,
            |report| self.record_run(id, report),
            |dir, cleanup| self.record_cleanup(id, dir, cleanup),
        )
        .await;
        let candidate_path = prepared.candidate.as_ref().map(|file| file.0.clone());
        // The overlay is read: the file is no longer needed.
        drop(prepared.candidate);
        let scrub = self.scrubber(candidate_path.as_deref());
        if *cancel.borrow() {
            // The runs were stopped and awaited by the runner.
            return Ended::Cancelled(CancelReason::Requested);
        }
        let preflight = match result {
            Ok(_) => None,
            Err(EvalError::Preflight { reason, .. }) => Some(scrub.text(&format!(
                "evaluation preflight failed: {reason}; its evidence is in the output directory"
            ))),
            Err(error) => return failed_run(&error, &scrub),
        };
        if run.report
            && let Err(error) = tokio::select! {
                biased;
                () = cancelled(&cancel) => return Ended::Cancelled(CancelReason::Requested),
                made = self.make_reports(id, std::slice::from_ref(&prepared.name), false) => made,
            }
        {
            let mut ended = failed_report(&error, &scrub, false);
            if let (Some(text), Ended::Failed { message, .. }) = (&preflight, &mut ended) {
                *message = format!("{text}; {message}");
            }
            return ended;
        }
        match preflight {
            Some(message) => Ended::failed(
                EvaluationJobFailure::Preflight,
                ErrorCode::InvalidRequest,
                message,
            ),
            None => Ended::Completed,
        }
    }

    fn set_output(&self, id: &OperationId, name: &str) {
        let view = lock(&self.state).table.update(id, |view| {
            view.output = Some(name.to_owned());
        });
        if let Some(view) = view {
            self.publish(EvaluationEvent::EvaluationJobProgress {
                view: Box::new(view),
            });
        }
    }

    fn record_run(&self, id: &OperationId, report: &RunReport) {
        let outcome = &report.outcome;
        let scrub = self.scrubber(None);
        let line = EvaluationRunLine {
            label: truncate(&report.label, MAX_NAME_BYTES),
            index: report.index,
            status: match outcome.status {
                eval::driver::Status::Completed => EvaluationRunStatus::Completed,
                eval::driver::Status::Incomplete => EvaluationRunStatus::Incomplete,
                eval::driver::Status::Failed => EvaluationRunStatus::Failed,
                eval::driver::Status::PrepareFailed => EvaluationRunStatus::PrepareFailed,
            },
            reason: truncate(&scrub.text(&outcome.reason), MAX_TITLE_BYTES),
            user_turns: outcome.user_turns,
            tokens: outcome.tokens,
            tool_calls: outcome.tool_calls,
            elapsed_ms: outcome.elapsed_ms,
            denied_requests: u32::try_from(outcome.denied_requests.len()).unwrap_or(u32::MAX),
            pushed_refs: u32::try_from(outcome.pushed_refs.len()).unwrap_or(u32::MAX),
            verification_passed: outcome
                .verification
                .as_ref()
                .map(|verification| verification.passed),
        };
        let view = lock(&self.state).table.update(id, |view| {
            if view.runs.len() < MAX_JOB_RUNS {
                view.runs.push(line);
            } else {
                view.runs_omitted = view.runs_omitted.saturating_add(1);
            }
        });
        if let Some(view) = view {
            self.publish(EvaluationEvent::EvaluationJobProgress {
                view: Box::new(view),
            });
        }
    }

    /// Workspaces the runner could not remove: the view says so, the job's own
    /// result does not change.
    fn record_cleanup(&self, id: &OperationId, dir: &Path, cleanup: &eval::cleanup::Report) {
        let scrub = self.scrubber(None);
        let warnings: Vec<String> = cleanup
            .entries
            .iter()
            .filter_map(|entry| match &entry.action {
                eval::cleanup::Action::Skip(reason) | eval::cleanup::Action::Failed(reason) => {
                    let relative = entry.path.strip_prefix(dir).unwrap_or(&entry.path);
                    Some(truncate(
                        &scrub.text(&format!(
                            "workspace {} was not removed ({reason}); retry with evorch eval clean",
                            relative.display()
                        )),
                        MAX_JOB_MESSAGE_BYTES,
                    ))
                }
                _ => None,
            })
            .take(MAX_JOB_WARNINGS)
            .collect();
        if warnings.is_empty() {
            return;
        }
        lock(&self.state).table.update(id, |view| {
            view.warnings = warnings;
        });
    }

    // ---- report -------------------------------------------------------------

    async fn report_job(&self, id: &OperationId, targets: Vec<String>, rejudge: bool) -> Ended {
        let scrub = self.scrubber(None);
        match self.make_reports(id, &targets, rejudge).await {
            Ok(()) => Ended::Completed,
            Err(error) => failed_report(&error, &scrub, true),
        }
    }

    /// Report the directories `names` under the evaluation root and record
    /// them in the view.
    async fn make_reports(
        &self,
        id: &OperationId,
        names: &[String],
        rejudge: bool,
    ) -> Result<(), EvalError> {
        let root = self.parts.root.clone();
        let wanted = names.to_vec();
        let dirs = tokio::task::spawn_blocking(move || resolve_targets(&root, &wanted))
            .await
            .map_err(|error| EvalError::Io(std::io::Error::other(error.to_string())))??;
        let deps = ReportDeps {
            env: Arc::new(routing::ProcessEnv),
            judge: self.parts.seams.judge.clone(),
            credentials: Some(self.parts.credentials.clone()),
            catalog: self.catalog().await,
        };
        let reports = eval::report::report_pinned(
            dirs,
            Some(self.parts.config_dir.clone()),
            rejudge,
            &deps,
            |index| self.record_report(id, names.get(index)),
        )
        .await?;
        let table = (reports.len() > 1).then(|| {
            crate::areas::diagnostics::read_ops::bounded(&eval::cross_thread_table(&reports))
        });
        let view = lock(&self.state).table.update(id, |view| {
            view.cross_thread = table;
        });
        if let Some(view) = view {
            self.publish(EvaluationEvent::EvaluationJobProgress {
                view: Box::new(view),
            });
        }
        Ok(())
    }

    /// A report that is complete on disk, recorded the moment it is, so that a
    /// job stopped partway says which ones it wrote.
    fn record_report(&self, id: &OperationId, name: Option<&String>) {
        let Some(name) = name else { return };
        let view = lock(&self.state).table.update(id, |view| {
            view.reports.push(name.clone());
        });
        if let Some(view) = view {
            self.publish(EvaluationEvent::EvaluationJobProgress {
                view: Box::new(view),
            });
        }
    }

    /// The price catalog, read for each job: one that failed to load, or went
    /// stale, must not decide every later job of a long-lived core.
    async fn catalog(&self) -> Option<Arc<catalog::ModelCatalog>> {
        if let Some(source) = &self.parts.seams.catalog_source {
            return source();
        }
        if !self.parts.seams.load_catalog {
            return None;
        }
        catalog::ModelCatalog::load_or_refresh(&self.parts.config_dir.join("catalog"))
            .await
            .ok()
            .map(Arc::new)
    }

    // ---- views ----------------------------------------------------------------

    fn publish(&self, event: EvaluationEvent) {
        let _ = self.parts.source.publish(UiEvent::Evaluation(event));
    }

    /// Replaces every path the core knows by a label, then masks secrets.
    fn scrubber(&self, candidate: Option<&Path>) -> Scrub {
        let mut paths: Vec<(PathBuf, &'static str)> = Vec::new();
        if let Some(path) = candidate {
            paths.push((path.to_path_buf(), "<candidate>"));
        }
        paths.push((self.parts.root.clone(), "<evaluations>"));
        paths.push((self.parts.database.clone(), "<database>"));
        paths.push((self.parts.config_dir.clone(), "<config>"));
        Scrub(paths)
    }

    fn settle(&self, id: &OperationId, ended: &Ended) {
        let view = {
            let mut state = lock(&self.state);
            let view = state.table.update(id, |view| {
                let partial = if self.wrote_something(view) {
                    EvaluationJobEffect::Partial
                } else {
                    EvaluationJobEffect::None
                };
                match ended {
                    Ended::Completed => {
                        view.operation.state = OperationState::Ready;
                        view.effect = EvaluationJobEffect::Complete;
                    }
                    Ended::Failed {
                        failure,
                        code,
                        message,
                        unknown,
                    } => {
                        view.operation.state = OperationState::Failed { code: *code };
                        view.failure = Some(*failure);
                        view.effect = if *unknown {
                            EvaluationJobEffect::Unknown
                        } else {
                            partial
                        };
                        view.message = Some(truncate(message, MAX_JOB_MESSAGE_BYTES));
                    }
                    Ended::Cancelled(reason) => {
                        view.operation.state = OperationState::Cancelled { reason: *reason };
                        view.effect = partial;
                        view.message = Some(
                            match reason {
                                CancelReason::Requested => {
                                    "cancelled; the files already written stay, and the workspaces are removed with evorch eval clean"
                                }
                                _ => {
                                    "the core stopped; the files already written stay, and the workspaces are removed with evorch eval clean"
                                }
                            }
                            .to_owned(),
                        );
                    }
                }
            });
            state.running.remove(id);
            view
        };
        if let Some(view) = view {
            self.publish(EvaluationEvent::EvaluationJobSettled {
                view: Box::new(view),
            });
        }
    }

    /// Whether a job that did not finish left something behind: a run wrote
    /// into its output directory, or a report job wrote a report.
    fn wrote_something(&self, view: &EvaluationJobView) -> bool {
        match view.kind {
            EvaluationJobKind::Run => view.output.as_ref().is_some_and(|name| {
                std::fs::read_dir(self.parts.root.join(name))
                    .is_ok_and(|mut entries| entries.next().is_some())
            }),
            _ => !view.reports.is_empty(),
        }
    }
}

/// What identifies a job that is asked for: its id, its kind, the digest of its
/// request and the directories it uses.
struct JobRequest<'a> {
    id: &'a OperationId,
    kind: EvaluationJobKind,
    digest: [u8; 32],
    names: Vec<String>,
}

fn encode<T: serde::Serialize>(request: &T) -> Result<Vec<u8>, WorkbenchError> {
    serde_json::to_vec(request)
        .map_err(|_| error(ErrorCode::InvalidRequest, "the request cannot be encoded"))
}

struct Scrub(Vec<(PathBuf, &'static str)>);

impl Scrub {
    fn text(&self, text: &str) -> String {
        let mut text = text.to_owned();
        let mut paths: Vec<_> = self.0.iter().collect();
        // The longest first, so that a path inside another is not half replaced.
        paths.sort_by_key(|(path, _)| std::cmp::Reverse(path.as_os_str().len()));
        for (path, label) in paths {
            let path = path.to_string_lossy();
            if !path.is_empty() {
                text = text.replace(path.as_ref(), label);
            }
        }
        masked(&text)
    }
}

/// The failure of the runner, as a view carries it.
fn failed_run(error: &EvalError, scrub: &Scrub) -> Ended {
    let (failure, code) = match error {
        EvalError::Usage(_) | EvalError::Config(_) => {
            (EvaluationJobFailure::Usage, ErrorCode::InvalidRequest)
        }
        EvalError::Database { .. } | EvalError::Source(eval::source::SourceError::Storage(_)) => {
            (EvaluationJobFailure::Source, ErrorCode::StorageUnavailable)
        }
        EvalError::Source(_) => (EvaluationJobFailure::Source, ErrorCode::InvalidRequest),
        EvalError::Overlay(_) => (EvaluationJobFailure::Overlay, ErrorCode::InvalidRequest),
        EvalError::Io(_) | EvalError::Credential(_) | EvalError::CredentialTask(_) => {
            (EvaluationJobFailure::Io, ErrorCode::StorageUnavailable)
        }
        EvalError::Preflight { .. } => (EvaluationJobFailure::Preflight, ErrorCode::InvalidRequest),
    };
    Ended::failed(failure, code, scrub.text(&error.to_string()))
}

/// The failure of a report. `recorded` says that every report written so far
/// is named in the view, so a read-side failure (which happens before the
/// failing directory is written) wrote nothing more and the effect is exact;
/// any other failure may have written part of what it was asked to, so its
/// effect is unknown.
fn failed_report(error: &EvalError, scrub: &Scrub, recorded: bool) -> Ended {
    let read_side = matches!(
        error,
        EvalError::Usage(_) | EvalError::Config(_) | EvalError::Database { .. }
    );
    match failed_run(error, scrub) {
        Ended::Failed {
            failure,
            code,
            message,
            ..
        } => {
            let failure = match failure {
                EvaluationJobFailure::Usage => EvaluationJobFailure::Usage,
                _ => EvaluationJobFailure::Report,
            };
            Ended::Failed {
                failure,
                code,
                message,
                unknown: !(recorded && read_side),
            }
        }
        other => other,
    }
}

/// The directories of a report, by name under the root: each must be a plain
/// directory there (a link is not followed).
fn resolve_targets(root: &Path, names: &[String]) -> Result<Vec<PinnedDir>, EvalError> {
    let root = PinnedDir::open_root(root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            EvalError::Usage("there is no evaluation directory yet".into())
        } else {
            EvalError::Io(error)
        }
    })?;
    // Each directory is opened from the pinned root without following a link,
    // and stays open: the report is read and written through the descriptors,
    // so nothing that is swapped in afterwards is followed.
    names
        .iter()
        .map(|name| {
            root.open_dir(name).map_err(|error| {
                let not_a_directory = matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
                ) || [rustix::io::Errno::LOOP, rustix::io::Errno::NOTDIR]
                    .iter()
                    .any(|errno| error.raw_os_error() == Some(errno.raw_os_error()));
                if not_a_directory {
                    EvalError::Usage(format!("{name} is not an evaluation directory"))
                } else {
                    EvalError::Io(error)
                }
            })
        })
        .collect()
}

/// A private file holding an overlay text, removed when it is dropped.
struct OverlayFile(PathBuf);

impl Drop for OverlayFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

struct Prepared {
    name: String,
    dir: PathBuf,
    candidate: Option<OverlayFile>,
}

/// Resolve the output directory under the root and write the overlay to a
/// private file.
fn prepare_run(
    root: &Path,
    thread: &str,
    output: Option<String>,
    candidate: Option<String>,
) -> Result<Prepared, Ended> {
    std::fs::create_dir_all(root)
        .map_err(|_| Ended::io("the evaluation root cannot be created"))?;
    let root = root
        .canonicalize()
        .map_err(|_| Ended::io("the evaluation root cannot be resolved"))?;
    let name = match output {
        Some(name) => name,
        None => {
            let stem = eval::output::default_dir(Path::new(""), thread, SystemTime::now());
            let stem = stem
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            // A name that is free: the default has a one second resolution.
            std::iter::once(stem.clone())
                .chain((2..).map(|n| format!("{stem}-{n}")))
                .find(|name| std::fs::symlink_metadata(root.join(name)).is_err())
                .unwrap_or(stem)
        }
    };
    let dir = root.join(&name);
    if std::fs::symlink_metadata(&dir).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(Ended::usage(format!("{name} is a link, not a directory")));
    }
    let candidate = candidate.map(|text| write_overlay(&text)).transpose()?;
    Ok(Prepared {
        name,
        dir,
        candidate,
    })
}

fn write_overlay(text: &str) -> Result<OverlayFile, Ended> {
    use std::io::Write;
    let stored = || Ended::io("the candidate overlay cannot be stored");
    let path = std::env::temp_dir().join(format!(
        "evorch-core-candidate-{}.toml",
        uuid::Uuid::new_v4().simple()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(|_| stored())?;
    let overlay = OverlayFile(path);
    file.write_all(text.as_bytes()).map_err(|_| stored())?;
    Ok(overlay)
}

/// Aborts a task when it is dropped, so that a job dropped without an outcome
/// never leaves its work running.
/// Resolves when a stop is asked; never when the asker is gone.
async fn cancelled(cancel: &watch::Receiver<bool>) {
    let mut cancel = cancel.clone();
    if cancel.wait_for(|requested| *requested).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// How long a stopped job's work may take to stop its runs before it is
/// dropped: the replay's own bound for its runs to stop, and then time to
/// finish what follows.
const STOP_GRACE: Duration = Duration::from_secs(eval::driver::SETTLE_TIMEOUT.as_secs() + 30);

// The work is dropped only after the replay gave up waiting for its runs.
const _: () = assert!(STOP_GRACE.as_secs() > eval::driver::SETTLE_TIMEOUT.as_secs());

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Settles a job as cancelled when its task is dropped without an outcome.
struct Settle {
    jobs: Arc<Jobs>,
    id: OperationId,
    settled: bool,
}

impl Settle {
    fn finish(&mut self, ended: Ended) {
        self.settled = true;
        self.jobs.settle(&self.id, &ended);
    }
}

impl Drop for Settle {
    fn drop(&mut self) {
        if !self.settled {
            self.jobs
                .settle(&self.id, &Ended::Cancelled(CancelReason::Shutdown));
        }
    }
}
