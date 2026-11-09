//! The CI failure log of a pull request, for a notice to an agent (ADR 0043 Phase 2, PR B).
//!
//! This module only *obtains and prepares* the log. It does not decide when to look, deliver
//! anything, store anything or show anything: [`PullRequestLogSource::fetch_failed_logs`] is
//! called after a notice has been reserved, and what it returns is a value
//! ([`CiLogBundle`]) that holds a bounded, redacted excerpt and a structured account of what
//! was left out. A failure to get a log never touches the pull request snapshot; it is a
//! [`CiLogSkip`] in the bundle.
//!
//! # Which run is asked for
//!
//! A check's `provider_id` is not a run ID: Phase 1 builds `job:<id>` or `run:<id>` from an
//! Actions URL, and `check_run:<name>` or `status:<name>` for everything else. So the ID is
//! never handed to `gh run view` as it is. A failing check is resolved in two steps, each one
//! fixed `gh` argument vector with numeric IDs only:
//!
//! ```text
//! gh api --hostname github.com --method GET repos/<owner>/<repo>/actions/jobs/<job-id> --jq <JOB_JQ>
//! gh api --hostname github.com --method GET repos/<owner>/<repo>/actions/runs/<run-id> --jq <RUN_JQ>
//! ```
//!
//! (the first only for a `job:` check; `<owner>/<repo>` is the pull request's base
//! repository). The run is accepted when all of these hold, and the log is "not available"
//! with a structured reason otherwise:
//!
//! - the run belongs to the base repository (`repository.full_name`); a job or run of another
//!   repository is a 404 on this path and a mismatch if it is ever answered;
//! - its `head_sha` is the head the notice is about (and the check's own `head_sha` is too).
//!   A run that is only known by a merge commit does not match and is not used;
//! - it is related to the pull request: the run itself lists this pull request (number and
//!   base repository) in `pull_requests`. A run that lists no pull request, or only other
//!   ones, is refused: the same head can be the head of pull requests to other bases, and
//!   nothing else in the response proves which one a run belongs to. (GitHub lists none for
//!   some runs, such as a fork's `pull_request` run; those have no log here.);
//! - it has completed (a run in progress has no log yet).
//!
//! Only then is the log read, with a fixed vector:
//!
//! ```text
//! gh run view <run-id> --repo github.com/<owner>/<repo> --log-failed
//! ```
//!
//! Checks that are not Actions (a status context, a check run of an external CI) have no log
//! here. A job ID is never used as a run ID or the other way round: the two are different
//! namespaces and each has its own endpoint.
//!
//! # Limits
//!
//! At most [`CI_LOG_RUNS_MAX`] runs, [`CI_LOG_METADATA_CALLS_MAX`] metadata calls (each
//! answers one small object, so there is no paging), [`CI_LOG_RAW_BYTES_MAX`] bytes of raw log
//! in all, an excerpt of at most [`CI_LOG_EXCERPT_BYTES_MAX`] bytes and
//! [`CI_LOG_EXCERPT_LINES_MAX`] lines in all, and [`CI_LOG_DEADLINE`] for everything. A
//! caller can lower them ([`LogLimits`]) and never raise them. The processes run exactly as
//! the Phase 1 adapter runs `gh`: no stdin, no pager, no prompt, and the child is killed
//! when the future is dropped.
//!
//! # What is kept
//!
//! The raw log and `gh`'s stderr exist only inside the call. They are cleaned
//! ([`excerpt`]: redacted before anything is cut), and neither is stored in the bundle or
//! quoted in a failure. A log that the raw limit cut is cut back to its last whole line and
//! redacted as a stream; the cut piece is never quoted.
//!
//! # The text for the agent
//!
//! [`render_ci_log_data`] wraps the bundle in `<ci-log-data>`: one JSON object on one line,
//! with `<` and `>` written as `\u003c` and `\u003e`, so no log text can close the wrapper
//! or open another tag.

mod excerpt;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use storage::pull_requests::{
    CheckConclusion, CheckKind, CheckStatus, PullRequestCheck, PullRequestKey,
};

pub use self::excerpt::CI_LOG_LINE_BYTES_MAX;
use self::excerpt::{CleanedLog, clean_log, select_tail};
use super::FetchError;
use super::source::{
    ProcessError, ProcessOutput, ProcessRunner, ProcessSpec, TokioProcessRunner, checked_key,
    classify_failure, gh_process_spec,
};

