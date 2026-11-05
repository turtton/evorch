//! Pull request integration storage (ADR 0043 Phase 1): the durable contract.
//!
//! A thread is linked to at most one GitHub pull request at a time. This module is the
//! public surface of what the harness keeps about that: the **links** (with their
//! history), the **candidates** a tool output suggested, the **inheritance** of a link by
//! a derived thread, the shared **snapshots** of the facts fetched about a pull request,
//! and the small per-thread **lifecycle state**.
//!
//! # Rules the storage enforces
//!
//! - **Compare-and-swap on the lifecycle revision.** Every explicit decision
//!   ([`StorageHandle::link_pull_request`], [`StorageHandle::unlink_pull_request`]) names
//!   the revision the caller read; a window that read an older one gets
//!   [`StorageError::StaleRevision`] and changes nothing, so a late unlink cannot erase a
//!   newer link. A relink ends the old row and creates the new one in one transaction; an
//!   unlink ends the row, advances the revision and cancels the thread's own pending
//!   inheritance in the same transaction.
//! - **Inheritance is decided once.** A derived thread's pending row is applied only while
//!   its recorded child revision still matches, no explicit decision exists and the pull
//!   request is *confirmed open* by a snapshot. A cancelled or skipped row never returns
//!   to pending, and a parent's unlink does not touch the child.
//! - **Snapshots are ordered by the sequence handed out when a fetch starts.** A result
//!   older than the stored one is refused; a failure never erases the last success; the
//!   attempt metadata is ordered the same way. Any number of processes can write.
//! - **Facts only.** No pull request body, comment or log text is stored. A check keeps a
//!   sanitized single-line name; the review decision is the structured enum.
//! - **Retention.** The thread-keyed tables are reclaimed with their thread, writes for a
//!   deleted thread are refused ([`StorageError::Tombstoned`]) and the facts of a key no
//!   thread references any more are cleared.
//!
//! Nothing here fetches, polls or notifies: that is the runtime's. Each write returns the
//! new state, and the runtime publishes events only after the call returned `Ok`.

use serde::{Deserialize, Serialize};

use crate::StorageError;

