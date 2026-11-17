//! Pull requests namespace: the pull request a thread tracks, the policy and
//! the budget of the automatic fix notices, and what clients are told about
//! them (ADR 0045, ADR 0043, PR 15).
//!
//! ## Shape
//!
//! * The core owns the pull request. It links, fetches (the `gh` calls included),
//!   stores the facts, decides whether an automatic notice may be sent, spends
//!   the budget and claims the one notification of a limit stop. A client never
//!   runs `gh` and never writes the database: it asks, with the commands and
//!   queries here, and shows what the core reports. Two clients, a desktop and a
//!   browser, use the same operations.
//! * Every state-changing command carries a client-chosen
//!   [`OperationId`](crate::ids::OperationId) and is answered at once with the
//!   operation in state `pending` (the shared contract of [`crate::operation`]).
//!   The work runs in an operation of the core; the end arrives as
//!   `pull_request_operation_completed` and stays readable with
//!   `get_pull_request_operation`. The same id with the same request joins the
//!   operation instead of applying twice; the same id with another request is a
//!   `conflict`.
//! * **Revisions.** A link or unlink quotes the lifecycle revision the client
//!   read; a policy change quotes both the link revision and the policy revision.
//!   A client that read an older one is refused with `stale_revision` and nothing
//!   changes, so two clients cannot overwrite each other. The revisions are
//!   tokens to quote back, not numbers to compare.
//! * **A command that was answered with an unknown outcome** (the connection
//!   broke, or the core restarted) is never sent again under a new id: the
//!   client asks for the operation, and when the core no longer knows it, the
//!   outcome is unknown and the client reads the thread's pull request again.
//!   An operation that failed after its effect may have been written carries
//!   `outcome_unknown`.
//! * **Events say that something changed.** They name the thread and the
//!   pull request, never carry the facts; the client reads the thread again.
//!   The notifications a change gives reason for, and the one notification of
//!   an automatic fix limit, are claimed by the core exactly once for every
//!   client together (see [`PullRequestsEvent::AutoFixLimitReached`]).
use crate::{
    handshake::{Capability, WireLimits},
    ids::{OperationId, ThreadId},
    limits::{MAX_ID_BYTES, MAX_NAME_BYTES, MAX_TEXT_BYTES},
    operation::OperationView,
    validation::{ValidationError, check_count, check_line, check_text},
    wire::{Revision, Sequence, UnixMillis},
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &[
    "link_pull_request",
    "unlink_pull_request",
    "refresh_pull_request",
    "confirm_pull_request_candidate",
    "dismiss_pull_request_candidate",
    "set_auto_fix",
    "reset_auto_fix_budget",
    "change_auto_fix_destination",
    "request_manual_notice",
];
pub const QUERY_OPERATIONS: &[&str] = &[
    "get_thread_pull_request",
    "get_pull_request_notice",
    "get_pull_request_operation",
    "list_pull_request_operations",
];
pub const EVENT_TYPES: &[&str] = &[
    "pull_request_changed",
    "auto_fix_limit_reached",
    "pull_request_operation_completed",
];
pub const NESTED_COMMANDS: &[super::NestedOperation] = &[];

/// Most operations a core keeps and lists.
pub const MAX_OPERATIONS: usize = 64;
/// Most checks of one pull request a reply carries.
pub const MAX_CHECKS: usize = 200;
/// Most undecided candidates of one thread a reply carries.
pub const MAX_CANDIDATES: usize = 64;
/// Most notifications one change names.
pub const MAX_NOTIFICATIONS: usize = 16;
/// Longest text of an operation's message.
pub const MAX_MESSAGE_BYTES: usize = 1024;
/// Longest locator (`owner/repo#123` or a URL).
pub const MAX_LOCATOR_BYTES: usize = 512;
/// Longest commit hash.
pub const MAX_HEAD_BYTES: usize = 128;
/// Longest text of one label of the facts (a branch, a check name).
pub const MAX_LABEL_BYTES: usize = 1024;
/// The automatic fix notices one budget allows (the storage's limit).
pub const AUTO_FIX_LIMIT: u32 = 3;

// ---- views ----------------------------------------------------------------

/// Which pull request: the base repository `owner/name` and the number.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequestRef {
    /// `github` in Phase 1.
    pub provider: String,
    /// `github.com` in Phase 1.
    pub host: String,
    pub repository: String,
    #[serde(with = "crate::wire")]
    pub number: u64,
}