/// The most runs a log is read from.
pub const CI_LOG_RUNS_MAX: usize = 3;
/// The most `gh api` calls that find and check runs.
pub const CI_LOG_METADATA_CALLS_MAX: usize = 8;
/// The most stdout read from one metadata call (the `--jq` selection is far smaller).
pub const CI_LOG_METADATA_BYTES_MAX: usize = 16 * 1024;
/// The most raw log bytes read, in all.
pub const CI_LOG_RAW_BYTES_MAX: usize = 1024 * 1024;
/// The most excerpt bytes, in all.
pub const CI_LOG_EXCERPT_BYTES_MAX: usize = 8 * 1024;
/// The most excerpt lines, in all.
pub const CI_LOG_EXCERPT_LINES_MAX: usize = 200;
/// The time for everything, from the first process to the last.
pub const CI_LOG_DEADLINE: Duration = Duration::from_secs(20);
/// The most check names listed for one run.
pub const CI_LOG_CHECK_NAMES_MAX: usize = 5;
/// The most skips listed in the text for the agent.
pub const CI_LOG_SKIPS_SHOWN_MAX: usize = 8;

/// The opening of the wrapper around a log excerpt.
pub const CI_LOG_DATA_OPEN: &str = "<ci-log-data>";
/// The closing of the wrapper around a log excerpt.
pub const CI_LOG_DATA_CLOSE: &str = "</ci-log-data>";
/// The `content_type` field of the wrapped object.
pub const CI_LOG_CONTENT_TYPE: &str = "data_not_instructions";

/// What is kept of a job: nothing else of the response is read.
const JOB_JQ: &str = "{id, run_id, head_sha, status, conclusion}";
/// What is kept of a run: its place (repository, head, pull requests) and whether it ended.
const RUN_JQ: &str = "{id, head_sha, event, status, conclusion, repository: .repository.full_name, pull_requests: [.pull_requests[]? | {number, base_url: .base.repo.url}]}";
/// The prefix of a pull request's `base.repo.url` in the API.
const API_REPOS_PREFIX: &str = "https://api.github.com/repos/";

/// The bounds of one call. [`LogLimits::default`] is the policy; a caller can only lower it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLimits {
    /// The time for the whole call.
    pub deadline: Duration,
    /// The most runs a log is read from.
    pub max_runs: usize,
    /// The most `gh api` calls.
    pub max_metadata_calls: usize,
    /// The most raw log bytes read, in all.
    pub raw_bytes: usize,
    /// The most excerpt bytes, in all.
    pub excerpt_bytes: usize,
    /// The most excerpt lines, in all.
    pub excerpt_lines: usize,
}

impl Default for LogLimits {
    fn default() -> Self {
        Self {
            deadline: CI_LOG_DEADLINE,
            max_runs: CI_LOG_RUNS_MAX,
            max_metadata_calls: CI_LOG_METADATA_CALLS_MAX,
            raw_bytes: CI_LOG_RAW_BYTES_MAX,
            excerpt_bytes: CI_LOG_EXCERPT_BYTES_MAX,
            excerpt_lines: CI_LOG_EXCERPT_LINES_MAX,
        }
    }
}

impl LogLimits {
    /// The limits with every value held to the policy maximum.
    #[must_use]
    pub fn clamped(&self) -> Self {
        Self {
            deadline: self.deadline.min(CI_LOG_DEADLINE),
            max_runs: self.max_runs.min(CI_LOG_RUNS_MAX),
            max_metadata_calls: self.max_metadata_calls.min(CI_LOG_METADATA_CALLS_MAX),
            raw_bytes: self.raw_bytes.min(CI_LOG_RAW_BYTES_MAX),
            excerpt_bytes: self.excerpt_bytes.min(CI_LOG_EXCERPT_BYTES_MAX),
            excerpt_lines: self.excerpt_lines.min(CI_LOG_EXCERPT_LINES_MAX),
        }
    }
}

/// What a notice needs a log for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRequest {
    /// The pull request (its base repository is where runs are looked up).
    pub key: PullRequestKey,
    /// The head commit the notice is about.
    pub head_sha: String,
    /// The failing checks to read logs for, as the snapshot holds them.
    pub checks: Vec<PullRequestCheck>,
    /// The bounds.
    pub limits: LogLimits,
}

impl LogRequest {
    /// A request with the default limits.
    #[must_use]
    pub fn new(
        key: PullRequestKey,
        head_sha: impl Into<String>,
        checks: Vec<PullRequestCheck>,
    ) -> Self {
        Self {
            key,
            head_sha: head_sha.into(),
            checks,
            limits: LogLimits::default(),
        }
    }
}