/// Most checks one snapshot keeps. A larger list must be cut by the caller, which then
/// reports it as incomplete.
pub const PULL_REQUEST_CHECKS_MAX: usize = 200;
/// Longest check name, in characters.
pub const PULL_REQUEST_CHECK_NAME_MAX_CHARS: usize = 160;
/// Longest provider-side check identifier, in characters.
pub const PULL_REQUEST_CHECK_ID_MAX_CHARS: usize = 128;
/// Longest branch name, in characters.
pub const PULL_REQUEST_BRANCH_MAX_CHARS: usize = 255;
/// Longest stored fetch error message (or inheritance attempt note), in characters.
pub const PULL_REQUEST_ERROR_MAX_CHARS: usize = 256;
/// Candidates one thread keeps, decided ones included; a full thread takes no new one.
pub const PULL_REQUEST_CANDIDATES_MAX: usize = 64;
/// Notification keys one thread remembers exactly (older ones are summarized by a floor).
pub const PULL_REQUEST_NOTIFIED_MAX: usize = 64;
/// Longest notification kind, in bytes.
pub const PULL_REQUEST_NOTIFICATION_KIND_MAX_BYTES: usize = 48;
/// Most rows one listing call returns.
pub const PULL_REQUEST_LIST_MAX: usize = 200;
/// Longest thread, run or call identifier, in bytes.
pub const PULL_REQUEST_ID_MAX_BYTES: usize = 256;
/// The only host Phase 1 knows.
pub const PULL_REQUEST_GITHUB_HOST: &str = "github.com";

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// The stored label.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// The value stored under `label`, if any.
            #[must_use]
            pub fn parse(label: &str) -> Option<Self> {
                match label {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

string_enum! {
    /// Where a pull request lives. Phase 1 knows GitHub only.
    PullRequestProvider {
        /// GitHub (github.com).
        Github => "github",
    }
}

string_enum! {
    /// How a link came to be.
    LinkOrigin {
        /// A person linked it (or confirmed a candidate).
        Explicit => "explicit",
        /// A derived thread took it from its parent.
        Inherited => "inherited",
    }
}

string_enum! {
    /// Why a link ended.
    LinkEndReason {
        /// Another link replaced it.
        Relinked => "relinked",
        /// A person unlinked it.
        Unlinked => "unlinked",
    }
}

string_enum! {
    /// State of a candidate.
    CandidateState {
        /// Shown to the person, not decided yet.
        Presented => "presented",
        /// The person ignored it.
        Dismissed => "dismissed",
        /// The person linked it.
        Confirmed => "confirmed",
    }
}

string_enum! {
    /// State of a pull request as GitHub reports it.
    PullRequestState {
        /// Open.
        Open => "open",
        /// Closed without merging.
        Closed => "closed",
        /// Merged.
        Merged => "merged",
    }
}

string_enum! {
    /// GitHub's structured review decision. Counts and review text are never kept.
    ReviewDecision {
        /// Approved.
        Approved => "approved",
        /// Changes were requested.
        ChangesRequested => "changes_requested",
        /// A review is still required.
        ReviewRequired => "review_required",
        /// GitHub reports no decision (no review rule applies).
        None => "none",
    }
}

string_enum! {
    /// What kind of check a row summarizes.
    CheckKind {
        /// A check run (GitHub Actions or an app).
        CheckRun => "check_run",
        /// A commit status context.
        StatusContext => "status_context",
    }
}

string_enum! {
    /// Progress of a check.
    CheckStatus {
        /// Waiting to start.
        Queued => "queued",
        /// Running.
        InProgress => "in_progress",
        /// Finished; the conclusion says how.
        Completed => "completed",
    }
}

string_enum! {
    /// How a finished check ended.
    CheckConclusion {
        /// Passed.
        Success => "success",
        /// Failed.
        Failure => "failure",
        /// Neutral.
        Neutral => "neutral",
        /// Cancelled.
        Cancelled => "cancelled",
        /// Skipped.
        Skipped => "skipped",
        /// Timed out.
        TimedOut => "timed_out",
        /// Needs a person.
        ActionRequired => "action_required",
        /// Stale.
        Stale => "stale",
        /// Could not start.
        StartupFailure => "startup_failure",
        /// Any other result the provider reports.
        Other => "other",
    }
}

string_enum! {
    /// Why a fetch failed (the class, never the response).
    FetchErrorKind {
        /// `gh` is not signed in or lacks a scope.
        Unauthenticated => "unauthenticated",
        /// The pull request or repository is not visible.
        NotFound => "not_found",
        /// The API rate limit was hit.
        RateLimited => "rate_limited",
        /// The network failed.
        Network => "network",
        /// The fetch deadline passed.
        Timeout => "timeout",
        /// The response could not be understood.
        InvalidResponse => "invalid_response",
        /// Anything else.
        Other => "other",
    }
}

string_enum! {
    /// State of a thread's inheritance row.
    InheritanceState {
        /// Waiting for the pull request to be confirmed open.
        Pending => "pending",
        /// The child took the link.
        Applied => "applied",
        /// Decided not to inherit.
        Skipped => "skipped",
        /// The child made its own decision first.
        Cancelled => "cancelled",
    }
}

string_enum! {
    /// Why an inheritance row was skipped or cancelled.
    InheritanceReason {
        /// The child linked a pull request itself.
        ExplicitLink => "explicit_link",
        /// The child unlinked (it decided about pull requests itself).
        ExplicitUnlink => "explicit_unlink",
        /// The child's lifecycle revision moved on.
        ChildChanged => "child_changed",
        /// The child already has an active link.
        ChildLinked => "child_linked",
        /// The pull request is merged.
        Merged => "merged",
        /// The pull request is closed.
        Closed => "closed",
        /// A lane thread never inherits.
        LaneThread => "lane_thread",
        /// Any other reason the runtime decided.
        Other => "other",
    }
}

/// Why the runtime may skip an inheritance row by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InheritanceSkipReason {
    /// The pull request is merged.
    Merged,
    /// The pull request is closed.
    Closed,
    /// The derived thread is a lane.
    LaneThread,
    /// Any other reason.
    Other,
}

impl InheritanceSkipReason {
    pub(crate) fn reason(self) -> InheritanceReason {
        match self {
            Self::Merged => InheritanceReason::Merged,
            Self::Closed => InheritanceReason::Closed,
            Self::LaneThread => InheritanceReason::LaneThread,
            Self::Other => InheritanceReason::Other,
        }
    }
}

/// Why a pull request storage call was refused (in addition to
/// [`StorageError::StaleRevision`], [`StorageError::Tombstoned`] and SQLite failures).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullRequestRejection {
    /// A field is empty, oversized or malformed. Never silently cut.
    Invalid {
        /// The offending field.
        field: &'static str,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The thread has no active link.
    NotLinked,
    /// The link the call is bound to is not the active link any more.
    LinkNotActive,
    /// The candidate does not exist for this thread.
    CandidateNotFound,
    /// The thread already holds [`PULL_REQUEST_CANDIDATES_MAX`] candidates.
    CandidateLimit,
    /// The parent link does not exist.
    ParentLinkNotFound,
    /// The parent link already ended, so there is nothing to inherit.
    ParentLinkNotActive,
    /// A lane thread never inherits a link.
    LaneThread,
    /// The derived thread is the parent thread.
    SameThread,
}

impl std::fmt::Display for PullRequestRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid { field, reason } => {
                write!(formatter, "invalid pull request {field}: {reason}")
            }
            Self::NotLinked => write!(formatter, "the thread has no active pull request link"),
            Self::LinkNotActive => write!(formatter, "the pull request link is not active"),
            Self::CandidateNotFound => write!(formatter, "pull request candidate not found"),
            Self::CandidateLimit => write!(formatter, "too many undecided pull request candidates"),
            Self::ParentLinkNotFound => {
                write!(formatter, "the parent pull request link is unknown")
            }
            Self::ParentLinkNotActive => write!(formatter, "the parent pull request link ended"),
            Self::LaneThread => write!(formatter, "a lane thread does not inherit a link"),
            Self::SameThread => write!(formatter, "a thread cannot inherit from itself"),
        }
    }
}

