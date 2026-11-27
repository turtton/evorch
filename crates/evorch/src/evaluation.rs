//! `evorch eval run/report/clean`: replay a recorded thread under
//! the user's configuration and a candidate overlay, then judge, reprice and
//! report the replays (ADR 0036).
//!
//! `run` and `report` (which also rejudges) are jobs of the core (ADR 0045
//! §10): this module turns the command line into the core's request, follows
//! the job until it settles and prints what it left. The core owns the
//! configuration database, the credentials and the evaluation directory; the
//! CLI never opens them, and without a core it does nothing. `clean` removes
//! workspaces on the local disk and stays here.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use workbench_client::WorkbenchClient;
use workbench_protocol::{
    commands::{Command as CoreCommand, CommandResult},
    events::{SubscriptionScope, UiEvent},
    handshake::Capability,
    ids::{OperationId, ThreadId},
    namespaces::{
        diagnostics::OperationTarget,
        evaluation::{
            EvaluationCommand, EvaluationEvent, EvaluationJobEffect, EvaluationJobView,
            EvaluationQuery, EvaluationReply, EvaluationResult, EvaluationRunLine,
            StartEvaluationReport, StartEvaluationRun, validate_target_name,
        },
    },
    operation::OperationState,
    queries::{Query, QueryReply},
};

use crate::cli::core_client::{self, CliError, Rendered, Step};

pub const USAGE: &str = "usage: evorch eval run --thread <thread-id> [--candidate <overlay.toml>] [--common <overlay.toml>] [--approved-digest <hex>] [--repeat <n>] [--output <name>] [--user-config <dir>] [--problem <text>] [--verify <command>] [--verify-timeout <secs>] [--report|--no-report] [--keep-workspace]
       evorch eval clean [--output <dir> ...] [--user-config <dir>] [--dry-run] [--include-unfinished]
       evorch eval report --output <name> [--output <name> ...] [--user-config <dir>] [--rejudge|--checks-only]
`eval run` and `eval report` run in the evorch core, which is started when none is running; they
do not work without it. `eval run` replays the thread <n> times (default eval.repeat_target) under
the user's configuration, then <n> times with the candidate overlay merged over it. The overlay is a
config file fragment (the CLI reads the file and sends its text); when a role_profile is active,
change role_profiles.<name> rather than agents or routing. It cannot change eval, tools or
comment_checker. --common is an overlay of only agents, routing, role_profile and role_profiles that both the
baseline and the candidate run under (the baseline is the user's configuration with it; the candidate
is that with the candidate overlay merged over it): use it to fix the model under evaluation on both
sides. A candidate must change something the baseline does not have; it cannot change a role's model
together with a tool description, and one that changes a tool description is not accepted yet.
--approved-digest is set by eval_start: the run is refused if the sample, overlays, configuration or
prompt presets no longer hash to the approved digest. --problem describes what went wrong in the thread and is judged as its own aspect.
--verify runs a command in each replay's final workspace, without network, failing after
--verify-timeout (default 600) s. The core reads its own database; there is no --db.
The output is a directory of <user-config-dir>/eval, named by --output (a bare name, or an
absolute path that is a direct child of that directory: no `..`, no links); it must be missing or empty. The default is
<UTC YYYYmmdd-HHMMSS>-<thread-id>. Workspaces are removed after finish/verification, including on
failure; --keep-workspace preserves them. Interrupting or killing this command, or losing its
connection, stops the job it started (and no other job): the core cancels it when the command's
last connection to it ends, and the replay removes its workspaces as it ends. They remain only
with --keep-workspace, when one could not be removed (the job's warnings say which), when the job
did not stop in time and was dropped, or when the core itself was killed; `eval clean` removes them.
Unless --no-report is given, the directory is then reported as by `eval report`.
`eval report` reprices every run from one price table, judges each baseline/candidate pair
in both orders (cached in <dir>/judgments; --rejudge asks again), and writes
<dir>/report.json and <dir>/report.md. Several directories are also tabulated side by side.
`eval report --checks-only` grades the runs against the thread's sample checks from their recorded
evidence alone: no judge, simulator, verification command or provider is used, and report.json
is not touched. It writes <dir>/checks-<sample hash>.json and .md. An evaluation that pinned no
sample is graded against the sample as it is now and is marked as a post-hoc analysis. It cannot
be combined with --rejudge.
`eval clean` removes only preflight-workspace and runs/<child>/workspace, never evidence.
Without --output it scans immediate evaluations in <user-config-dir>/eval. --dry-run
changes nothing. Busy locks are always skipped; unlocked interrupted evaluations are
eligible. For unfinished legacy evaluations without a lock, --include-unfinished requires
the user to confirm that the old evaluation is NOT running. Sizes are logical estimates,
not actual disk space freed.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run(RunRequest),
    Report(ReportRequest),
    Clean(CleanArgs),
}