/// Why a check or run has no (or no more) log in the bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogSkipReason {
    /// The request itself is malformed (not a github.com key, not a commit ID).
    InvalidRequest,
    /// The check did not fail (or has not finished).
    NotFailed,
    /// The check is not a GitHub Actions job or run.
    ExternalCheck,
    /// The check or the run belongs to another commit than the notice's head.
    HeadMismatch,
    /// The run belongs to another repository.
    RepositoryMismatch,
    /// The run is not related to this pull request.
    PullRequestMismatch,
    /// GitHub has no such job or run in the repository.
    RunNotFound,
    /// The run has not finished, so it has no log yet.
    RunInProgress,
    /// The run's logs are gone (expired or deleted).
    LogExpired,
    /// The run produced no failed-step log.
    LogEmpty,
    /// `gh` is not signed in to github.com.
    Unauthenticated,
    /// The token may not read the repository's Actions.
    PermissionDenied,
    /// GitHub's rate limit was hit.
    RateLimited,
    /// The network failed.
    Network,
    /// The `gh` program does not exist.
    GhNotFound,
    /// `gh` could not be run, or failed in a way that fits nothing else.
    ProcessFailed,
    /// `gh` answered with something that cannot be trusted.
    InvalidResponse,
    /// The time for the call ran out. Omitted by a limit.
    DeadlineExceeded,
    /// More runs than [`CI_LOG_RUNS_MAX`]. Omitted by a limit.
    RunLimit,
    /// More metadata calls than [`CI_LOG_METADATA_CALLS_MAX`]. Omitted by a limit.
    MetadataLimit,
    /// The raw log budget was used up. Omitted by a limit.
    RawLimit,
    /// The excerpt budget left no room. Omitted by a limit.
    ExcerptLimit,
}

/// Whether something is missing because it could not be had, or because a limit was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSkipKind {
    /// The log cannot be had (or is not wanted): see the reason.
    Unavailable,
    /// A limit left it out; asking again with more room could have it.
    Omitted,
}

impl LogSkipReason {
    /// Whether this is a limit or a log that cannot be had.
    #[must_use]
    pub fn kind(self) -> LogSkipKind {
        match self {
            Self::DeadlineExceeded
            | Self::RunLimit
            | Self::MetadataLimit
            | Self::RawLimit
            | Self::ExcerptLimit => LogSkipKind::Omitted,
            _ => LogSkipKind::Unavailable,
        }
    }

    /// Whether every other check would fail the same way, so the call stops.
    fn aborts(self) -> bool {
        matches!(
            self,
            Self::GhNotFound
                | Self::Unauthenticated
                | Self::PermissionDenied
                | Self::RateLimited
                | Self::Network
                | Self::ProcessFailed
        )
    }
}

/// One check (or run) that has no log in the bundle, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CiLogSkip {
    /// The check's name (single-line and bounded by Phase 1); `None` for a whole request.
    pub check: Option<String>,
    /// The Actions run, when it was found.
    pub run_id: Option<u64>,
    /// Why.
    pub reason: LogSkipReason,
}

/// The excerpt of one run's failed-step log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CiLogRun {
    /// The Actions run the log is from.
    pub run_id: u64,
    /// The failing checks it answers (at most [`CI_LOG_CHECK_NAMES_MAX`]).
    pub checks: Vec<String>,
    /// The redacted excerpt: the last lines of the log that fit. Plain text, one `\n` between
    /// lines; escaping for the notice is [`render_ci_log_data`]'s job.
    pub excerpt: String,
    /// How many lines before the excerpt were left out.
    pub lines_omitted: usize,
    /// Whether the raw limit cut the log, so the excerpt is of its beginning, not its end.
    pub raw_truncated: bool,
    /// Whether a line was cut to [`CI_LOG_LINE_BYTES_MAX`].
    pub line_cut: bool,
    /// Whether the log was not valid UTF-8 and was repaired.
    pub lossy_utf8: bool,
    /// How many credentials were masked.
    pub redactions: usize,
}

/// What a call found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CiLogBundle {
    /// The head commit the logs are for.
    pub head_sha: String,
    /// The runs that have an excerpt, in the order of the checks.
    pub runs: Vec<CiLogRun>,
    /// The checks and runs that have none.
    pub skipped: Vec<CiLogSkip>,
    /// Whether anything was cut from an excerpt (lines, a raw cut, or a long line).
    pub truncated: bool,
}

impl CiLogBundle {
    /// A bundle with nothing in it but the reason.
    #[must_use]
    pub fn refused(head_sha: &str, reason: LogSkipReason) -> Self {
        Self {
            head_sha: head_sha.chars().take(64).collect(),
            runs: Vec::new(),
            skipped: vec![CiLogSkip {
                check: None,
                run_id: None,
                reason,
            }],
            truncated: false,
        }
    }

    /// Whether at least one run has an excerpt.
    #[must_use]
    pub fn has_excerpt(&self) -> bool {
        self.runs.iter().any(|run| !run.excerpt.is_empty())
    }

