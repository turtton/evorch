//! What the pull request surfaces show (ADR 0043 Phase 1): the egui-free view of a thread's
//! linked pull request, read from the database, and the rules that turn it into words and
//! glyphs.
//!
//! Nothing here fetches or writes. The facts are the stored snapshot of the pull request, the
//! link and the local HEAD the owner recorded; the model never trusts an event's payload.
//! A pull request whose state cannot be established (no facts, a failed fetch, no checks, a
//! partial list of checks) is never shown as passing.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use runtime::pull_requests::diff::{ChecksVerdict, checks_verdict};
use storage::pull_requests::{
    CandidateState, CheckConclusion, CheckStatus, FetchFailure, LocalHeadObservation,
    PullRequestCandidate, PullRequestDisplay, PullRequestFacts, PullRequestKey, PullRequestLink,
    PullRequestState,
};

/// Facts older than this, in a window that tracks the thread, read as stale: the poll
/// interval is at most five minutes, so a snapshot this old means polling is not working.
pub const STALE_AFTER_SECONDS: i64 = 15 * 60;

const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// Everything shown about one thread's pull request, as one read of the database found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPullRequest {
    pub thread_id: String,
    /// Pass back as the expected revision of Link, Unlink and confirm.
    pub lifecycle_revision: u64,
    pub transition_seq: u64,
    /// The active link; `None` when nothing is linked (the bar and the badge are absent).
    pub link: Option<PullRequestLink>,
    pub local_head: Option<LocalHeadObservation>,
    /// The facts of the newest successful fetch of the linked pull request.
    pub facts: Option<PullRequestFacts>,
    /// What the same fetch gave for the display only (the cleaned title). Drawn as plain text.
    pub display: PullRequestDisplay,
    pub fetched_at_ns: Option<i64>,
    /// Set while the newest fetch attempt failed.
    pub failure: Option<FetchFailure>,
    /// Candidates the thread's tool outputs suggested, oldest first (decided ones included, so
    /// a confirmed one can be shown as linked).
    pub candidates: Vec<PullRequestCandidate>,
    /// Whether this process holds the thread's lease and so polls its pull request.
    pub tracked_here: bool,
}

impl ThreadPullRequest {
    #[must_use]
    pub fn key(&self) -> Option<&PullRequestKey> {
        self.link.as_ref().map(|link| &link.key)
    }

    #[must_use]
    pub fn number(&self) -> Option<u64> {
        self.key().map(|key| key.number)
    }

    /// `owner/repo#number`, the label of a pull request everywhere in the interface.
    #[must_use]
    pub fn label(&self) -> Option<String> {
        self.key().map(key_label)
    }
}

/// The wall clock, nanoseconds since the Unix epoch.
#[must_use]
pub fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
        .unwrap_or(0)
}

/// `owner/repo#number` of a key.
#[must_use]
pub fn key_label(key: &PullRequestKey) -> String {
    format!("{}#{}", key.repository, key.number)
}

/// The glyph class of a pull request. Only [`PrGlyph::Passing`] means success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrGlyph {
    Passing,
    Failing,
    Pending,
    Merged,
    Closed,
    /// No facts, a failed fetch, no checks, a partial list or checks that neither passed
    /// nor failed.
    Unknown,
}

impl PrGlyph {
    /// The word that accompanies the glyph; colour is never the only signal.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Passing => "passing",
            Self::Failing => "failing",
            Self::Pending => "running",
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Unknown => "unknown",
        }
    }
}

/// The glyph class of the thread's pull request; `None` without a link.
#[must_use]
pub fn glyph(view: &ThreadPullRequest) -> Option<PrGlyph> {
    view.link.as_ref()?;
    let Some(facts) = &view.facts else {
        return Some(PrGlyph::Unknown);
    };
    Some(match facts.state {
        PullRequestState::Merged => PrGlyph::Merged,
        PullRequestState::Closed => PrGlyph::Closed,
        PullRequestState::Open => {
            if view.failure.is_some() {
                // The newest answer is a failure: what was stored may be old.
                PrGlyph::Unknown
            } else {
                match checks_verdict(facts) {
                    ChecksVerdict::Passing => PrGlyph::Passing,
                    ChecksVerdict::Failing => PrGlyph::Failing,
                    ChecksVerdict::Pending => PrGlyph::Pending,
                    ChecksVerdict::Unsettled | ChecksVerdict::Unknown => PrGlyph::Unknown,
                }
            }
        }
    })
}