/// `evorch eval run`, as typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunRequest {
    pub thread: String,
    pub candidate: Option<PathBuf>,
    pub common: Option<PathBuf>,
    pub approved_digest: Option<String>,
    pub repeat: Option<u32>,
    pub output: Option<PathBuf>,
    pub user_config_dir: Option<PathBuf>,
    pub problem: Option<String>,
    pub verify: Option<String>,
    pub verify_timeout_secs: Option<u32>,
    pub keep_workspace: bool,
    pub report: bool,
}

/// `evorch eval report`, as typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportRequest {
    pub outputs: Vec<PathBuf>,
    pub user_config_dir: Option<PathBuf>,
    pub rejudge: bool,
    pub checks_only: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanArgs {
    pub outputs: Vec<PathBuf>,
    pub user_config_dir: Option<PathBuf>,
    pub dry_run: bool,
    pub include_unfinished: bool,
}

fn parse_clean(mut argv: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut args = CleanArgs::default();
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--dry-run" => args.dry_run = true,
            "--include-unfinished" => args.include_unfinished = true,
            "--output" => args
                .outputs
                .push(argv.next().ok_or_else(|| USAGE.to_owned())?.into()),
            "--user-config" => {
                args.user_config_dir = Some(argv.next().ok_or_else(|| USAGE.to_owned())?.into())
            }
            _ => return Err(USAGE.into()),
        }
    }
    Ok(Command::Clean(args))
}

fn clean(args: CleanArgs) -> Result<String, String> {
    let home = if args.outputs.is_empty() {
        Some(
            args.user_config_dir
                .or_else(config::user_config_dir)
                .ok_or("cannot resolve user config directory; pass --user-config <dir>")?
                .join("eval"),
        )
    } else {
        None
    };
    let report = eval::cleanup::clean(
        &args.outputs,
        home.as_deref(),
        eval::cleanup::Options {
            dry_run: args.dry_run,
            include_unfinished: args.include_unfinished,
        },
    );
    format_cleanup(&report)
}

fn format_cleanup(report: &eval::cleanup::Report) -> Result<String, String> {
    use eval::cleanup::{Action, Size};
    let mut lines = Vec::new();
    for path in &report.unfinished_legacy {
        lines.push(format!("warning: {}: unfinished legacy evaluation; user must confirm the old evaluation is NOT running", path.display()));
    }
    let mut total = Some(0_u64);
    let mut unknown = 0;
    for entry in &report.entries {
        let action = match &entry.action {
            Action::WouldRemove => "would-remove".to_owned(),
            Action::Removed => "removed".to_owned(),
            Action::Skip(reason) => format!("skip ({reason})"),
            Action::Failed(reason) => format!("failed ({reason})"),
        };
        let size = match &entry.size {
            Size::Bytes(bytes) => {
                total = total.and_then(|total| total.checked_add(*bytes));
                format!("{bytes} bytes")
            }
            Size::Unknown(reason) => {
                unknown += 1;
                format!("unknown ({reason})")
            }
            Size::NotMeasured => {
                unknown += 1;
                "unknown (not measured)".into()
            }
        };
        lines.push(format!("{action}: {}: {size}", entry.path.display()));
    }
    lines.push(format!("total: {} bytes known, {unknown} unknown; logical size estimate, not actual disk space freed",
        total.map_or_else(|| "unknown (overflow)".into(), |bytes| bytes.to_string())));
    let text = lines.join("\n");
    if report.has_failures() {
        Err(text)
    } else {
        Ok(text)
    }
}

/// Parses everything after `evorch`, starting with `eval run`, `report` or `clean`.
pub fn parse_args(argv: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut argv = argv;
    if argv.next().as_deref() != Some("eval") {
        return Err(USAGE.into());
    }
    match argv.next().as_deref() {
        Some("run") => parse_run(argv),
        Some("report") => parse_report(argv),
        Some("clean") => parse_clean(argv),
        _ => Err(USAGE.into()),
    }
}

