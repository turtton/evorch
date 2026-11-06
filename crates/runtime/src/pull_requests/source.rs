//! The `gh` adapter: fetches a pull request's facts by running the GitHub CLI.
//!
//! # What is asked for
//!
//! One fixed command, built as an argument vector (no shell string):
//!
//! ```text
//! gh pr view <number> --repo github.com/<owner>/<repo> --json <GH_JSON_FIELDS>
//! ```
//!
//! The host is written into `--repo`, and `GH_HOST`/`GH_REPO` are removed from the
//! environment, so no variable can send the request to another host. The fields are
//! [`GH_JSON_FIELDS`]: identity, state, base and head, and `statusCheckRollup` and
//! `reviewDecision`. The title, body, comments, reviews, commits, files and every count are
//! not requested; a test pins the list.
//!
//! # Checks and their pagination
//!
//! `gh pr view --json statusCheckRollup` follows GitHub's pagination itself (100 checks per
//! page, all pages, in `cli/cli` `pkg/cmd/pr/shared/finder.go: preloadPrChecks`), and its JSON
//! carries neither `pageInfo` nor a total. So a list that arrives is the whole list, and a
//! failure while paging is a failed `gh`. A missing or `null` list (gh prints `null` when it
//! found no check data) is not known to be complete; only an explicit `[]` is a known "no
//! checks", and even that is not a success. The adapter adds its own bound: more than
//! [`PULL_REQUEST_CHECKS_MAX`] checks are cut (failing and running ones are kept first) and
//! the list is marked incomplete. The output is bounded too ([`GH_STDOUT_LIMIT`]); output
//! over the bound is an error, not a shorter answer. The checks belong to the head commit
//! `headRefOid` of the same response, which is the SHA recorded for each of them.
//!
//! `gh` exposes no check ID: the provider ID is built from the Actions job or run in
//! `detailsUrl` when it is on github.com, and from the kind and name otherwise.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use storage::pull_requests::{
    CheckConclusion, CheckKind, CheckStatus, PULL_REQUEST_BRANCH_MAX_CHARS,
    PULL_REQUEST_CHECK_ID_MAX_CHARS, PULL_REQUEST_CHECK_NAME_MAX_CHARS, PULL_REQUEST_CHECKS_MAX,
    PullRequestCheck, PullRequestFacts, PullRequestKey, PullRequestState, ReviewDecision,
};
use tokio::io::AsyncReadExt;

use super::{FetchError, FetchedPullRequest, PullRequestSource, clean_text, locator};

/// The `--json` fields requested from `gh pr view`. Nothing else is asked for.
pub const GH_JSON_FIELDS: &[&str] = &[
    "number",
    "url",
    "state",
    "baseRefName",
    "headRefName",
    "headRefOid",
    "headRepository",
    "headRepositoryOwner",
    "reviewDecision",
    "statusCheckRollup",
];

/// The most stdout read from `gh`. Over it, the fetch fails.
pub const GH_STDOUT_LIMIT: usize = 4 * 1024 * 1024;
/// The most stderr read from `gh`.
pub const GH_STDERR_LIMIT: usize = 16 * 1024;

/// Variables that could change what `gh` talks to or how it behaves.
const ENV_REMOVE: &[&str] = &[
    "GH_HOST",
    "GH_REPO",
    "GH_DEBUG",
    "GH_FORCE_TTY",
    "GH_BROWSER",
    "GH_EDITOR",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
];

/// Variables that keep `gh` non-interactive and quiet. The token is never read or set here:
/// `gh` uses its own authentication.
const ENV_SET: &[(&str, &str)] = &[
    ("GH_PROMPT_DISABLED", "1"),
    ("GH_PAGER", "cat"),
    ("PAGER", "cat"),
    ("NO_COLOR", "1"),
    ("CLICOLOR", "0"),
    ("GH_NO_UPDATE_NOTIFIER", "1"),
    ("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1"),
    ("GH_SPINNER_DISABLED", "1"),
    ("TERM", "dumb"),
];