    /// Makes the biggest excerpt shorter by dropping its first half. `false` when no excerpt
    /// is left to shorten.
    fn shrink_largest_excerpt(&mut self) -> bool {
        let Some(run) = self
            .runs
            .iter_mut()
            .filter(|run| !run.excerpt.is_empty())
            .max_by_key(|run| run.excerpt.len())
        else {
            return false;
        };
        let lines: Vec<&str> = run.excerpt.split('\n').collect();
        let keep = lines.len() / 2;
        let dropped = lines.len() - keep;
        run.excerpt = lines[dropped..].join("\n");
        run.lines_omitted += dropped;
        self.truncated = true;
        true
    }
}

/// Wraps a bundle for an agent: `<ci-log-data>`, one line of JSON, `</ci-log-data>`.
///
/// The JSON carries `content_type` (`data_not_instructions`), the head, the runs with their
/// excerpts, and the first [`CI_LOG_SKIPS_SHOWN_MAX`] skips. Every `<` and `>` is written as
/// `\u003c` and `\u003e` (valid JSON escapes, and `<`/`>` occur only inside strings), so no
/// text of a log or a check name can contain the closing tag.
///
/// The returned text is never longer than `max_bytes`, escaping included. To get there, in
/// this order: excerpts lose their leading lines (biggest first); every run keeps one check
/// name; skips are listed fewer; runs are dropped from the last. What is dropped is counted in
/// `skipped_not_shown` and `runs_not_shown`, and `truncated` is set. The smallest text (the
/// head, the two counts and no run or skip) is [`minimal_ci_log_data`]'s length; with a budget
/// below it there is nothing safe to say and the result is `None`: the caller sends no log.
#[must_use]
pub fn render_ci_log_data(bundle: &CiLogBundle, max_bytes: usize) -> Option<String> {
    let mut view = View {
        bundle: bundle.clone(),
        skips_shown: bundle.skipped.len().min(CI_LOG_SKIPS_SHOWN_MAX),
        runs_hidden: 0,
    };
    loop {
        let text = render_once(&view);
        if text.len() <= max_bytes {
            return Some(text);
        }
        if !view.reduce() {
            return None;
        }
    }
}

/// The smallest text [`render_ci_log_data`] can return for `bundle`: no run, no skip.
///
/// It is the text the reduction in [`render_ci_log_data`] ends with, so `truncated` is the
/// bundle's own flag; leaving a run or a skip out sets it when [`render_once`] writes it.
/// (Forcing it to `true` made this text one byte shorter than the whole text of an empty,
/// untruncated bundle, which the limit check then refused although a budget of this size
/// fits.)
#[must_use]
pub fn minimal_ci_log_data(bundle: &CiLogBundle) -> String {
    render_once(&View {
        bundle: CiLogBundle {
            head_sha: bundle.head_sha.clone(),
            runs: Vec::new(),
            skipped: bundle.skipped.clone(),
            truncated: bundle.truncated,
        },
        skips_shown: 0,
        runs_hidden: bundle.runs.len(),
    })
}

/// A bundle and how much of it is shown.
struct View {
    bundle: CiLogBundle,
    skips_shown: usize,
    runs_hidden: usize,
}

impl View {
    /// Makes the text smaller by one step. `false` when nothing is left to take out.
    fn reduce(&mut self) -> bool {
        if self.bundle.shrink_largest_excerpt() {
            return true;
        }
        if let Some(run) = self.bundle.runs.iter_mut().find(|run| run.checks.len() > 1) {
            run.checks.truncate(1);
            self.bundle.truncated = true;
            return true;
        }
        if self.skips_shown > 0 {
            self.skips_shown /= 2;
            return true;
        }
        if self.bundle.runs.pop().is_some() {
            self.runs_hidden += 1;
            self.bundle.truncated = true;
            return true;
        }
        false
    }
}

fn render_once(view: &View) -> String {
    #[derive(Serialize)]
    struct Payload<'a> {
        content_type: &'static str,
        head_sha: &'a str,
        truncated: bool,
        runs: &'a [CiLogRun],
        runs_not_shown: usize,
        skipped: &'a [CiLogSkip],
        skipped_not_shown: usize,
    }
    let bundle = &view.bundle;
    let shown = view.skips_shown.min(bundle.skipped.len());
    let payload = Payload {
        content_type: CI_LOG_CONTENT_TYPE,
        head_sha: &bundle.head_sha,
        truncated: bundle.truncated || view.runs_hidden > 0 || shown < bundle.skipped.len(),
        runs: &bundle.runs,
        runs_not_shown: view.runs_hidden,
        skipped: &bundle.skipped[..shown],
        skipped_not_shown: bundle.skipped.len() - shown,
    };
    let json = serde_json::to_string(&payload).expect("a bundle serializes to JSON");
    let escaped = json.replace('<', "\\u003c").replace('>', "\\u003e");
    format!("{CI_LOG_DATA_OPEN}\n{escaped}\n{CI_LOG_DATA_CLOSE}")
}