fn positive(flag: &str, value: &str) -> Result<u32, String> {
    value
        .parse()
        .ok()
        .filter(|number| *number > 0)
        .ok_or_else(|| format!("{flag} must be a positive integer\n{USAGE}"))
}

fn parse_run(mut argv: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut request = RunRequest {
        report: true,
        ..RunRequest::default()
    };
    let mut thread = None;
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--report" | "--no-report" => {
                request.report = flag == "--report";
                continue;
            }
            "--keep-workspace" => {
                request.keep_workspace = true;
                continue;
            }
            _ => {}
        }
        let value = argv.next().ok_or_else(|| USAGE.to_owned())?;
        match flag.as_str() {
            "--thread" => thread = Some(value),
            "--candidate" => request.candidate = Some(PathBuf::from(value)),
            "--common" => request.common = Some(PathBuf::from(value)),
            "--approved-digest" => request.approved_digest = Some(value),
            "--repeat" => request.repeat = Some(positive("--repeat", &value)?),
            "--output" => request.output = Some(PathBuf::from(value)),
            "--db" => {
                return Err(format!(
                    "--db is not accepted: the evorch core reads its own database from the user config \
                     directory (--user-config); drop --db\n{USAGE}"
                ));
            }
            "--user-config" => request.user_config_dir = Some(PathBuf::from(value)),
            "--problem" => request.problem = Some(value).filter(|text| !text.trim().is_empty()),
            "--verify" => request.verify = Some(value).filter(|text| !text.trim().is_empty()),
            "--verify-timeout" => {
                request.verify_timeout_secs =
                    Some(value.parse().ok().filter(|secs| *secs > 0).ok_or_else(|| {
                        format!("--verify-timeout must be a positive number of seconds\n{USAGE}")
                    })?);
            }
            _ => return Err(USAGE.into()),
        }
    }
    request.thread = thread
        .filter(|thread| !thread.is_empty())
        .ok_or_else(|| USAGE.to_owned())?;
    Ok(Command::Run(request))
}

fn parse_report(mut argv: impl Iterator<Item = String>) -> Result<Command, String> {
    let mut request = ReportRequest::default();
    while let Some(flag) = argv.next() {
        if flag == "--rejudge" {
            request.rejudge = true;
            continue;
        }
        if flag == "--checks-only" {
            request.checks_only = true;
            continue;
        }
        let value = argv.next().ok_or_else(|| USAGE.to_owned())?;
        match flag.as_str() {
            "--output" => request.outputs.push(PathBuf::from(value)),
            "--user-config" => request.user_config_dir = Some(PathBuf::from(value)),
            _ => return Err(USAGE.into()),
        }
    }
    if request.outputs.is_empty() || (request.rejudge && request.checks_only) {
        return Err(USAGE.into());
    }
    Ok(Command::Report(request))
}

// ---- where the evaluations are --------------------------------------------

/// The evaluation root of a user configuration directory: where the core
/// keeps the evaluations it runs.
fn eval_root(user_config_dir: Option<&Path>) -> Result<PathBuf, String> {
    user_config_dir
        .map(Path::to_path_buf)
        .or_else(config::user_config_dir)
        .map(|dir| dir.join("eval"))
        .ok_or_else(|| "cannot resolve user config directory; pass --user-config <dir>".to_owned())
}

/// The name of the directory `value` stands for under `root`. The core takes
/// names only, so a path is accepted when it is a directory directly inside
/// the evaluation root (after resolving links on the way to it); a link at the
/// end is refused, never followed. A relative path is only a name, and no path
/// may contain `..`.
fn target_name(root: &Path, value: &Path) -> Result<String, String> {
    let shown = value.display();
    if value
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(format!("{shown} must not contain `..`"));
    }
    let mut parts = value.components();
    let single = match (parts.next(), parts.next()) {
        (Some(std::path::Component::Normal(name)), None) => Some(name),
        _ => None,
    };
    let name = if let Some(name) = single {
        name.to_owned()
    } else if !value.is_absolute() {
        return Err(format!(
            "{shown} is not a name; pass a name or an absolute path directly inside the evaluation directory"
        ));
    } else {
        let name = value
            .file_name()
            .ok_or_else(|| format!("{shown} does not name a directory"))?
            .to_owned();
        let parent = match value.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        let parent = parent
            .canonicalize()
            .map_err(|error| format!("cannot resolve {}: {error}", parent.display()))?;
        let root = root.canonicalize().map_err(|error| {
            format!(
                "cannot resolve the evaluation directory {}: {error}",
                root.display()
            )
        })?;
        if parent != root {
            return Err(format!(
                "{shown} is not directly inside the evaluation directory {}; pass its name",
                root.display()
            ));
        }
        name
    };
    if std::fs::symlink_metadata(root.join(&name)).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(format!("{shown} is a link, not a directory"));
    }
    let name = name
        .into_string()
        .map_err(|_| format!("{shown} is not a name the core accepts"))?;
    validate_target_name("output", &name).map_err(|error| format!("{shown}: {error}"))?;
    Ok(name)
}