impl PullRequestRef {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line("provider", &self.provider, MAX_NAME_BYTES, false)?;
        check_line("host", &self.host, MAX_NAME_BYTES, false)?;
        check_line("repository", &self.repository, MAX_NAME_BYTES, false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkOrigin {
    /// A person linked it, or confirmed a candidate.
    Explicit,
    /// A derived thread took it from its parent.
    Inherited,
}

/// The thread's active link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkView {
    pub pull_request: PullRequestRef,
    /// The revision of this link; policy commands quote it.
    pub link_revision: Revision,
    pub origin: LinkOrigin,
    pub linked_at: UnixMillis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
    None,
}

/// One check of the pull request's head. `kind`, `status` and `conclusion` are
/// the snake_case labels of the facts (`check_run`, `completed`, `failure`, ...);
/// a client shows a label it does not know as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckView {
    pub name: String,
    pub kind: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactsView {
    pub state: PullRequestState,
    pub base_branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_repository: Option<String>,
    pub head_branch: String,
    pub head_sha: String,
    pub checks: Vec<CheckView>,
    /// Whether `checks` is the whole list.
    pub checks_complete: bool,
    pub review_decision: ReviewDecision,
}

/// Why the last fetch failed: the class (`unauthenticated`, `rate_limited`,
/// `network`, ...) and a bounded sentence without a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchFailureView {
    pub kind: String,
    pub message: String,
}

/// What the core knows of the pull request itself, shared by every thread that
/// links it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotView {
    /// The last successful fetch; absent before the first one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<FactsView>,
    /// The title, for display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<UnixMillis>,
    /// The failure of the last attempt, when it failed. The facts above are
    /// then the last success, never erased by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<FetchFailureView>,
    pub consecutive_failures: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateState {
    Presented,
    Dismissed,
    Confirmed,
}

/// A pull request a tool output suggested for the thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateView {
    #[serde(with = "crate::wire")]
    pub candidate_id: u64,
    pub pull_request: PullRequestRef,
    pub state: CandidateState,
}

/// Where the shown head came from, as far as the core knows. Only the process
/// that owns the thread knows; everything else reads `unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeadAttribution {
    Agent,
    Unknown,
}

/// The policy and the budget of the automatic fix notices of the thread's pull
/// request. Decided by the core alone: a client shows it, and changes it only
/// with the commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoFixView {
    /// This pull request's switch (per pull request, and opt-in).
    pub enabled: bool,
    /// The core's own setting; no automatic notice is sent while it is off.
    pub globally_enabled: bool,
    /// The thread the notices are sent to, when one was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<ThreadId>,
    /// The link revision the policy was made for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_revision: Option<Revision>,
    /// Quoted by the next policy command.
    pub policy_revision: Revision,
    /// How many notices the budget has spent, and how many are held.
    pub used: u32,
    pub reserved: u32,
    pub limit: u32,
    /// No automatic notice before this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_until: Option<UnixMillis>,
    /// The label of why automatic notices stopped (`limit_reached`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// The budget is spent and only a reset starts it again.
    pub limit_reached: bool,
}

/// Everything a client shows of a thread's pull request, in one read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadPullRequest {
    pub thread: ThreadId,
    /// Whether the core tracks pull requests at all (`[pull_requests] enabled`).
    pub tracking_enabled: bool,
    /// Whether this core holds the thread, and so can change its pull request.
    pub owned_here: bool,
    /// Quoted by `link_pull_request` and `unlink_pull_request`.
    pub lifecycle_revision: Revision,
    pub transition_seq: Sequence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<LinkView>,
    /// The commit the thread's checkout was last seen at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotView>,
    pub candidates: Vec<CandidateView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_fix: Option<AutoFixView>,
    pub head_attribution: HeadAttribution,
}