/// Gets the failure logs of a pull request's checks. Implementations are cancel-safe:
/// dropping the future ends the call and the `gh` child.
#[async_trait]
pub trait PullRequestLogSource: Send + Sync {
    /// Reads the logs of `request.checks` that belong to `request.head_sha`.
    ///
    /// This never fails: whatever cannot be had is a [`CiLogSkip`] in the bundle.
    async fn fetch_failed_logs(&self, request: &LogRequest) -> CiLogBundle;
}

/// Reads logs with the `gh` CLI.
#[derive(Clone)]
pub struct GhLogSource {
    runner: Arc<dyn ProcessRunner>,
    program: String,
}

impl std::fmt::Debug for GhLogSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GhLogSource")
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

impl Default for GhLogSource {
    fn default() -> Self {
        Self::new()
    }
}

impl GhLogSource {
    /// `gh` from `PATH`, run as a real process.
    #[must_use]
    pub fn new() -> Self {
        Self::with_runner(Arc::new(TokioProcessRunner))
    }

    /// `gh` from `PATH`, run by `runner` (a fake in tests).
    #[must_use]
    pub fn with_runner(runner: Arc<dyn ProcessRunner>) -> Self {
        Self {
            runner,
            program: "gh".to_owned(),
        }
    }

    /// Runs `program` instead of `gh` from `PATH`.
    #[must_use]
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }
}

/// The request after validation: what the steps compare against.
struct Ctx {
    key: PullRequestKey,
    head_sha: String,
    limits: LogLimits,
}

/// A run that passed every check, and the checks that led to it.
struct AcceptedRun {
    run_id: u64,
    checks: Vec<String>,
}

struct FetchedLog {
    run_id: u64,
    checks: Vec<String>,
    cleaned: CleanedLog,
    raw_truncated: bool,
}

/// Everything the steps have found so far. It lives outside the steps' future so that what
/// was found before the deadline is still there when the future is dropped.
struct State {
    skipped: Vec<CiLogSkip>,
    accepted: Vec<AcceptedRun>,
    runs: HashMap<u64, Result<usize, LogSkipReason>>,
    jobs: HashMap<u64, Result<u64, LogSkipReason>>,
    metadata_calls: usize,
    raw_remaining: usize,
    next_check: usize,
    next_log: usize,
    fetched: Vec<FetchedLog>,
    abort: Option<LogSkipReason>,
}

impl State {
    fn new(raw_bytes: usize) -> Self {
        Self {
            skipped: Vec::new(),
            accepted: Vec::new(),
            runs: HashMap::new(),
            jobs: HashMap::new(),
            metadata_calls: 0,
            raw_remaining: raw_bytes,
            next_check: 0,
            next_log: 0,
            fetched: Vec::new(),
            abort: None,
        }
    }

    fn skip(&mut self, check: Option<&str>, run_id: Option<u64>, reason: LogSkipReason) {
        self.skipped.push(CiLogSkip {
            check: check.map(str::to_owned),
            run_id,
            reason,
        });
        if reason.aborts() {
            self.abort = Some(reason);
        }
    }
}

/// An Actions job or run, as a check's provider ID names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionsRef {
    Job(u64),
    Run(u64),
}

fn parse_actions_ref(provider_id: &str) -> Option<ActionsRef> {
    let (kind, id) = provider_id.split_once(':')?;
    if id.is_empty() || id.len() > 20 || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let id = id.parse::<u64>().ok().filter(|id| *id > 0)?;
    match kind {
        "job" => Some(ActionsRef::Job(id)),
        "run" => Some(ActionsRef::Run(id)),
        _ => None,
    }
}

#[derive(Deserialize)]
struct RawJob {
    id: u64,
    run_id: u64,
    head_sha: String,
}

#[derive(Deserialize)]
struct RawRun {
    id: u64,
    head_sha: String,
    status: Option<String>,
    repository: Option<String>,
    #[serde(default)]
    pull_requests: Vec<RawRunPullRequest>,
}

#[derive(Deserialize)]
struct RawRunPullRequest {
    number: u64,
    base_url: Option<String>,
}