/// A program to run, with everything about its environment spelled out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessSpec {
    /// The program, looked up on `PATH` when it has no `/`.
    pub program: String,
    /// The arguments, one by one (no shell is involved).
    pub args: Vec<String>,
    /// Environment variables removed before the program starts.
    pub env_remove: Vec<String>,
    /// Environment variables set for the program.
    pub env_set: Vec<(String, String)>,
    /// The most stdout bytes kept; a longer output marks [`ProcessOutput::stdout_truncated`]
    /// and ends the process.
    pub stdout_limit: usize,
    /// The most stderr bytes kept (the rest is read and dropped).
    pub stderr_limit: usize,
}

/// What a finished process left.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessOutput {
    /// The exit code; `None` when a signal ended the process.
    pub exit_code: Option<i32>,
    /// Stdout, at most `stdout_limit` bytes.
    pub stdout: Vec<u8>,
    /// Whether stdout was longer than the limit.
    pub stdout_truncated: bool,
    /// Stderr, at most `stderr_limit` bytes.
    pub stderr: Vec<u8>,
    /// Whether stderr was longer than the limit. A cut text can end in the middle of a
    /// credential, so it is used to classify the failure but never quoted.
    pub stderr_truncated: bool,
}

/// Why a process could not be run at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProcessError {
    /// The program does not exist.
    #[error("program not found")]
    NotFound,
    /// The program could not be started.
    #[error("could not start the program: {0}")]
    Spawn(String),
    /// Its output could not be read.
    #[error("could not read the program's output: {0}")]
    Io(String),
}

/// Runs a [`ProcessSpec`]. Dropping the returned future ends the process.
#[async_trait]
pub trait ProcessRunner: Send + Sync {
    /// Runs the process to its end (or until its stdout passes the limit).
    ///
    /// # Errors
    /// [`ProcessError`] when it cannot start or its output cannot be read.
    async fn run(&self, spec: ProcessSpec) -> Result<ProcessOutput, ProcessError>;
}

/// Runs real processes with `tokio`. The child has no stdin, and is killed when the future
/// is dropped (a cancelled or timed-out fetch) or once its stdout passes the limit.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioProcessRunner;

#[async_trait]
impl ProcessRunner for TokioProcessRunner {
    async fn run(&self, spec: ProcessSpec) -> Result<ProcessOutput, ProcessError> {
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for name in &spec.env_remove {
            command.env_remove(name);
        }
        for (name, value) in &spec.env_set {
            command.env(name, value);
        }
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ProcessError::NotFound
            } else {
                ProcessError::Spawn(error.kind().to_string())
            }
        })?;
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let (out, err) = tokio::join!(
            async {
                let read = read_capped(&mut stdout, spec.stdout_limit).await;
                if matches!(read, Ok((_, true))) {
                    let _ = child.start_kill();
                }
                read
            },
            async {
                let read = read_capped(&mut stderr, spec.stderr_limit).await;
                // What is over the limit is read and dropped, so the child never blocks.
                let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
                read
            }
        );
        let (stdout, stdout_truncated) =
            out.map_err(|error| ProcessError::Io(error.to_string()))?;
        let (stderr, stderr_truncated) =
            err.map_err(|error| ProcessError::Io(error.to_string()))?;
        let status = child
            .wait()
            .await
            .map_err(|error| ProcessError::Io(error.to_string()))?;
        Ok(ProcessOutput {
            exit_code: status.code(),
            stdout,
            stdout_truncated,
            stderr,
            stderr_truncated,
        })
    }
}

/// Reads up to `limit` bytes; the flag says whether more was available.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    let exceeded = bytes.len() > limit;
    bytes.truncate(limit);
    Ok((bytes, exceeded))
}

/// Fetches pull requests with the `gh` CLI.
#[derive(Clone)]
pub struct GhSource {
    runner: Arc<dyn ProcessRunner>,
    program: String,
    clock: fn() -> i64,
}

impl std::fmt::Debug for GhSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GhSource")
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

impl Default for GhSource {
    fn default() -> Self {
        Self::new()
    }
}