// ---- following the job ------------------------------------------------------

/// One line per replay, as it ends.
fn summary(line: &EvaluationRunLine) -> String {
    let status = serde_json::to_value(line.status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default();
    let verification = match line.verification_passed {
        None => "",
        Some(true) => ", verification passed",
        Some(false) => ", verification failed",
    };
    format!(
        "{}-{}: {status} ({}); {} user turns, {} tokens, {} tool calls, {:.1} s, {} denied, {} pushed refs{verification}",
        line.label,
        line.index,
        line.reason,
        line.user_turns,
        line.tokens,
        line.tool_calls,
        Duration::from_millis(line.elapsed_ms).as_secs_f64(),
        line.denied_requests,
        line.pushed_refs,
    )
}

fn is_job_event(event: &UiEvent) -> bool {
    matches!(
        event,
        UiEvent::Evaluation(
            EvaluationEvent::EvaluationJobProgress { .. }
                | EvaluationEvent::EvaluationJobSettled { .. }
        )
    )
}

/// Start the job `command` names and follow it until it settles, handing every
/// replay line to `progress` once.
async fn follow(
    client: &WorkbenchClient,
    command: EvaluationCommand,
    id: &OperationId,
    progress: &dyn Fn(&str),
) -> Result<EvaluationJobView, CliError> {
    let reply = core_client::send(client, CoreCommand::Evaluation(command)).await?;
    match reply.result.as_deref() {
        Some(CommandResult::Evaluation(EvaluationResult::Job(_))) => {}
        _ => {
            return Err(CliError::Failed(
                "protocol violation: the core did not answer with a job".into(),
            ));
        }
    }
    let printed = Cell::new(0_usize);
    core_client::wait_settled(client, SubscriptionScope::All, is_job_event, || {
        let (printed, id) = (&printed, id.clone());
        async move {
            let query = Query::Evaluation(EvaluationQuery::GetEvaluationJob(OperationTarget {
                operation_id: id,
            }));
            // The core no longer holds the job: it restarted, so whether the
            // job finished is not known.
            let Some(view) = core_client::read_operation(client, query, |reply| match reply {
                QueryReply::Evaluation(EvaluationReply::Job(view)) => Some(*view),
                _ => None,
            })
            .await?
            else {
                return Ok(Step::Unknown);
            };
            for line in view.runs.iter().skip(printed.get()) {
                progress(&summary(line));
            }
            printed.set(view.runs.len());
            Ok(if view.operation.state.is_final() {
                Step::Settled(view)
            } else {
                Step::Pending
            })
        }
    })
    .await
}

// ---- what the command prints ------------------------------------------------

fn warnings(view: &EvaluationJobView, lines: &mut Vec<String>) {
    if view.runs_omitted > 0 {
        lines.push(format!(
            "{} more replays ended than the core lists",
            view.runs_omitted
        ));
    }
    lines.extend(view.warnings.iter().map(|text| format!("warning: {text}")));
}

fn report_lines(root: &Path, view: &EvaluationJobView, lines: &mut Vec<String>) {
    lines.extend(view.reports.iter().map(|name| {
        format!(
            "Report in {}",
            root.join(name)
                .join(eval::output::REPORT_MARKDOWN_FILE)
                .display()
        )
    }));
    if let Some(table) = &view.cross_thread {
        lines.push(table.text.clone());
    }
}

/// What a job that did not finish left on disk, as a sentence for `dir`.
fn effect_line(view: &EvaluationJobView, what: &str, dir: &str) -> Option<String> {
    match view.effect {
        EvaluationJobEffect::Partial => Some(format!("{what} written so far stay in {dir}")),
        EvaluationJobEffect::Unknown => Some(format!(
            "what the job wrote is unknown; look at {dir} before running it again"
        )),
        EvaluationJobEffect::None | EvaluationJobEffect::Complete => None,
    }
}

fn stopped(view: &EvaluationJobView) -> String {
    view.message
        .clone()
        .unwrap_or_else(|| match view.operation.state {
            OperationState::Cancelled { .. } => "the evaluation was cancelled".to_owned(),
            _ => "the evaluation failed".to_owned(),
        })
}

fn render_run(view: &EvaluationJobView, root: &Path, report: bool) -> Rendered {
    let dir = view
        .output
        .as_ref()
        .map(|name| root.join(name).display().to_string());
    let mut lines = Vec::new();
    if view.operation.state == OperationState::Ready {
        lines.extend(dir.iter().map(|dir| format!("Results in {dir}")));
        if report {
            report_lines(root, view, &mut lines);
        }
        warnings(view, &mut lines);
        return Ok(lines.join("\n"));
    }
    lines.push(stopped(view));
    if let Some(dir) = &dir {
        lines.extend(effect_line(view, "the files", dir));
    }
    report_lines(root, view, &mut lines);
    warnings(view, &mut lines);
    Err(lines.join("\n"))
}

/// The result of a checks-only job: where each result file is and the grading of every run.
fn checks_lines(root: &Path, view: &EvaluationJobView, lines: &mut Vec<String>) {
    lines.extend(
        view.reports
            .iter()
            .map(|name| format!("Checks graded in {}", root.join(name).display())),
    );
    if let Some(summary) = &view.cross_thread {
        lines.push(summary.text.clone());
    }
}

fn render_report(view: &EvaluationJobView, root: &Path, checks_only: bool) -> Rendered {
    let mut lines = Vec::new();
    let report_lines = if checks_only {
        checks_lines
    } else {
        report_lines
    };
    if view.operation.state == OperationState::Ready {
        report_lines(root, view, &mut lines);
        return Ok(lines.join("\n"));
    }
    lines.push(stopped(view));
    lines.extend(effect_line(
        view,
        "the reports",
        &root.display().to_string(),
    ));
    report_lines(root, view, &mut lines);
    Err(lines.join("\n"))
}

// ---- the commands ------------------------------------------------------------

async fn run_job(request: RunRequest, root: &Path) -> Result<Rendered, CliError> {
    let failed = |text: String| CliError::Failed(text);
    let output = request
        .output
        .as_deref()
        .map(|value| target_name(root, value))
        .transpose()
        .map_err(failed)?;
    let candidate = request
        .candidate
        .as_deref()
        .map(|path| {
            std::fs::read_to_string(path)
                .map_err(|error| format!("cannot read candidate {}: {error}", path.display()))
        })
        .transpose()
        .map_err(failed)?;
    let common = request
        .common
        .as_deref()
        .map(|path| {
            std::fs::read_to_string(path)
                .map_err(|error| format!("cannot read common overlay {}: {error}", path.display()))
        })
        .transpose()
        .map_err(failed)?;
    let operation_id = core_client::operation_id("cli-eval")?;
    let start = StartEvaluationRun {
        operation_id: operation_id.clone(),
        thread: ThreadId::new(request.thread)
            .map_err(|error| CliError::Failed(format!("--thread: {error}")))?,
        candidate,
        common,
        approved_digest: request.approved_digest,
        repeat: request.repeat,
        output,
        problem: request.problem,
        verify: request.verify,
        verify_timeout_secs: request.verify_timeout_secs,
        keep_workspace: request.keep_workspace,
        report: request.report,
    };
    let report = request.report;
    let connected =
        core_client::connect(request.user_config_dir, &[Capability::SnapshotEvents]).await?;
    let view = follow(
        &connected.client,
        EvaluationCommand::StartEvaluationRun(Box::new(start)),
        &operation_id,
        &|line| println!("{line}"),
    )
    .await?;
    Ok(render_run(&view, root, report))
}

async fn report_job(request: ReportRequest, root: &Path) -> Result<Rendered, CliError> {
    let targets = request
        .outputs
        .iter()
        .map(|value| target_name(root, value))
        .collect::<Result<Vec<_>, _>>()
        .map_err(CliError::Failed)?;
    let operation_id = core_client::operation_id("cli-eval")?;
    let start = StartEvaluationReport {
        operation_id: operation_id.clone(),
        targets,
        rejudge: request.rejudge,
        checks_only: request.checks_only,
    };
    let checks_only = request.checks_only;
    let connected =
        core_client::connect(request.user_config_dir, &[Capability::SnapshotEvents]).await?;
    let view = follow(
        &connected.client,
        EvaluationCommand::StartEvaluationReport(start),
        &operation_id,
        &|line| println!("{line}"),
    )
    .await?;
    Ok(render_report(&view, root, checks_only))
}

/// Run the command; `clean` locally, the others in the core.
///
/// # Errors
/// The command could not be carried out (no core, a refusal, a lost reply);
/// a job that ran and failed is the `Err` of the text, not of this.
pub async fn execute(command: Command) -> Result<Rendered, CliError> {
    match command {
        Command::Clean(args) => Ok(clean(args)),
        Command::Run(request) => {
            let root = eval_root(request.user_config_dir.as_deref()).map_err(CliError::Failed)?;
            run_job(request, &root).await
        }
        Command::Report(request) => {
            let root = eval_root(request.user_config_dir.as_deref()).map_err(CliError::Failed)?;
            report_job(request, &root).await
        }
    }
}

/// Run the command and print its result; the exit code says how it went.
pub async fn run(command: Command) -> ExitCode {
    core_client::finish(execute(command).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_protocol::{
        errors::ErrorCode,
        management::EvaluationJobKind,
        namespaces::{diagnostics::BoundedText, evaluation::EvaluationRunStatus},
        operation::{CancelReason, OperationView},
        wire::Revision,
    };

    fn parse(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn every_run_flag_is_parsed() {
        // Given: a full `eval run` command line.
        let argv = [
            "eval",
            "run",
            "--thread",
            "thread-1",
            "--candidate",
            "c.toml",
            "--common",
            "common.toml",
            "--approved-digest",
            "abcdef0123456789",
            "--repeat",
            "3",
            "--output",
            "out",
            "--user-config",
            "config",
            "--problem",
            "the reviewer missed a bug",
            "--verify",
            "cargo test --offline",
            "--verify-timeout",
            "120",
            "--no-report",
            "--keep-workspace",
        ];

        // When: it is parsed.
        let command = parse(&argv).unwrap();

        // Then: every value lands in its field and the report step is skipped.
        assert_eq!(
            command,
            Command::Run(RunRequest {
                thread: "thread-1".into(),
                candidate: Some("c.toml".into()),
                common: Some("common.toml".into()),
                approved_digest: Some("abcdef0123456789".into()),
                repeat: Some(3),
                output: Some("out".into()),
                user_config_dir: Some("config".into()),
                problem: Some("the reviewer missed a bug".into()),
                verify: Some("cargo test --offline".into()),
                verify_timeout_secs: Some(120),
                keep_workspace: true,
                report: false,
            })
        );
    }

    #[test]
    fn a_run_needs_only_the_thread_and_reports_by_default() {
        let command = parse(&["eval", "run", "--thread", "t"]).unwrap();
        assert_eq!(
            command,
            Command::Run(RunRequest {
                thread: "t".into(),
                report: true,
                ..RunRequest::default()
            })
        );
    }

    #[test]
    fn report_takes_several_directories_and_rejudge() {
        let command = parse(&[
            "eval",
            "report",
            "--output",
            "a",
            "--rejudge",
            "--output",
            "b",
            "--user-config",
            "config",
        ])
        .unwrap();
        assert_eq!(
            command,
            Command::Report(ReportRequest {
                outputs: vec!["a".into(), "b".into()],
                user_config_dir: Some("config".into()),
                rejudge: true,
                checks_only: false,
            })
        );
    }

    #[test]
    fn checks_only_is_a_report_flag_that_cannot_rejudge() {
        let command = parse(&["eval", "report", "--checks-only", "--output", "a"]).unwrap();
        assert_eq!(
            command,
            Command::Report(ReportRequest {
                outputs: vec!["a".into()],
                checks_only: true,
                ..ReportRequest::default()
            })
        );
        let error = parse(&[
            "eval",
            "report",
            "--output",
            "a",
            "--checks-only",
            "--rejudge",
        ])
        .unwrap_err();
        assert!(error.contains("usage: evorch eval run"), "{error}");
    }

    #[test]
    fn the_core_owns_the_database_so_db_is_refused() {
        let error = parse(&["eval", "run", "--thread", "t", "--db", "events.db"]).unwrap_err();
        assert!(error.contains("--db is not accepted"), "{error}");
    }

    #[test]
    fn malformed_command_lines_are_refused_with_usage() {
        for argv in [
            &["eval"][..],
            &["eval", "inspect"],
            &["eval", "report"],
            &["eval", "report", "--rejudge"],
            &["eval", "report", "--output"],
            &["eval", "report", "--output", "a", "--thread", "t"],
            &["eval", "run"],
            &["eval", "run", "--thread"],
            &["eval", "run", "--thread", ""],
            &["eval", "run", "--thread", "t", "--unknown", "x"],
            &["eval", "run", "--thread", "t", "--repeat", "0"],
            &["eval", "run", "--thread", "t", "--repeat", "many"],
            &["eval", "run", "--thread", "t", "--verify-timeout", "0"],
            &["eval", "run", "--thread", "t", "--rejudge"],
        ] {
            let error = parse(argv).unwrap_err();
            assert!(
                error.contains("usage: evorch eval run"),
                "{argv:?}: {error}"
            );
        }
    }

    #[test]
    fn a_target_is_a_name_or_a_directory_directly_inside_the_evaluation_root() {
        // Given: an evaluation root with a directory, a link and a stranger.
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("eval");
        std::fs::create_dir_all(root.join("run-a")).unwrap();
        std::fs::create_dir_all(temp.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(temp.path().join("elsewhere"), root.join("link")).unwrap();

        // Then: a name and a path of the root's own directory both give the name.
        assert_eq!(target_name(&root, Path::new("run-a")).unwrap(), "run-a");
        assert_eq!(target_name(&root, &root.join("run-a")).unwrap(), "run-a");
        assert_eq!(
            target_name(&root, &root.join("not-yet")).unwrap(),
            "not-yet"
        );
        // A path that reaches the root through a link is the same directory.
        let through = temp.path().join("through");
        std::os::unix::fs::symlink(&root, &through).unwrap();
        assert_eq!(target_name(&root, &through.join("run-a")).unwrap(), "run-a");

        // And what is outside the root, a link at the end, a nested path or an
        // unnameable one is refused before anything is sent.
        for refused in [
            temp.path().join("elsewhere"),
            root.join("link"),
            root.join("run-a").join("deeper"),
            // `..` anywhere, even where it would lead back into the root.
            root.join("..").join("eval").join("run-a"),
            root.join("..").join("elsewhere"),
            PathBuf::from("../run-a"),
            // A relative path is a name or nothing.
            PathBuf::from("sub/run-a"),
            // A directory that is a link out of the root, and a path through it.
            root.join("link").join("inside"),
            temp.path().join("elsewhere").join("run-a"),
            root.join(".groups"),
            root.join("bad name"),
            PathBuf::from(".."),
            PathBuf::from("."),
            PathBuf::from("/"),
        ] {
            assert!(
                target_name(&root, &refused).is_err(),
                "{} must be refused",
                refused.display()
            );
        }
    }

    fn job(state: OperationState, kind: EvaluationJobKind) -> EvaluationJobView {
        let effect = if state == OperationState::Ready {
            EvaluationJobEffect::Complete
        } else {
            EvaluationJobEffect::None
        };
        EvaluationJobView {
            operation: OperationView {
                operation_id: OperationId::new("op").unwrap(),
                state,
                revision: Revision(1),
                result: None,
            },
            kind,
            runs: Vec::new(),
            runs_omitted: 0,
            output: None,
            reports: Vec::new(),
            cross_thread: None,
            effect,
            failure: None,
            message: None,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn a_finished_run_says_where_its_results_and_report_are() {
        let mut view = job(OperationState::Ready, EvaluationJobKind::Run);
        view.output = Some("out".into());
        view.reports = vec!["out".into()];
        view.warnings = vec!["workspace x was not removed".into()];
        let text = render_run(&view, Path::new("/cfg/eval"), true).unwrap();
        assert!(text.starts_with("Results in /cfg/eval/out\n"), "{text}");
        assert!(text.contains("Report in /cfg/eval/out/report.md"), "{text}");
        assert!(
            text.contains("warning: workspace x was not removed"),
            "{text}"
        );
        assert!(
            !render_run(&view, Path::new("/cfg/eval"), false)
                .unwrap()
                .contains("Report in")
        );
    }

    #[test]
    fn a_failed_run_is_an_error_that_names_what_it_left() {
        let failed = OperationState::Failed {
            code: ErrorCode::InvalidRequest,
        };
        let mut view = job(failed, EvaluationJobKind::Run);
        view.output = Some("out".into());
        view.message = Some("evaluation preflight failed: no thread".into());
        view.effect = EvaluationJobEffect::Unknown;
        view.reports = vec!["out".into()];
        let text = render_run(&view, Path::new("/cfg/eval"), true).unwrap_err();
        assert!(text.starts_with("evaluation preflight failed"), "{text}");
        assert!(text.contains("unknown; look at /cfg/eval/out"), "{text}");
        assert!(text.contains("Report in /cfg/eval/out/report.md"), "{text}");

        view.effect = EvaluationJobEffect::Partial;
        view.operation.state = OperationState::Cancelled {
            reason: CancelReason::Requested,
        };
        view.message = None;
        let text = render_run(&view, Path::new("/cfg/eval"), true).unwrap_err();
        assert!(text.starts_with("the evaluation was cancelled"), "{text}");
        assert!(
            text.contains("written so far stay in /cfg/eval/out"),
            "{text}"
        );
    }

    #[test]
    fn a_report_lists_each_directory_and_the_side_by_side_table() {
        let mut view = job(OperationState::Ready, EvaluationJobKind::Rejudge);
        view.reports = vec!["a".into(), "b".into()];
        view.cross_thread = Some(BoundedText {
            text: "| thread | a | b |".into(),
            total_bytes: 19,
            truncated: false,
        });
        let text = render_report(&view, Path::new("/cfg/eval"), false).unwrap();
        assert_eq!(
            text,
            "Report in /cfg/eval/a/report.md\nReport in /cfg/eval/b/report.md\n| thread | a | b |"
        );
    }

    #[test]
    fn a_checks_only_job_prints_the_grading_not_a_report_path() {
        let mut view = job(OperationState::Ready, EvaluationJobKind::GenerateReport);
        view.reports = vec!["a".into()];
        view.cross_thread = Some(BoundedText {
            text: "thread-1 (checks pinned at the start): checks-abc.md".into(),
            total_bytes: 52,
            truncated: false,
        });
        let text = render_report(&view, Path::new("/cfg/eval"), true).unwrap();
        assert_eq!(
            text,
            "Checks graded in /cfg/eval/a\nthread-1 (checks pinned at the start): checks-abc.md"
        );
    }

    #[test]
    fn a_replay_line_keeps_the_format_the_runner_always_printed() {
        let line = EvaluationRunLine {
            label: "baseline".into(),
            index: 1,
            status: EvaluationRunStatus::Completed,
            reason: "done".into(),
            user_turns: 2,
            tokens: 300,
            tool_calls: 4,
            elapsed_ms: 1500,
            denied_requests: 0,
            pushed_refs: 1,
            verification_passed: Some(false),
        };
        assert_eq!(
            summary(&line),
            "baseline-1: completed (done); 2 user turns, 300 tokens, 4 tool calls, 1.5 s, 0 denied, 1 pushed refs, verification failed"
        );
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    #[tokio::test]
    async fn clean_parses_all_flags_and_needs_no_core() {
        let temp = tempfile::tempdir().unwrap();
        let command = parse_args(
            [
                "eval",
                "clean",
                "--output",
                "a",
                "--output",
                "b",
                "--user-config",
                "user",
                "--dry-run",
                "--include-unfinished",
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(
            command,
            Command::Clean(CleanArgs {
                outputs: vec!["a".into(), "b".into()],
                user_config_dir: Some("user".into()),
                dry_run: true,
                include_unfinished: true
            })
        );
        // No core runs here: clean must never try to reach one.
        let text = execute(Command::Clean(CleanArgs {
            user_config_dir: Some(temp.path().into()),
            ..Default::default()
        }))
        .await
        .unwrap()
        .unwrap();
        assert!(text.contains("0 bytes known"));
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }
    #[test]
    fn clean_failure_and_unknown_sizes_are_not_reported_as_success_or_zero() {
        use eval::cleanup::{Action, Entry, Report, Size};
        let report = Report {
            entries: vec![Entry {
                path: "workspace".into(),
                action: Action::Failed("permission denied".into()),
                size: Size::Unknown("permission denied".into()),
            }],
            ..Default::default()
        };
        let text = format_cleanup(&report).unwrap_err();
        assert!(text.contains("failed (permission denied)"));
        assert!(text.contains("unknown (permission denied)"));
        assert!(text.contains("1 unknown"));
    }
}