#[async_trait]
impl PullRequestLogSource for GhLogSource {
    async fn fetch_failed_logs(&self, request: &LogRequest) -> CiLogBundle {
        let Ok(key) = checked_key(&request.key) else {
            return CiLogBundle::refused(&request.head_sha, LogSkipReason::InvalidRequest);
        };
        let head_sha = request.head_sha.trim().to_ascii_lowercase();
        if !matches!(head_sha.len(), 40 | 64) || !head_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return CiLogBundle::refused(&request.head_sha, LogSkipReason::InvalidRequest);
        }
        let limits = request.limits.clamped();
        let ctx = Ctx {
            key,
            head_sha,
            limits,
        };
        let mut state = State::new(ctx.limits.raw_bytes);
        let finished = tokio::time::timeout(
            ctx.limits.deadline,
            self.collect(&ctx, &request.checks, &mut state),
        )
        .await
        .is_ok();
        // Whatever was not reached is left out for the reason that stopped the call.
        let stopped = state.abort.unwrap_or(LogSkipReason::DeadlineExceeded);
        if !finished || state.abort.is_some() {
            for check in &request.checks[state.next_check.min(request.checks.len())..] {
                state.skip(Some(&check.name), None, stopped);
            }
            for index in state.next_log..state.accepted.len() {
                let run = &state.accepted[index];
                let (name, run_id) = (run.checks.first().cloned(), run.run_id);
                state.skip(name.as_deref(), Some(run_id), stopped);
            }
        }
        build_bundle(&ctx, state)
    }
}

impl GhLogSource {
    async fn collect(&self, ctx: &Ctx, checks: &[PullRequestCheck], state: &mut State) {
        for (index, check) in checks.iter().enumerate() {
            if state.abort.is_some() {
                return;
            }
            state.next_check = index;
            if let Err(reason) = self.resolve_check(ctx, check, state).await {
                state.skip(Some(&check.name), None, reason);
            }
            state.next_check = index + 1;
        }
        self.read_logs(ctx, state).await;
    }

    async fn resolve_check(
        &self,
        ctx: &Ctx,
        check: &PullRequestCheck,
        state: &mut State,
    ) -> Result<(), LogSkipReason> {
        if check.kind != CheckKind::CheckRun {
            return Err(LogSkipReason::ExternalCheck);
        }
        let failed = matches!(
            check.conclusion,
            Some(CheckConclusion::Failure | CheckConclusion::TimedOut)
        );
        if check.status != CheckStatus::Completed || !failed {
            return Err(LogSkipReason::NotFailed);
        }
        if !check.head_sha.eq_ignore_ascii_case(&ctx.head_sha) {
            return Err(LogSkipReason::HeadMismatch);
        }
        let run_id = match parse_actions_ref(&check.provider_id) {
            None => return Err(LogSkipReason::ExternalCheck),
            Some(ActionsRef::Run(run_id)) => run_id,
            Some(ActionsRef::Job(job_id)) => self.resolve_job(ctx, job_id, state).await?,
        };
        let index = self.resolve_run(ctx, run_id, state).await?;
        let names = &mut state.accepted[index].checks;
        if names.len() < CI_LOG_CHECK_NAMES_MAX && !names.contains(&check.name) {
            names.push(check.name.clone());
        }
        Ok(())
    }

    /// The run a job belongs to, after the job is checked against the head.
    async fn resolve_job(
        &self,
        ctx: &Ctx,
        job_id: u64,
        state: &mut State,
    ) -> Result<u64, LogSkipReason> {
        if let Some(known) = state.jobs.get(&job_id) {
            return *known;
        }
        if state.accepted.len() >= ctx.limits.max_runs {
            return Err(LogSkipReason::RunLimit);
        }
        let endpoint = format!("repos/{}/actions/jobs/{job_id}", ctx.key.repository);
        let answer = match self.metadata::<RawJob>(ctx, &endpoint, JOB_JQ, state).await {
            Ok(job) if job.id != job_id || job.run_id == 0 => Err(LogSkipReason::InvalidResponse),
            Ok(job) if !job.head_sha.eq_ignore_ascii_case(&ctx.head_sha) => {
                Err(LogSkipReason::HeadMismatch)
            }
            Ok(job) => Ok(job.run_id),
            Err(LogSkipReason::MetadataLimit) => return Err(LogSkipReason::MetadataLimit),
            Err(reason) => Err(reason),
        };
        state.jobs.insert(job_id, answer);
        answer
    }