impl ThreadPullRequest {
    pub fn validate(&self) -> Result<(), ValidationError> {
        if let Some(link) = &self.link {
            link.pull_request.validate()?;
        }
        if let Some(snapshot) = &self.snapshot {
            if let Some(facts) = &snapshot.facts {
                check_count("checks", facts.checks.len(), MAX_CHECKS)?;
                check_line("base_branch", &facts.base_branch, MAX_LABEL_BYTES, true)?;
                check_line("head_branch", &facts.head_branch, MAX_LABEL_BYTES, true)?;
                check_line("head_sha", &facts.head_sha, MAX_HEAD_BYTES, true)?;
                for check in &facts.checks {
                    check_line("check", &check.name, MAX_LABEL_BYTES, true)?;
                }
            }
            if let Some(title) = &snapshot.title {
                check_line("title", title, MAX_LABEL_BYTES, true)?;
            }
            if let Some(failure) = &snapshot.last_failure {
                check_line("failure_kind", &failure.kind, MAX_NAME_BYTES, false)?;
                check_text("failure", &failure.message, MAX_MESSAGE_BYTES, true)?;
            }
        }
        check_count("candidates", self.candidates.len(), MAX_CANDIDATES)?;
        for candidate in &self.candidates {
            candidate.pull_request.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeMode {
    Manual,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticePhase {
    Reserved,
    Ready,
    Adopted,
    Cancelled,
}

/// A fix notice the core reserved (the budget of an automatic one is spent
/// when it is adopted).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoticeView {
    pub notice_id: String,
    pub pull_request: PullRequestRef,
    pub head_sha: String,
    pub mode: NoticeMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<ThreadId>,
    pub link_revision: Revision,
    pub policy_revision: Revision,
    pub phase: NoticePhase,
    /// The text sent to the conversation, once it is prepared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
}

impl NoticeView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_line("notice_id", &self.notice_id, MAX_ID_BYTES, false)?;
        self.pull_request.validate()?;
        check_line("head_sha", &self.head_sha, MAX_HEAD_BYTES, true)?;
        if let Some(payload) = &self.payload {
            check_text("payload", payload, MAX_TEXT_BYTES, true)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestOperationKind {
    Link,
    Unlink,
    Refresh,
    ConfirmCandidate,
    DismissCandidate,
    SetAutoFix,
    ResetAutoFixBudget,
    ChangeAutoFixDestination,
    RequestManualNotice,
}

/// An operation with what a client needs to explain its end. The `message` is a
/// bounded, secret-free sentence; the state itself carries only the failure
/// class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequestOperationView {
    pub operation: OperationView,
    pub kind: PullRequestOperationKind,
    pub thread: ThreadId,
    /// What a `Ready` link, unlink or confirmation left: the lifecycle revision
    /// to quote next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_revision: Option<Revision>,
    /// What a `Ready` policy command left: the policy revision to quote next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_revision: Option<Revision>,
    /// The notice a `Ready` manual notice request reserved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The operation failed after its effect may have been written: it is not a
    /// success, and not a refusal that changed nothing. Read the thread again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub outcome_unknown: bool,
}

impl PullRequestOperationView {
    pub fn validate(&self) -> Result<(), ValidationError> {
        self.operation.validate()?;
        if let Some(message) = &self.message {
            check_text("message", message, MAX_MESSAGE_BYTES, true)?;
        }
        if let Some(notice) = &self.notice_id {
            check_line("notice_id", notice, MAX_ID_BYTES, false)?;
        }
        Ok(())
    }
}

// ---- commands -------------------------------------------------------------

/// Link the thread to a pull request: a github.com URL or `owner/repo#123`.
/// The core fetches the pull request soon after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkPullRequest {
    pub operation_id: OperationId,
    pub thread: ThreadId,
    pub locator: String,
    /// The lifecycle revision the client read.
    pub expected_revision: Revision,
}

/// End the thread's link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnlinkPullRequest {
    pub operation_id: OperationId,
    pub thread: ThreadId,
    pub expected_revision: Revision,
}

/// Fetch the thread's pull request now. Asking again while a fetch runs joins
/// one follow-up fetch; it never starts a second one at the same time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshPullRequest {
    pub operation_id: OperationId,
    pub thread: ThreadId,
}