impl GhSource {
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
            clock: crate::run_store::review_time_ns,
        }
    }

    /// Runs `program` instead of `gh` from `PATH` (an absolute path, or a stand-in in tests).
    #[must_use]
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    /// Stamps fetches with `clock` (nanoseconds since the Unix epoch) instead of the system
    /// clock.
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> i64) -> Self {
        self.clock = clock;
        self
    }

    /// The process that fetching `key` runs.
    ///
    /// # Errors
    /// [`FetchError::InvalidHost`] or [`FetchError::InvalidReference`] for a key that is not
    /// a github.com pull request.
    pub fn spec(&self, key: &PullRequestKey) -> Result<ProcessSpec, FetchError> {
        let key = checked_key(key)?;
        Ok(ProcessSpec {
            program: self.program.clone(),
            args: gh_arguments(&key),
            env_remove: ENV_REMOVE.iter().map(|name| (*name).to_owned()).collect(),
            env_set: ENV_SET
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            stdout_limit: GH_STDOUT_LIMIT,
            stderr_limit: GH_STDERR_LIMIT,
        })
    }
}

pub(super) fn checked_key(key: &PullRequestKey) -> Result<PullRequestKey, FetchError> {
    key.normalized().map_err(|_| {
        if key.host.trim().eq_ignore_ascii_case("github.com") {
            FetchError::InvalidReference("malformed repository or number".into())
        } else {
            FetchError::InvalidHost(clean_text(&format!("host {:?}", key.host), 64))
        }
    })
}

/// A `gh` process with the environment every `gh` call here runs in: non-interactive, no
/// pager or colour, and no variable that could point it at another host. `stdout_limit`
/// bounds what is read; the stderr limit is [`GH_STDERR_LIMIT`].
pub(super) fn gh_process_spec(
    program: &str,
    args: Vec<String>,
    stdout_limit: usize,
) -> ProcessSpec {
    ProcessSpec {
        program: program.to_owned(),
        args,
        env_remove: ENV_REMOVE.iter().map(|name| (*name).to_owned()).collect(),
        env_set: ENV_SET
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        stdout_limit,
        stderr_limit: GH_STDERR_LIMIT,
    }
}

/// The argument vector of `gh pr view` for `key` (a normalized key).
#[must_use]
pub fn gh_arguments(key: &PullRequestKey) -> Vec<String> {
    vec![
        "pr".to_owned(),
        "view".to_owned(),
        key.number.to_string(),
        "--repo".to_owned(),
        format!("{}/{}", key.host, key.repository),
        "--json".to_owned(),
        GH_JSON_FIELDS.join(","),
    ]
}

#[async_trait]
impl PullRequestSource for GhSource {
    async fn fetch(
        &self,
        key: &PullRequestKey,
        deadline: Duration,
    ) -> Result<FetchedPullRequest, FetchError> {
        let key = checked_key(key)?;
        let spec = self.spec(&key)?;
        let output = match tokio::time::timeout(deadline, self.runner.run(spec)).await {
            Err(_elapsed) => return Err(FetchError::Timeout(deadline)),
            Ok(Err(ProcessError::NotFound)) => return Err(FetchError::GhNotFound),
            Ok(Err(error)) => return Err(FetchError::other(&error.to_string())),
            Ok(Ok(output)) => output,
        };
        if output.exit_code != Some(0) {
            return Err(classify_failure(&output));
        }
        if output.stdout_truncated {
            return Err(FetchError::invalid_response("output over the size limit"));
        }
        let fetched_at_ns = (self.clock)();
        let (canonical, url, facts) = parse_output(&key, &output.stdout)?;
        Ok(FetchedPullRequest {
            requested: key,
            canonical,
            url,
            facts,
            fetched_at_ns,
        })
    }
}

/// What a failed `gh` run means, from its exit code and (bounded, redacted) stderr.
pub(super) fn classify_failure(output: &ProcessOutput) -> FetchError {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lower = stderr.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    // Text that was cut at the limit may end inside a credential that no rule can recognize
    // any more, so it classifies the failure but is not quoted.
    let detail = if output.stderr_truncated {
        format!(
            "exit code {:?}; stderr over the size limit (not quoted)",
            output.exit_code
        )
    } else if stderr.trim().is_empty() {
        format!("exit code {:?}", output.exit_code)
    } else {
        stderr.into_owned()
    };
    if has(&["rate limit", "http 429", "abuse detection"]) {
        FetchError::rate_limited(&detail)
    } else if output.exit_code == Some(4)
        || has(&[
            "gh auth login",
            "not logged in",
            "http 401",
            "bad credentials",
            "requires authentication",
            "authentication failed",
        ])
    {
        FetchError::unauthenticated(&detail)
    } else if has(&[
        "http 403",
        "resource not accessible",
        "saml",
        "forbidden",
        "must be granted",
        "scope",
    ]) {
        FetchError::permission_denied(&detail)
    } else if has(&[
        "could not resolve to a pullrequest",
        "could not resolve to a repository",
        "http 404",
        "not found",
        "no pull requests found",
    ]) {
        FetchError::not_found(&detail)
    } else if has(&[
        "dial tcp",
        "no such host",
        "connection refused",
        "connection reset",
        "network is unreachable",
        "i/o timeout",
        "tls handshake",
        "timeout awaiting",
        "context deadline exceeded",
        "unexpected eof",
        "temporary failure in name resolution",
    ]) {
        FetchError::network(&detail)
    } else {
        FetchError::other(&detail)
    }
}