pub(crate) fn invalid(field: &'static str, reason: &'static str) -> StorageError {
    StorageError::PullRequest(PullRequestRejection::Invalid { field, reason })
}

/// Replaces control characters (newlines included) by spaces, collapses runs of white
/// space, trims, and keeps at most `max_chars` characters. Idempotent. Third-party text
/// (check names, provider IDs, error excerpts) passes through this before it is stored.
#[must_use]
pub fn sanitize_single_line(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_control() || ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            if out.chars().count() + 1 >= max_chars {
                break;
            }
            out.push(' ');
            pending_space = false;
        }
        if out.chars().count() >= max_chars {
            break;
        }
        out.push(ch);
    }
    out
}

/// The identity of a pull request: `(provider, host, repository, number)`, where
/// `repository` is the **base** repository as `owner/name`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PullRequestKey {
    /// Where the pull request lives.
    pub provider: PullRequestProvider,
    /// The host; `github.com` in Phase 1.
    pub host: String,
    /// The base repository, `owner/name`, lowercase.
    pub repository: String,
    /// The pull request number.
    pub number: u64,
}

impl PullRequestKey {
    /// A GitHub (github.com) pull request of the base repository `owner/name`.
    ///
    /// # Errors
    /// [`PullRequestRejection::Invalid`] for a malformed repository or number.
    pub fn github(repository: &str, number: u64) -> Result<Self, StorageError> {
        Self {
            provider: PullRequestProvider::Github,
            host: PULL_REQUEST_GITHUB_HOST.to_owned(),
            repository: repository.to_owned(),
            number,
        }
        .normalized()
    }

    /// The key with its host and repository in canonical (lowercase) form, validated.
    ///
    /// # Errors
    /// [`PullRequestRejection::Invalid`] when the host is not `github.com`, the
    /// repository is not `owner/name` or the number is zero or above `u32::MAX`.
    pub fn normalized(&self) -> Result<Self, StorageError> {
        let host = self.host.trim().to_ascii_lowercase();
        if host != PULL_REQUEST_GITHUB_HOST {
            return Err(invalid("host", "only github.com is supported"));
        }
        let repository = normalize_repository("repository", &self.repository)?;
        if self.number == 0 || self.number > u64::from(u32::MAX) {
            return Err(invalid("number", "must be in 1..=4294967295"));
        }
        Ok(Self {
            provider: self.provider,
            host,
            repository,
            number: self.number,
        })
    }
}