/// Link the thread to a presented candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmCandidate {
    pub operation_id: OperationId,
    pub thread: ThreadId,
    #[serde(with = "crate::wire")]
    pub candidate_id: u64,
    pub expected_revision: Revision,
}

/// Ignore a candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DismissCandidate {
    pub operation_id: OperationId,
    pub thread: ThreadId,
    #[serde(with = "crate::wire")]
    pub candidate_id: u64,
}

/// Switch the automatic fix notices of the thread's pull request on or off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetAutoFix {
    pub operation_id: OperationId,
    pub thread: ThreadId,
    pub link_revision: Revision,
    pub policy_revision: Revision,
    pub enabled: bool,
}

/// Start the budget again and switch the notices on. When the notices went to
/// another thread, `confirmed_previous` names that destination, which the core
/// compares with the stored one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResetAutoFixBudget {
    pub operation_id: OperationId,
    pub thread: ThreadId,
    pub link_revision: Revision,
    pub policy_revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_previous: Option<ThreadId>,
}

/// Move the notices to this thread. `confirmed_previous` is the destination the
/// person saw; a different stored one refuses the change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeAutoFixDestination {
    pub operation_id: OperationId,
    pub thread: ThreadId,
    pub link_revision: Revision,
    pub policy_revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_previous: Option<ThreadId>,
}

/// Reserve a fix notice for the current head now. It spends no budget and
/// gives no permission to resume a stopped conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestManualNotice {
    pub operation_id: OperationId,
    pub thread: ThreadId,
    pub link_revision: Revision,
    pub expected_head: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PullRequestsCommand {
    LinkPullRequest(LinkPullRequest),
    UnlinkPullRequest(UnlinkPullRequest),
    RefreshPullRequest(RefreshPullRequest),
    ConfirmPullRequestCandidate(ConfirmCandidate),
    DismissPullRequestCandidate(DismissCandidate),
    SetAutoFix(SetAutoFix),
    ResetAutoFixBudget(ResetAutoFixBudget),
    ChangeAutoFixDestination(ChangeAutoFixDestination),
    RequestManualNotice(RequestManualNotice),
}

impl PullRequestsCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Commands
    }

    /// The operation this command names.
    pub fn operation_id(&self) -> &OperationId {
        match self {
            Self::LinkPullRequest(c) => &c.operation_id,
            Self::UnlinkPullRequest(c) => &c.operation_id,
            Self::RefreshPullRequest(c) => &c.operation_id,
            Self::ConfirmPullRequestCandidate(c) => &c.operation_id,
            Self::DismissPullRequestCandidate(c) => &c.operation_id,
            Self::SetAutoFix(c) => &c.operation_id,
            Self::ResetAutoFixBudget(c) => &c.operation_id,
            Self::ChangeAutoFixDestination(c) => &c.operation_id,
            Self::RequestManualNotice(c) => &c.operation_id,
        }
    }

    /// The thread whose pull request this command changes.
    pub fn thread(&self) -> &ThreadId {
        match self {
            Self::LinkPullRequest(c) => &c.thread,
            Self::UnlinkPullRequest(c) => &c.thread,
            Self::RefreshPullRequest(c) => &c.thread,
            Self::ConfirmPullRequestCandidate(c) => &c.thread,
            Self::DismissPullRequestCandidate(c) => &c.thread,
            Self::SetAutoFix(c) => &c.thread,
            Self::ResetAutoFixBudget(c) => &c.thread,
            Self::ChangeAutoFixDestination(c) => &c.thread,
            Self::RequestManualNotice(c) => &c.thread,
        }
    }

    pub fn kind(&self) -> PullRequestOperationKind {
        match self {
            Self::LinkPullRequest(_) => PullRequestOperationKind::Link,
            Self::UnlinkPullRequest(_) => PullRequestOperationKind::Unlink,
            Self::RefreshPullRequest(_) => PullRequestOperationKind::Refresh,
            Self::ConfirmPullRequestCandidate(_) => PullRequestOperationKind::ConfirmCandidate,
            Self::DismissPullRequestCandidate(_) => PullRequestOperationKind::DismissCandidate,
            Self::SetAutoFix(_) => PullRequestOperationKind::SetAutoFix,
            Self::ResetAutoFixBudget(_) => PullRequestOperationKind::ResetAutoFixBudget,
            Self::ChangeAutoFixDestination(_) => PullRequestOperationKind::ChangeAutoFixDestination,
            Self::RequestManualNotice(_) => PullRequestOperationKind::RequestManualNotice,
        }
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::LinkPullRequest(c) => check_line("locator", &c.locator, MAX_LOCATOR_BYTES, false),
            Self::RequestManualNotice(c) => {
                check_line("expected_head", &c.expected_head, MAX_HEAD_BYTES, false)
            }
            Self::UnlinkPullRequest(_)
            | Self::RefreshPullRequest(_)
            | Self::ConfirmPullRequestCandidate(_)
            | Self::DismissPullRequestCandidate(_)
            | Self::SetAutoFix(_)
            | Self::ResetAutoFixBudget(_)
            | Self::ChangeAutoFixDestination(_) => Ok(()),
        }
    }
}