    /// The index in `state.accepted` of a run that passed every check.
    async fn resolve_run(
        &self,
        ctx: &Ctx,
        run_id: u64,
        state: &mut State,
    ) -> Result<usize, LogSkipReason> {
        if let Some(known) = state.runs.get(&run_id) {
            return *known;
        }
        if state.accepted.len() >= ctx.limits.max_runs {
            return Err(LogSkipReason::RunLimit);
        }
        let endpoint = format!("repos/{}/actions/runs/{run_id}", ctx.key.repository);
        let answer = match self.metadata::<RawRun>(ctx, &endpoint, RUN_JQ, state).await {
            Ok(run) => verify_run(ctx, run_id, &run),
            Err(LogSkipReason::MetadataLimit) => return Err(LogSkipReason::MetadataLimit),
            Err(reason) => Err(reason),
        };
        let answer = answer.map(|()| {
            state.accepted.push(AcceptedRun {
                run_id,
                checks: Vec::new(),
            });
            state.accepted.len() - 1
        });
        state.runs.insert(run_id, answer);
        answer
    }

    async fn metadata<T: DeserializeOwned>(
        &self,
        ctx: &Ctx,
        endpoint: &str,
        jq: &str,
        state: &mut State,
    ) -> Result<T, LogSkipReason> {
        if state.metadata_calls >= ctx.limits.max_metadata_calls {
            return Err(LogSkipReason::MetadataLimit);
        }
        state.metadata_calls += 1;
        let args = [
            "api",
            "--hostname",
            &ctx.key.host,
            "--method",
            "GET",
            endpoint,
            "--jq",
            jq,
        ]
        .map(str::to_owned)
        .to_vec();
        let output = self
            .run(gh_process_spec(
                &self.program,
                args,
                CI_LOG_METADATA_BYTES_MAX,
            ))
            .await?;
        if output.stdout_truncated {
            return Err(LogSkipReason::InvalidResponse);
        }
        if output.exit_code != Some(0) {
            return Err(reason_from_failure(&output));
        }
        serde_json::from_slice(&output.stdout).map_err(|_| LogSkipReason::InvalidResponse)
    }

    async fn read_logs(&self, ctx: &Ctx, state: &mut State) {
        while state.next_log < state.accepted.len() {
            if state.abort.is_some() {
                return;
            }
            let index = state.next_log;
            let run_id = state.accepted[index].run_id;
            let checks = state.accepted[index].checks.clone();
            let first_check = checks.first().cloned();
            if state.raw_remaining == 0 {
                state.skip(
                    first_check.as_deref(),
                    Some(run_id),
                    LogSkipReason::RawLimit,
                );
                state.next_log += 1;
                continue;
            }
            let args = [
                "run".to_owned(),
                "view".to_owned(),
                run_id.to_string(),
                "--repo".to_owned(),
                format!("{}/{}", ctx.key.host, ctx.key.repository),
                "--log-failed".to_owned(),
            ]
            .to_vec();
            let spec = gh_process_spec(&self.program, args, state.raw_remaining);
            let output = match self.run(spec).await {
                Ok(output) => output,
                Err(reason) => {
                    state.skip(first_check.as_deref(), Some(run_id), reason);
                    state.next_log += 1;
                    continue;
                }
            };
            // What the process printed counts against the budget whatever its exit code says.
            state.raw_remaining = if output.stdout_truncated {
                0
            } else {
                state.raw_remaining.saturating_sub(output.stdout.len())
            };
            // The runner ends a process whose output passed the limit, so a cut log has no
            // exit code of its own.
            if output.exit_code != Some(0) && !output.stdout_truncated {
                state.skip(
                    first_check.as_deref(),
                    Some(run_id),
                    reason_from_failure(&output),
                );
            } else {
                let cleaned = clean_log(&output.stdout, output.stdout_truncated);
                if cleaned.lines.is_empty() {
                    state.skip(
                        first_check.as_deref(),
                        Some(run_id),
                        LogSkipReason::LogEmpty,
                    );
                } else {
                    state.fetched.push(FetchedLog {
                        run_id,
                        checks,
                        cleaned,
                        raw_truncated: output.stdout_truncated,
                    });
                }
            }
            state.next_log += 1;
        }
    }

    async fn run(&self, spec: ProcessSpec) -> Result<ProcessOutput, LogSkipReason> {
        self.runner.run(spec).await.map_err(|error| match error {
            ProcessError::NotFound => LogSkipReason::GhNotFound,
            ProcessError::Spawn(_) | ProcessError::Io(_) => LogSkipReason::ProcessFailed,
        })
    }
}