/// Validates `owner/name` and returns it lowercase.
pub(crate) fn normalize_repository(
    field: &'static str,
    repository: &str,
) -> Result<String, StorageError> {
    let repository = repository.trim().to_ascii_lowercase();
    let Some((owner, name)) = repository.split_once('/') else {
        return Err(invalid(field, "must be owner/name"));
    };
    let owner_ok = !owner.is_empty()
        && owner.len() <= 39
        && owner
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let name_ok = !name.is_empty()
        && name.len() <= 100
        && name != "."
        && name != ".."
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        });
    if !owner_ok || !name_ok {
        return Err(invalid(
            field,
            "must be owner/name with GitHub's characters",
        ));
    }
    Ok(repository)
}

/// One link of a thread to a pull request (an active one, or history).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestLink {
    /// Identifier of the link row.
    pub link_id: u64,
    /// The linked thread.
    pub thread_id: String,
    /// The pull request.
    pub key: PullRequestKey,
    /// How the link came to be.
    pub origin: LinkOrigin,
    /// For an inherited link, the parent thread's link it was taken from.
    pub parent_link_id: Option<u64>,
    /// When the link was made.
    pub created_at_ns: i64,
    /// The lifecycle revision that created the link.
    pub link_revision: u64,
    /// How the link ended; `None` while it is active.
    pub end: Option<LinkEnd>,
}

/// How a link ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkEnd {
    /// When.
    pub at_ns: i64,
    /// Why.
    pub reason: LinkEndReason,
    /// The lifecycle revision that ended it.
    pub revision: u64,
}

/// An explicit link decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkPullRequest {
    /// The thread to link.
    pub thread_id: String,
    /// The pull request.
    pub key: PullRequestKey,
    /// The lifecycle revision the caller read (0 for a thread that never had a decision).
    pub expected_revision: u64,
    /// When the decision was made.
    pub at_ns: i64,
}

/// What [`StorageHandle::link_pull_request`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkReceipt {
    /// The active link after the call.
    pub link: PullRequestLink,
    /// The lifecycle revision after the call.
    pub lifecycle_revision: u64,
    /// `false` when the thread was already explicitly linked to this pull request and
    /// nothing changed (the revision did not move either).
    pub changed: bool,
    /// The link this call ended, if any.
    pub superseded: Option<PullRequestLink>,
    /// Whether the thread's own pending inheritance was cancelled.
    pub cancelled_inheritance: bool,
}

/// What [`StorageHandle::unlink_pull_request`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlinkReceipt {
    /// The link that ended. `None` when the thread had no active link and the unlink only
    /// declined its pending inheritance.
    pub ended: Option<PullRequestLink>,
    /// The lifecycle revision after the call.
    pub lifecycle_revision: u64,
    /// Whether the thread's own pending inheritance was cancelled.
    pub cancelled_inheritance: bool,
}

/// The thread's last observation of the local checkout HEAD, bound to the link revision
/// it was made for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalHeadObservation {
    /// The link revision the observation belongs to.
    pub link_revision: u64,
    /// The observed commit (lowercase hex).
    pub head_sha: String,
    /// When it was observed.
    pub observed_at_ns: i64,
}

/// Everything about a thread's pull request lifecycle, read together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPullRequestState {
    /// The thread.
    pub thread_id: String,
    /// Counts link and unlink decisions; 0 for a thread that never had one. Pass it back
    /// as `expected_revision`.
    pub lifecycle_revision: u64,
    /// The last transition sequence handed out.
    pub transition_seq: u64,
    /// The active link, if any.
    pub active_link: Option<PullRequestLink>,
    /// The local HEAD observation, only while it belongs to the active link.
    pub local_head: Option<LocalHeadObservation>,
    /// The thread's inheritance row (as a child), if any.
    pub inheritance: Option<InheritanceRecord>,
}

/// The identity of a notification, so it is sent once.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NotificationKey {
    /// The link revision the notification is about.
    pub link_revision: u64,
    /// The transition sequence it is about.
    pub transition_seq: u64,
    /// The notification kind (`checks_failed`, `merged`, …): lowercase letters, digits
    /// and `_`.
    pub kind: String,
}