/// What a command answers: the operation it started, in the state it has now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PullRequestsResult {
    Operation(PullRequestOperationView),
}

// ---- queries --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PullRequestsQuery {
    /// The thread's link, the facts, the candidates and the auto fix policy.
    GetThreadPullRequest {
        thread: ThreadId,
    },
    GetPullRequestNotice {
        notice_id: String,
    },
    GetPullRequestOperation {
        operation_id: OperationId,
    },
    /// The retained operations, newest first.
    ListPullRequestOperations {},
}

impl PullRequestsQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Queries
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::GetPullRequestNotice { notice_id } => {
                check_line("notice_id", notice_id, MAX_ID_BYTES, false)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PullRequestsReply {
    Thread(Box<ThreadPullRequest>),
    Notice(Box<NoticeView>),
    Operation(PullRequestOperationView),
    Operations {
        operations: Vec<PullRequestOperationView>,
    },
}

impl PullRequestsReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Thread(thread) => thread.validate(),
            Self::Notice(notice) => notice.validate(),
            Self::Operation(operation) => operation.validate(),
            Self::Operations { operations } => {
                check_count("operations", operations.len(), MAX_OPERATIONS)?;
                operations
                    .iter()
                    .try_for_each(PullRequestOperationView::validate)
            }
        }
    }
}

// ---- events ---------------------------------------------------------------

/// What changed about a thread's pull request. The client reads the thread
/// again; the event decides nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestChange {
    Linked,
    Unlinked,
    CandidateChanged,
    /// A fetch found different facts.
    FactsChanged,
    /// Fetching started to fail, failed differently, or recovered.
    FetchStatusChanged,
    /// The policy, the budget, a notice or the delivery state changed.
    AutoFixChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PullRequestsEvent {
    /// Something about the thread's pull request changed; read the thread.
    PullRequestChanged {
        thread: ThreadId,
        pull_request: PullRequestRef,
        change: PullRequestChange,
        link_revision: Revision,
        transition_seq: Sequence,
        /// The kinds of notification this change gives reason to tell the
        /// person about (`merged`, `closed`, `checks_failed`, ...). The core
        /// claimed each once for the thread, the link revision and the
        /// transition: show them, do not decide again.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        notifications: Vec<String>,
    },
    /// The automatic fix stopped at its limit. The core claimed the one
    /// notification of this stop before sending the event, so a second client
    /// and a second process never tell the person again.
    AutoFixLimitReached {
        thread: ThreadId,
        pull_request: PullRequestRef,
        link_revision: Revision,
        #[serde(with = "crate::wire")]
        budget_epoch: u64,
        limit: u32,
    },
    /// An operation ended; also readable with `get_pull_request_operation`.
    PullRequestOperationCompleted { operation: PullRequestOperationView },
}

impl PullRequestsEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::PullRequestChanged {
                pull_request,
                notifications,
                ..
            } => {
                pull_request.validate()?;
                check_count("notifications", notifications.len(), MAX_NOTIFICATIONS)?;
                notifications
                    .iter()
                    .try_for_each(|kind| check_line("notification", kind, MAX_NAME_BYTES, false))
            }
            Self::AutoFixLimitReached { pull_request, .. } => pull_request.validate(),
            Self::PullRequestOperationCompleted { operation } => operation.validate(),
        }
    }
}