/// Whether the open pull request has a failed check (the condition that expands the bar).
#[must_use]
pub fn is_failing(view: &ThreadPullRequest) -> bool {
    glyph(view) == Some(PrGlyph::Failing)
}

/// The one-line CI summary: `CI passing 5/5`, `CI failing 2/5`, `CI running 3/5 done`, …
#[must_use]
pub fn ci_summary(view: &ThreadPullRequest) -> String {
    let Some(facts) = &view.facts else {
        return if view.failure.is_some() {
            "Not fetched".to_owned()
        } else {
            "Waiting for the first fetch".to_owned()
        };
    };
    if facts.state != PullRequestState::Open {
        return String::new();
    }
    let total = facts.checks.len();
    let failed = facts
        .checks
        .iter()
        .filter(|check| check.status == CheckStatus::Completed && failed(check.conclusion))
        .count();
    let finished = facts
        .checks
        .iter()
        .filter(|check| check.status == CheckStatus::Completed)
        .count();
    if view.failure.is_some() && failed == 0 {
        return "CI unknown".to_owned();
    }
    match checks_verdict(facts) {
        ChecksVerdict::Failing => format!("CI failing {failed}/{total}"),
        ChecksVerdict::Pending => format!("CI running {finished}/{total} done"),
        ChecksVerdict::Passing => format!("CI passing {total}/{total}"),
        ChecksVerdict::Unsettled => format!("CI unsettled {finished}/{total}"),
        ChecksVerdict::Unknown if total == 0 => "No checks".to_owned(),
        ChecksVerdict::Unknown => format!("CI incomplete {total} checks"),
    }
}

fn failed(conclusion: Option<CheckConclusion>) -> bool {
    matches!(
        conclusion,
        Some(
            CheckConclusion::Failure | CheckConclusion::TimedOut | CheckConclusion::StartupFailure
        )
    )
}

/// How a check reads in the expanded list.
#[must_use]
pub fn check_is_failed(check: &storage::pull_requests::PullRequestCheck) -> bool {
    check.status == CheckStatus::Completed && failed(check.conclusion)
}

/// The words of a pull request's state.
#[must_use]
pub const fn state_word(state: PullRequestState) -> &'static str {
    match state {
        PullRequestState::Open => "Open",
        PullRequestState::Closed => "Closed",
        PullRequestState::Merged => "Merged",
    }
}

/// A warning the linked pull request and the local checkout disagree. Computed from the stored
/// facts and the local HEAD the owner recorded; a warning never blocks anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mismatch {
    /// No single local checkout is on the pull request's head branch.
    NoLocalCheckout { branch: String },
    /// The local checkout's HEAD is not the pull request's head commit.
    LocalHeadDiffers { local: String, remote: String },
}

impl Mismatch {
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::NoLocalCheckout { branch } => {
                format!("No local checkout on branch {branch}")
            }
            Self::LocalHeadDiffers { local, remote } => format!(
                "Local HEAD {} differs from the pull request head {}",
                short_sha(local),
                short_sha(remote)
            ),
        }
    }
}

/// The first seven characters of a commit.
#[must_use]
pub fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// The disagreements between the pull request and the local checkout. Only for an open pull
/// request whose newest fetch worked: after a merge the branch is expected to move on.
#[must_use]
pub fn mismatches(view: &ThreadPullRequest) -> Vec<Mismatch> {
    let Some(facts) = &view.facts else {
        return Vec::new();
    };
    if facts.state != PullRequestState::Open || view.failure.is_some() {
        return Vec::new();
    }
    match &view.local_head {
        None => vec![Mismatch::NoLocalCheckout {
            branch: facts.head_branch.clone(),
        }],
        Some(head) if head.head_sha != facts.head_sha => vec![Mismatch::LocalHeadDiffers {
            local: head.head_sha.clone(),
            remote: facts.head_sha.clone(),
        }],
        Some(_) => Vec::new(),
    }
}