/// Whether the run is in the base repository, at the head, related to the pull request, and
/// finished.
fn verify_run(ctx: &Ctx, run_id: u64, run: &RawRun) -> Result<(), LogSkipReason> {
    if run.id != run_id {
        return Err(LogSkipReason::InvalidResponse);
    }
    let repository = run.repository.as_deref().map(str::to_ascii_lowercase);
    if repository.as_deref() != Some(ctx.key.repository.as_str()) {
        return Err(LogSkipReason::RepositoryMismatch);
    }
    if !run.head_sha.eq_ignore_ascii_case(&ctx.head_sha) {
        return Err(LogSkipReason::HeadMismatch);
    }
    let related = run.pull_requests.iter().any(|pull_request| {
        pull_request.number == ctx.key.number
            && pull_request
                .base_url
                .as_deref()
                .and_then(|url| url.strip_prefix(API_REPOS_PREFIX))
                .is_some_and(|repository| repository.eq_ignore_ascii_case(&ctx.key.repository))
    });
    if !related {
        return Err(LogSkipReason::PullRequestMismatch);
    }
    if run.status.as_deref() != Some("completed") {
        return Err(LogSkipReason::RunInProgress);
    }
    Ok(())
}

/// What a failed `gh` call means. The stderr only classifies; none of it is kept.
fn reason_from_failure(output: &ProcessOutput) -> LogSkipReason {
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    if stderr.contains("still in progress") {
        return LogSkipReason::RunInProgress;
    }
    if stderr.contains("log not found")
        || stderr.contains("http 410")
        || stderr.contains("logs have expired")
        || stderr.contains("no logs")
    {
        return LogSkipReason::LogExpired;
    }
    match classify_failure(output) {
        FetchError::Unauthenticated(_) => LogSkipReason::Unauthenticated,
        FetchError::PermissionDenied(_) => LogSkipReason::PermissionDenied,
        FetchError::RateLimited(_) => LogSkipReason::RateLimited,
        FetchError::Network(_) => LogSkipReason::Network,
        FetchError::NotFound(_) => LogSkipReason::RunNotFound,
        FetchError::GhNotFound => LogSkipReason::GhNotFound,
        _ => LogSkipReason::ProcessFailed,
    }
}

/// Shares the excerpt budget among the logs and assembles the bundle.
fn build_bundle(ctx: &Ctx, mut state: State) -> CiLogBundle {
    let count = state.fetched.len();
    let mut bytes_left = ctx.limits.excerpt_bytes;
    let mut lines_left = ctx.limits.excerpt_lines;
    let mut runs = Vec::with_capacity(count);
    let mut truncated = false;
    for (index, log) in std::mem::take(&mut state.fetched).into_iter().enumerate() {
        // Each log may take an equal share of what is left; what it leaves unused goes to
        // the ones after it.
        let sharers = count - index;
        let (excerpt, lines_omitted) = select_tail(
            &log.cleaned.lines,
            bytes_left / sharers,
            lines_left / sharers,
        );
        if excerpt.is_empty() {
            state.skip(
                log.checks.first().map(String::as_str),
                Some(log.run_id),
                LogSkipReason::ExcerptLimit,
            );
            continue;
        }
        bytes_left = bytes_left.saturating_sub(excerpt.len());
        lines_left = lines_left.saturating_sub(excerpt.matches('\n').count() + 1);
        truncated |= lines_omitted > 0 || log.raw_truncated || log.cleaned.line_cut;
        runs.push(CiLogRun {
            run_id: log.run_id,
            checks: log.checks,
            excerpt,
            lines_omitted,
            raw_truncated: log.raw_truncated,
            line_cut: log.cleaned.line_cut,
            lossy_utf8: log.cleaned.lossy_utf8,
            redactions: log.cleaned.redactions,
        });
    }
    CiLogBundle {
        head_sha: ctx.head_sha.clone(),
        runs,
        skipped: state.skipped,
        truncated,
    }
}

/// A [`PullRequestLogSource`] that answers from a script and records the requests.
#[derive(Debug, Default, Clone)]
pub struct FakePullRequestLogSource {
    state: Arc<Mutex<FakeLogState>>,
}

#[derive(Debug, Default)]
struct FakeLogState {
    script: std::collections::VecDeque<CiLogBundle>,
    requests: Vec<LogRequest>,
}

impl FakePullRequestLogSource {
    /// An empty script.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues the bundle the next call returns.
    pub fn push(&self, bundle: CiLogBundle) {
        self.lock().script.push_back(bundle);
    }

    /// Every request so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<LogRequest> {
        self.lock().requests.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeLogState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl PullRequestLogSource for FakePullRequestLogSource {
    async fn fetch_failed_logs(&self, request: &LogRequest) -> CiLogBundle {
        let mut state = self.lock();
        state.requests.push(request.clone());
        state.script.pop_front().unwrap_or_else(|| {
            CiLogBundle::refused(&request.head_sha, LogSkipReason::ProcessFailed)
        })
    }
}

#[cfg(test)]
mod tests;