// ---------------------------------------------------------------------------------------
// The answer: `gh pr view --json <GH_JSON_FIELDS>`

#[derive(Deserialize)]
struct RawPullRequest {
    number: Option<u64>,
    url: Option<String>,
    state: Option<String>,
    #[serde(rename = "baseRefName")]
    base_ref_name: Option<String>,
    #[serde(rename = "headRefName")]
    head_ref_name: Option<String>,
    #[serde(rename = "headRefOid")]
    head_ref_oid: Option<String>,
    #[serde(rename = "headRepository")]
    head_repository: Option<RawRepository>,
    #[serde(rename = "headRepositoryOwner")]
    head_repository_owner: Option<RawOwner>,
    #[serde(rename = "reviewDecision")]
    review_decision: Option<String>,
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Option<Vec<RawCheck>>,
}

#[derive(Deserialize)]
struct RawRepository {
    name: Option<String>,
}

#[derive(Deserialize)]
struct RawOwner {
    login: Option<String>,
}

#[derive(Deserialize)]
struct RawCheck {
    #[serde(rename = "__typename")]
    typename: Option<String>,
    name: Option<String>,
    #[serde(rename = "workflowName")]
    workflow_name: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    #[serde(rename = "detailsUrl")]
    details_url: Option<String>,
    context: Option<String>,
    state: Option<String>,
}

fn invalid(detail: &str) -> FetchError {
    FetchError::invalid_response(detail)
}

fn required<T>(value: Option<T>, field: &str) -> Result<T, FetchError> {
    value.ok_or_else(|| invalid(&format!("missing {field}")))
}

/// Turns `gh`'s JSON into the canonical key, the URL and the facts. Pure: the same bytes
/// always give the same result.
///
/// # Errors
/// [`FetchError::InvalidResponse`] for anything that cannot be trusted: JSON that does not
/// parse, a missing field, an unknown state, review decision, check type, status or state,
/// a check without a name or with a conclusion that does not fit its status, a head SHA or
/// branch that is not valid, a number or URL that is not the requested pull request.
pub fn parse_output(
    requested: &PullRequestKey,
    stdout: &[u8],
) -> Result<(PullRequestKey, String, PullRequestFacts), FetchError> {
    let raw: RawPullRequest = serde_json::from_slice(stdout).map_err(|error| {
        invalid(&format!(
            "not the expected JSON ({})",
            error.classify_name()
        ))
    })?;
    if required(raw.number, "number")? != requested.number {
        return Err(invalid("the number is not the requested one"));
    }
    let url = required(raw.url, "url")?;
    let canonical = canonical_key(&url, requested.number)?;
    let state = match required(raw.state, "state")?.as_str() {
        "OPEN" => PullRequestState::Open,
        "CLOSED" => PullRequestState::Closed,
        "MERGED" => PullRequestState::Merged,
        other => return Err(invalid(&format!("unknown state {other:?}"))),
    };
    let review_decision = match raw.review_decision.as_deref().unwrap_or_default() {
        "" => ReviewDecision::None,
        "APPROVED" => ReviewDecision::Approved,
        "CHANGES_REQUESTED" => ReviewDecision::ChangesRequested,
        "REVIEW_REQUIRED" => ReviewDecision::ReviewRequired,
        other => return Err(invalid(&format!("unknown review decision {other:?}"))),
    };
    let base_branch = branch(required(raw.base_ref_name, "baseRefName")?, "baseRefName")?;
    let head_branch = branch(required(raw.head_ref_name, "headRefName")?, "headRefName")?;
    let head_sha = commit_id(&required(raw.head_ref_oid, "headRefOid")?)?;
    let head_repository = head_repository(raw.head_repository, raw.head_repository_owner)?;
    // An explicit `[]` is a known "no checks"; a missing or null list is not known to be
    // complete (gh prints null when it found no check data at all).
    let (checks, checks_complete) = match raw.status_check_rollup {
        Some(list) => checks(list, &head_sha)?,
        None => (Vec::new(), false),
    };
    Ok((
        canonical,
        url,
        PullRequestFacts {
            state,
            base_branch,
            head_repository,
            head_branch,
            head_sha,
            checks,
            checks_complete,
            review_decision,
        },
    ))
}