/// A pull request a tool output suggested for a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestCandidate {
    /// Identifier of the candidate row.
    pub candidate_id: u64,
    /// The thread it was suggested for.
    pub thread_id: String,
    /// The pull request.
    pub key: PullRequestKey,
    /// The run whose tool call produced it.
    pub source_run_id: String,
    /// The tool call.
    pub source_call_id: String,
    /// Where the person left it.
    pub state: CandidateState,
    /// When it was first seen.
    pub created_at_ns: i64,
    /// When it last changed.
    pub updated_at_ns: i64,
}

/// A candidate to present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPullRequestCandidate {
    /// The thread.
    pub thread_id: String,
    /// The parsed pull request.
    pub key: PullRequestKey,
    /// The run whose tool call produced it.
    pub source_run_id: String,
    /// The tool call.
    pub source_call_id: String,
    /// When it was seen.
    pub at_ns: i64,
}

/// What presenting a candidate did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidatePresented {
    /// The candidate (an existing one keeps its state and original source).
    pub candidate: PullRequestCandidate,
    /// Whether this call created it.
    pub created: bool,
}

/// One check of a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestCheck {
    /// The provider's identifier (a run ID, or a status context name).
    pub provider_id: String,
    /// What kind of check it is.
    pub kind: CheckKind,
    /// The name, single-line and bounded ([`sanitize_single_line`]).
    pub name: String,
    /// Progress.
    pub status: CheckStatus,
    /// How it ended; present exactly when the status is `completed`.
    pub conclusion: Option<CheckConclusion>,
    /// The commit the result belongs to (lowercase hex).
    pub head_sha: String,
}

/// The facts of one successful fetch. Structured fields only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestFacts {
    /// Open, closed or merged.
    pub state: PullRequestState,
    /// The base branch.
    pub base_branch: String,
    /// The head repository (`owner/name`); `None` when the fork is gone.
    pub head_repository: Option<String>,
    /// The head branch.
    pub head_branch: String,
    /// The head commit (lowercase hex).
    pub head_sha: String,
    /// The checks, at most [`PULL_REQUEST_CHECKS_MAX`].
    pub checks: Vec<PullRequestCheck>,
    /// Whether `checks` is the whole list. `false` when it was cut or paged.
    pub checks_complete: bool,
    /// The structured review decision.
    pub review_decision: ReviewDecision,
}

/// A failed fetch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchFailure {
    /// The class of the failure.
    pub kind: FetchErrorKind,
    /// A short single-line description (sanitized, redacted, bounded).
    pub message: String,
}

/// The newest recorded outcome of a fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchAttempt {
    /// Its sequence.
    pub seq: u64,
    /// When it ended.
    pub at_ns: i64,
    /// `None` for a success.
    pub failure: Option<FetchFailure>,
}

/// What the harness knows about a pull request, shared by every thread that tracks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestSnapshot {
    /// The pull request.
    pub key: PullRequestKey,
    /// The facts of the newest successful fetch; `None` before the first one.
    pub facts: Option<PullRequestFacts>,
    /// When that fetch ended.
    pub fetched_at_ns: Option<i64>,
    /// The sequence of that fetch.
    pub success_seq: u64,
    /// The last sequence handed out.
    pub fetch_seq: u64,
    /// The newest recorded outcome (success or failure).
    pub last_attempt: Option<FetchAttempt>,
    /// Failures since the last success that was the newest attempt.
    pub consecutive_failures: u64,
}

/// What recording a fetch result did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchCommit {
    /// The facts were stored (the sequence was newer than the stored success).
    pub facts_stored: bool,
    /// The attempt metadata was stored (the sequence was newer than the last attempt).
    pub attempt_recorded: bool,
}