/// One thing a person should know about how far the shown facts can be trusted. The three
/// kinds are independent: each is shown whenever it holds, whatever the others say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// This window does not poll the thread: another window owns it, or nobody does (after a
    /// restart). What is shown is the last stored snapshot.
    NotTracked,
    /// Nothing newer than [`STALE_AFTER_SECONDS`] arrived.
    Stale,
    /// The newest fetch failed; the facts shown are the last ones that arrived.
    FetchFailed {
        /// The class of the failure (`unauthenticated`, `network`, …).
        kind: String,
        /// The stored, sanitized message.
        message: String,
    },
}

/// Whether the last successful fetch is older than [`STALE_AFTER_SECONDS`], tracked here or not.
#[must_use]
pub fn is_stale(view: &ThreadPullRequest, now_ns: i64) -> bool {
    view.fetched_at_ns
        .is_some_and(|at| now_ns.saturating_sub(at) > STALE_AFTER_SECONDS * NANOS_PER_SECOND)
}

/// Every status that holds for the view, in the order they are shown.
#[must_use]
pub fn statuses(view: &ThreadPullRequest, now_ns: i64) -> Vec<Status> {
    let mut found = Vec::new();
    if !view.tracked_here {
        found.push(Status::NotTracked);
    }
    if is_stale(view, now_ns) {
        found.push(Status::Stale);
    }
    if let Some(failure) = &view.failure {
        found.push(Status::FetchFailed {
            kind: failure.kind.as_str().to_owned(),
            message: failure.message.clone(),
        });
    }
    found
}

/// A candidate chip as the transcript shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateChip {
    pub candidate_id: u64,
    /// `owner/repo#number`, built from the parsed key; never the raw tool output.
    pub label: String,
    pub state: ChipState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipState {
    /// Waiting for the person: Link PR / Ignore.
    Offered,
    /// The thread is linked to this pull request now: a one-line confirmation.
    Linked,
}

/// The chips of a thread by the tool call that produced them. A dismissed candidate shows
/// nothing; a confirmed one shows the linked line only while the thread is still linked to
/// that pull request.
#[must_use]
pub fn chips_by_call(view: &ThreadPullRequest) -> HashMap<(String, String), Vec<CandidateChip>> {
    let linked = view.key();
    let mut chips: HashMap<(String, String), Vec<CandidateChip>> = HashMap::new();
    for candidate in &view.candidates {
        let state = match candidate.state {
            CandidateState::Dismissed => continue,
            CandidateState::Presented if Some(&candidate.key) == linked => ChipState::Linked,
            CandidateState::Presented => ChipState::Offered,
            CandidateState::Confirmed if Some(&candidate.key) == linked => ChipState::Linked,
            CandidateState::Confirmed => continue,
        };
        chips
            .entry((
                candidate.source_run_id.clone(),
                candidate.source_call_id.clone(),
            ))
            .or_default()
            .push(CandidateChip {
                candidate_id: candidate.candidate_id,
                label: key_label(&candidate.key),
                state,
            });
    }
    chips
}

/// The user's operations on the bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullRequestAction {
    Refresh {
        thread_id: String,
    },
    /// Expand or collapse the bar.
    ToggleExpanded {
        thread_id: String,
    },
    Unlink {
        thread_id: String,
    },
    Confirm {
        thread_id: String,
        candidate_id: u64,
    },
    Dismiss {
        thread_id: String,
        candidate_id: u64,
    },
    /// Open the "Link PR…" dialog for the thread.
    OpenLinkDialog {
        thread_id: String,
    },
    /// Link from the dialog; `locator` is a URL or `owner/repo#N`.
    Link {
        thread_id: String,
        locator: String,
    },
}