trait ClassifyName {
    fn classify_name(&self) -> &'static str;
}

impl ClassifyName for serde_json::Error {
    fn classify_name(&self) -> &'static str {
        match self.classify() {
            serde_json::error::Category::Io => "io",
            serde_json::error::Category::Syntax => "syntax",
            serde_json::error::Category::Data => "data",
            serde_json::error::Category::Eof => "truncated",
        }
    }
}

/// The pull request the URL names, which must be a github.com pull request with the
/// requested number.
fn canonical_key(url: &str, number: u64) -> Result<PullRequestKey, FetchError> {
    let key = locator::parse_locator(url)
        .map_err(|_| invalid("the url is not a github.com pull request"))?;
    if key.number != number {
        return Err(invalid("the url names another pull request"));
    }
    Ok(key)
}

fn branch(value: String, field: &str) -> Result<String, FetchError> {
    let value = value.trim().to_owned();
    if value.is_empty()
        || value.chars().count() > PULL_REQUEST_BRANCH_MAX_CHARS
        || value.chars().any(char::is_control)
    {
        return Err(invalid(&format!(
            "{field} is empty, too long or has control characters"
        )));
    }
    Ok(value)
}

fn commit_id(value: &str) -> Result<String, FetchError> {
    let value = value.trim().to_ascii_lowercase();
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("headRefOid is not a commit id"));
    }
    Ok(value)
}

/// `owner/name` of the head repository; `None` when the fork is gone.
fn head_repository(
    repository: Option<RawRepository>,
    owner: Option<RawOwner>,
) -> Result<Option<String>, FetchError> {
    let (Some(name), Some(login)) = (
        repository.and_then(|repository| repository.name),
        owner.and_then(|owner| owner.login),
    ) else {
        return Ok(None);
    };
    PullRequestKey::github(&format!("{login}/{name}"), 1)
        .map(|key| Some(key.repository))
        .map_err(|_| invalid("the head repository is not a GitHub owner/name"))
}

fn checks(raw: Vec<RawCheck>, head_sha: &str) -> Result<(Vec<PullRequestCheck>, bool), FetchError> {
    let mut checks = raw
        .into_iter()
        .map(|check| one_check(check, head_sha))
        .collect::<Result<Vec<_>, _>>()?;
    let complete = checks.len() <= PULL_REQUEST_CHECKS_MAX;
    if !complete {
        // Failing and running checks are the ones that matter, so they are kept first.
        checks.sort_by_key(severity);
        checks.truncate(PULL_REQUEST_CHECKS_MAX);
    }
    Ok((checks, complete))
}

fn severity(check: &PullRequestCheck) -> u8 {
    match (check.status, check.conclusion) {
        (
            CheckStatus::Completed,
            Some(
                CheckConclusion::Failure
                | CheckConclusion::TimedOut
                | CheckConclusion::StartupFailure
                | CheckConclusion::ActionRequired
                | CheckConclusion::Cancelled
                | CheckConclusion::Other,
            ),
        ) => 0,
        (CheckStatus::Completed, _) => 2,
        _ => 1,
    }
}

fn one_check(raw: RawCheck, head_sha: &str) -> Result<PullRequestCheck, FetchError> {
    match raw.typename.as_deref() {
        Some("CheckRun") => check_run(raw, head_sha),
        Some("StatusContext") => status_context(raw, head_sha),
        Some(other) => Err(invalid(&format!("unknown check type {other:?}"))),
        None => Err(invalid("a check has no type")),
    }
}