/// A derived thread's (pending) right to take its parent's link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritanceRecord {
    /// The derived thread.
    pub child_thread_id: String,
    /// The thread it derives from.
    pub parent_thread_id: String,
    /// The parent link it was pinned to.
    pub parent_link_id: u64,
    /// The pinned pull request.
    pub key: PullRequestKey,
    /// The child lifecycle revision the right is valid for.
    pub expected_child_revision: u64,
    /// Where it stands.
    pub state: InheritanceState,
    /// Why it was skipped or cancelled.
    pub decision_reason: Option<InheritanceReason>,
    /// The child link it created.
    pub applied_link_id: Option<u64>,
    /// How many times a resolution was tried.
    pub attempt_count: u64,
    /// When it was last tried.
    pub last_attempt_at_ns: Option<i64>,
    /// A short note on the last attempt.
    pub last_attempt_note: Option<String>,
    /// When it was scheduled.
    pub created_at_ns: i64,
    /// When it was decided.
    pub decided_at_ns: Option<i64>,
}

impl InheritanceRecord {
    /// The position of this row in the pending listing.
    #[must_use]
    pub fn cursor(&self) -> InheritanceCursor {
        InheritanceCursor {
            created_at_ns: self.created_at_ns,
            child_thread_id: self.child_thread_id.clone(),
        }
    }
}

/// Where a page of pending inheritance rows ended; pass it back to get the next page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritanceCursor {
    /// `created_at_ns` of the last row of the previous page.
    pub created_at_ns: i64,
    /// `child_thread_id` of the last row of the previous page.
    pub child_thread_id: String,
}

/// A request to schedule an inheritance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPullRequestInheritance {
    /// The derived thread (escalation, fork, rewind; never a lane).
    pub child_thread_id: String,
    /// The active link of the parent thread to pin.
    pub parent_link_id: u64,
    /// When it was scheduled.
    pub at_ns: i64,
}

/// Why a pending inheritance is still pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InheritancePending {
    /// No snapshot facts exist for the pull request yet.
    NoFacts,
    /// The stored success is older than `required_success_seq`.
    NeedsFresherSnapshot,
}

/// The outcome of [`StorageHandle::resolve_pull_request_inheritance`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InheritanceResolution {
    /// The child took the link.
    Applied {
        /// The child's new link.
        link: PullRequestLink,
        /// The child's lifecycle revision after the call.
        lifecycle_revision: u64,
    },
    /// Not decided; try again after a fetch. Nothing changed except the attempt count.
    Pending(InheritancePending),
    /// Decided now: the pull request is not open, or the child is a lane or already linked.
    Skipped(InheritanceReason),
    /// Decided now: the child changed on its own.
    Cancelled(InheritanceReason),
    /// Already decided earlier; nothing changed.
    AlreadyDecided(InheritanceState),
    /// No inheritance was scheduled for the thread.
    Missing,
}

/// Read-only access for a display that follows the shared database (ADR 0043 GUI). The
/// writes and the tracking itself stay with the single writer; these only read what it
/// committed, on the caller's own connection.
impl crate::Database {
    /// The lifecycle of a thread's pull request in one read.
    ///
    /// # Errors
    /// SQLite failure or an invalid thread ID.
    pub fn pull_request_thread_state(
        &self,
        thread_id: &str,
    ) -> Result<ThreadPullRequestState, StorageError> {
        crate::repo::pull_requests::thread_state(&self.conn, thread_id)
    }

    /// The shared snapshot of a pull request, if any fetch was ever started for it.
    ///
    /// # Errors
    /// SQLite failure or an invalid key.
    pub fn pull_request_snapshot(
        &self,
        key: &PullRequestKey,
    ) -> Result<Option<PullRequestSnapshot>, StorageError> {
        crate::repo::pull_requests::snapshots::get(&self.conn, key)
    }

    /// The thread's candidates, oldest first, optionally of one state.
    ///
    /// # Errors
    /// SQLite failure or an invalid thread ID.
    pub fn pull_request_candidates(
        &self,
        thread_id: &str,
        state: Option<CandidateState>,
    ) -> Result<Vec<PullRequestCandidate>, StorageError> {
        crate::repo::pull_requests::candidates(&self.conn, thread_id, state)
    }

    /// Active links of live, unarchived threads in link ID order after `after_link_id`.
    ///
    /// # Errors
    /// SQLite failure.
    pub fn active_pull_request_links(
        &self,
        after_link_id: u64,
        limit: usize,
    ) -> Result<Vec<PullRequestLink>, StorageError> {
        crate::repo::pull_requests::active_links(&self.conn, after_link_id, limit)
    }
}