/// Per-thread presentation state of the bar: whether it is expanded, and the memory that makes
/// a failure expand it exactly once.
#[derive(Debug, Default, Clone)]
pub struct BarState {
    pub expanded: bool,
    /// The link revision the memory below is about.
    link_revision: Option<u64>,
    /// Whether the last view this state saw was failing; `None` before the first view.
    was_failing: Option<bool>,
}

impl BarState {
    /// Takes in a newly loaded view. A move into failure expands the bar once; a failure that
    /// stays (polls that find the same failure, or one that returns after a failed fetch) never
    /// expands it again, so a bar the person closed stays closed. The first view after a link or
    /// an application start is a baseline, not a transition.
    pub fn observe(&mut self, view: &ThreadPullRequest) {
        let Some(link) = &view.link else {
            *self = Self::default();
            return;
        };
        if self.link_revision != Some(link.link_revision) {
            self.link_revision = Some(link.link_revision);
            self.was_failing = None;
        }
        // A view that cannot say how the checks stand (a failed fetch, no facts yet, no checks)
        // keeps what was last established: recovering into the same failure is no transition.
        if glyph(view) == Some(PrGlyph::Unknown) {
            return;
        }
        let failing = is_failing(view);
        if failing && self.was_failing == Some(false) {
            self.expanded = true;
        }
        self.was_failing = Some(failing);
    }
}

/// Everything the application remembers about pull requests, per thread.
#[derive(Debug, Default)]
pub struct PullRequestsModel {
    pub enabled: bool,
    views: BTreeMap<String, ThreadPullRequest>,
    bars: BTreeMap<String, BarState>,
    /// Threads known to have no pull request data worth a bar (loaded, nothing linked, no
    /// candidates), so showing them does not read again.
    loaded: BTreeSet<String>,
}

impl PullRequestsModel {
    #[must_use]
    pub fn view(&self, thread: &str) -> Option<&ThreadPullRequest> {
        self.views.get(thread).filter(|_| self.enabled)
    }

    /// The thread's view only when something is linked (the bar and badge condition).
    #[must_use]
    pub fn linked(&self, thread: &str) -> Option<&ThreadPullRequest> {
        self.view(thread).filter(|view| view.link.is_some())
    }

    #[must_use]
    pub fn is_loaded(&self, thread: &str) -> bool {
        self.loaded.contains(thread)
    }

    pub fn bar_mut(&mut self, thread: &str) -> &mut BarState {
        self.bars.entry(thread.to_owned()).or_default()
    }

    #[must_use]
    pub fn bar(&self, thread: &str) -> Option<&BarState> {
        self.bars.get(thread)
    }

    /// Replaces a thread's view with a fresh read.
    pub fn set(&mut self, view: ThreadPullRequest) {
        self.bars
            .entry(view.thread_id.clone())
            .or_default()
            .observe(&view);
        self.loaded.insert(view.thread_id.clone());
        self.views.insert(view.thread_id.clone(), view);
    }

    /// Forgets everything (the feature was switched off, or the database changed).
    pub fn clear(&mut self) {
        self.views.clear();
        self.bars.clear();
        self.loaded.clear();
    }

    /// Every loaded thread with a link, whether or not the feature is shown right now.
    pub fn linked_views_all(&self) -> impl Iterator<Item = &ThreadPullRequest> {
        self.views.values().filter(|view| view.link.is_some())
    }

    /// Every thread with a link, for the badges.
    pub fn linked_views(&self) -> impl Iterator<Item = &ThreadPullRequest> {
        self.views
            .values()
            .filter(|view| self.enabled && view.link.is_some())
    }

    /// The threads whose view shows the pull request `key`.
    pub fn threads_showing<'a>(
        &'a self,
        key: &'a PullRequestKey,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.views
            .values()
            .filter(move |view| view.key() == Some(key))
            .map(|view| view.thread_id.as_str())
    }
}

/// What the sidebar row shows: the number and the glyph class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullRequestBadge {
    pub number: u64,
    pub glyph: PrGlyph,
}

#[must_use]
pub fn badge(view: &ThreadPullRequest) -> Option<PullRequestBadge> {
    Some(PullRequestBadge {
        number: view.number()?,
        glyph: glyph(view)?,
    })
}

#[cfg(test)]
mod tests;