fn check_run(raw: RawCheck, head_sha: &str) -> Result<PullRequestCheck, FetchError> {
    let name = non_empty(raw.name, "a check run has no name")?;
    let display = match raw.workflow_name.as_deref().map(str::trim) {
        Some(workflow) if !workflow.is_empty() => format!("{workflow} / {name}"),
        _ => name.clone(),
    };
    let status = match required(raw.status, "a check run's status")?.as_str() {
        "COMPLETED" => CheckStatus::Completed,
        "IN_PROGRESS" => CheckStatus::InProgress,
        "QUEUED" | "PENDING" | "WAITING" | "REQUESTED" => CheckStatus::Queued,
        other => return Err(invalid(&format!("unknown check status {other:?}"))),
    };
    let conclusion_text = raw.conclusion.as_deref().unwrap_or_default();
    let conclusion = if status == CheckStatus::Completed {
        Some(match conclusion_text {
            "SUCCESS" => CheckConclusion::Success,
            "FAILURE" => CheckConclusion::Failure,
            "NEUTRAL" => CheckConclusion::Neutral,
            "CANCELLED" => CheckConclusion::Cancelled,
            "SKIPPED" => CheckConclusion::Skipped,
            "TIMED_OUT" => CheckConclusion::TimedOut,
            "ACTION_REQUIRED" => CheckConclusion::ActionRequired,
            "STALE" => CheckConclusion::Stale,
            "STARTUP_FAILURE" => CheckConclusion::StartupFailure,
            "" => return Err(invalid("a completed check run has no conclusion")),
            _ => CheckConclusion::Other,
        })
    } else if conclusion_text.is_empty() {
        None
    } else {
        return Err(invalid(
            "a check run that is not completed has a conclusion",
        ));
    };
    let provider_id =
        actions_id(raw.details_url.as_deref()).unwrap_or_else(|| format!("check_run:{display}"));
    build_check(
        provider_id,
        CheckKind::CheckRun,
        &display,
        status,
        conclusion,
        head_sha,
    )
}

fn status_context(raw: RawCheck, head_sha: &str) -> Result<PullRequestCheck, FetchError> {
    let context = non_empty(raw.context, "a status context has no name")?;
    let (status, conclusion) = match required(raw.state, "a status context's state")?.as_str() {
        "SUCCESS" => (CheckStatus::Completed, Some(CheckConclusion::Success)),
        "FAILURE" | "ERROR" => (CheckStatus::Completed, Some(CheckConclusion::Failure)),
        "PENDING" => (CheckStatus::InProgress, None),
        "EXPECTED" => (CheckStatus::Queued, None),
        other => return Err(invalid(&format!("unknown status state {other:?}"))),
    };
    build_check(
        format!("status:{context}"),
        CheckKind::StatusContext,
        &context,
        status,
        conclusion,
        head_sha,
    )
}

fn non_empty(value: Option<String>, problem: &str) -> Result<String, FetchError> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(problem))
}

/// The Actions job (or run) a check run belongs to, when its details URL is a github.com
/// Actions URL. Any other URL is the check's own business and is not trusted as an ID.
fn actions_id(details_url: Option<&str>) -> Option<String> {
    let rest = details_url?.strip_prefix("https://github.com/")?;
    let mut parts = rest.split(['/', '?', '#']);
    let (_owner, _repo, actions) = (parts.next()?, parts.next()?, parts.next()?);
    if actions != "actions" || parts.next()? != "runs" {
        return None;
    }
    let run = parts.next().filter(|run| is_number(run))?;
    match (parts.next(), parts.next()) {
        (Some("job"), Some(job)) if is_number(job) => Some(format!("job:{job}")),
        _ => Some(format!("run:{run}")),
    }
}

fn is_number(text: &str) -> bool {
    !text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit())
}

fn build_check(
    provider_id: String,
    kind: CheckKind,
    display: &str,
    status: CheckStatus,
    conclusion: Option<CheckConclusion>,
    head_sha: &str,
) -> Result<PullRequestCheck, FetchError> {
    let name = clean_text(display, PULL_REQUEST_CHECK_NAME_MAX_CHARS);
    let provider_id = clean_text(&provider_id, PULL_REQUEST_CHECK_ID_MAX_CHARS);
    if name.is_empty() || provider_id.is_empty() {
        return Err(invalid("a check has no usable name"));
    }
    Ok(PullRequestCheck {
        provider_id,
        kind,
        name,
        status,
        conclusion,
        head_sha: head_sha.to_owned(),
    })
}

#[cfg(test)]
mod tests;
