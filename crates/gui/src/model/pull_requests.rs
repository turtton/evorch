//! What the pull request surfaces show (ADR 0043 Phase 1): the egui-free view of a thread's
//! linked pull request, read from the database, and the rules that turn it into words and
//! glyphs.
//!
//! Nothing here fetches or writes. The facts are the stored snapshot of the pull request, the
//! link and the local HEAD the owner recorded; the model never trusts an event's payload.
//! A pull request whose state cannot be established (no facts, a failed fetch, no checks, a
//! partial list of checks) is never shown as passing.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use runtime::pull_requests::HeadAttribution;
use runtime::pull_requests::diff::{ChecksVerdict, checks_verdict, is_pending};
use storage::pull_request_auto_fix::{
    PR_AUTO_FIX_LIMIT, PrAutoFixState, PrNoticeMode, PrNoticePhase,
};
use storage::pull_requests::{
    CandidateState, CheckConclusion, CheckStatus, FetchFailure, LocalHeadObservation,
    PullRequestCandidate, PullRequestDisplay, PullRequestFacts, PullRequestKey, PullRequestLink,
    PullRequestState,
};

use crate::panes::pull_request_widgets::{self as widgets, AutoFixInput, BarMemory, NoticeState};
pub use crate::panes::pull_request_widgets::{
    AutoFixChip, AutoFixControls, ChipTone, Control, Mismatch, PrGlyph, PullRequestBadge,
    STALE_AFTER_SECONDS, Status, now_ns, short_sha,
};

#[cfg(test)]
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
    /// The PR-wide auto fix as the same read found it; `None` when nothing is linked.
    pub auto_fix: Option<AutoFixView>,
}

/// The notice reserved for the pull request's current head, in any phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadNotice {
    pub notice_id: String,
    pub phase: PrNoticePhase,
    pub mode: PrNoticeMode,
}

/// What the PR-wide auto fix keeps (shared by every thread that shows the pull request), and
/// what decides whether the controls can act.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoFixView {
    pub state: PrAutoFixState,
    /// The notice for the head in `facts`, if one was ever reserved.
    pub head_notice: Option<HeadNotice>,
    /// This thread is stopped (durable Stop): a notice to it needs an explicit Resume.
    pub this_stopped: bool,
    /// Notices are held back after repeated wakes without progress.
    pub suppressed: bool,
    /// What the coordinator knows about the shown head's origin. `Unknown` also when nothing
    /// could be asked (another window owns the thread, no service behind the reader).
    pub head_origin: HeadAttribution,
    /// The trusted global switch for automatic sends; manual sends do not depend on it.
    pub global_enabled: bool,
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

/// `owner/repo#number` of a key.
#[must_use]
pub fn key_label(key: &PullRequestKey) -> String {
    format!("{}#{}", key.repository, key.number)
}

/// The runtime's verdict of the facts, in the shared words.
fn verdict(facts: &PullRequestFacts) -> widgets::Verdict {
    match checks_verdict(facts) {
        ChecksVerdict::Unknown => widgets::Verdict::Unknown,
        ChecksVerdict::Pending => widgets::Verdict::Pending,
        ChecksVerdict::Failing => widgets::Verdict::Failing,
        ChecksVerdict::Passing => widgets::Verdict::Passing,
        ChecksVerdict::Unsettled => widgets::Verdict::Unsettled,
    }
}

pub(crate) const fn protocol_state(
    state: PullRequestState,
) -> workbench_protocol::namespaces::pull_requests::PullRequestState {
    use workbench_protocol::namespaces::pull_requests::PullRequestState as Wire;
    match state {
        PullRequestState::Open => Wire::Open,
        PullRequestState::Closed => Wire::Closed,
        PullRequestState::Merged => Wire::Merged,
    }
}

pub(crate) const fn protocol_review(
    decision: storage::pull_requests::ReviewDecision,
) -> workbench_protocol::namespaces::pull_requests::ReviewDecision {
    use storage::pull_requests::ReviewDecision as Stored;
    use workbench_protocol::namespaces::pull_requests::ReviewDecision as Wire;
    match decision {
        Stored::Approved => Wire::Approved,
        Stored::ChangesRequested => Wire::ChangesRequested,
        Stored::ReviewRequired => Wire::ReviewRequired,
        Stored::None => Wire::None,
    }
}

/// The checks of the facts as the shared rows: their stored labels.
pub(crate) fn check_rows(facts: &PullRequestFacts) -> Vec<widgets::CheckRow<'_>> {
    facts
        .checks
        .iter()
        .map(|check| widgets::CheckRow {
            name: &check.name,
            status: check.status.as_str(),
            conclusion: check.conclusion.map(CheckConclusion::as_str),
        })
        .collect()
}

/// The glyph class of the thread's pull request; `None` without a link.
#[must_use]
pub fn glyph(view: &ThreadPullRequest) -> Option<PrGlyph> {
    widgets::glyph_from(
        view.link.is_some(),
        view.facts.as_ref().map(|facts| protocol_state(facts.state)),
        view.failure.is_some(),
        view.facts
            .as_ref()
            .map_or(widgets::Verdict::Unknown, verdict),
    )
}

/// Whether the open pull request has a failed check (the condition that expands the bar).
#[must_use]
pub fn is_failing(view: &ThreadPullRequest) -> bool {
    glyph(view) == Some(PrGlyph::Failing)
}

/// The one-line CI summary: `CI passing 5/5`, `CI failing 2/5`, `CI running 3/5 done`, …
#[must_use]
pub fn ci_summary(view: &ThreadPullRequest) -> String {
    let counts = view.facts.as_ref().map(|facts| {
        widgets::CiCounts::of(
            facts.state == PullRequestState::Open,
            verdict(facts),
            &check_rows(facts),
        )
    });
    widgets::ci_words(counts, view.failure.is_some())
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
    widgets::state_word(protocol_state(state))
}

/// The disagreements between the pull request and the local checkout. Only for an open pull
/// request whose newest fetch worked: after a merge the branch is expected to move on.
#[must_use]
pub fn mismatches(view: &ThreadPullRequest) -> Vec<Mismatch> {
    let Some(facts) = &view.facts else {
        return Vec::new();
    };
    widgets::mismatches_of(
        facts.state == PullRequestState::Open && view.failure.is_none(),
        &facts.head_branch,
        &facts.head_sha,
        view.local_head.as_ref().map(|head| head.head_sha.as_str()),
    )
}

/// Whether the last successful fetch is older than [`STALE_AFTER_SECONDS`], tracked here or not.
#[must_use]
pub fn is_stale(view: &ThreadPullRequest, now_ns: i64) -> bool {
    widgets::is_stale_at(view.fetched_at_ns, now_ns)
}

/// Every status that holds for the view, in the order they are shown.
#[must_use]
pub fn statuses(view: &ThreadPullRequest, now_ns: i64) -> Vec<Status> {
    widgets::statuses_of(
        view.tracked_here,
        view.fetched_at_ns,
        view.failure
            .as_ref()
            .map(|failure| (failure.kind.as_str(), failure.message.as_str())),
        now_ns,
    )
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

/// The controls and statuses of the auto fix, as words. Pure: it says what the controls can
/// do; the operations check their own revisions. `None` without a link or a read of the
/// state.
#[must_use]
pub fn auto_fix_controls(view: &ThreadPullRequest, now_ns: i64) -> Option<AutoFixControls> {
    let link = view.link.as_ref()?;
    let auto = view.auto_fix.as_ref()?;
    let state = &auto.state;
    let open = view
        .facts
        .as_ref()
        .is_some_and(|facts| facts.state == PullRequestState::Open);
    let head_notice = auto
        .head_notice
        .as_ref()
        .and_then(|notice| match notice.phase {
            PrNoticePhase::Adopted => Some(NoticeState::Sent),
            PrNoticePhase::Reserved | PrNoticePhase::Ready => Some(NoticeState::Sending),
            PrNoticePhase::Cancelled => None,
        });
    let input = AutoFixInput {
        thread: &view.thread_id,
        link_revision: link.link_revision,
        tracked: view.tracked_here,
        open,
        failing: is_failing(view),
        checks_pending: is_pending(view.facts.as_ref()),
        enabled: state.enabled,
        destination: state.destination_thread_id.as_deref(),
        policy_link_revision: state.link_revision,
        policy_revision: state.policy_revision,
        used: state.used,
        reserved: state.reserved,
        limit: PR_AUTO_FIX_LIMIT,
        limit_reached: state.used >= PR_AUTO_FIX_LIMIT && !state.enabled,
        global_enabled: auto.global_enabled,
        cooldown_until_ns: state.cooldown_until_ns,
        stop_reason: state.stop_reason.as_deref(),
        head_notice,
        this_stopped: auto.this_stopped,
        suppressed: auto.suppressed,
        head_from_agent: auto.head_origin != HeadAttribution::Unknown,
        head_sha: view.facts.as_ref().map(|facts| facts.head_sha.as_str()),
    };
    Some(widgets::auto_fix_controls_of(&input, now_ns))
}

/// The user's operations on the bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullRequestAction {
    /// Turn auto fix on for this thread, or off. Turning it on while another thread has it
    /// opens the confirmation first.
    ToggleAutoFix {
        thread_id: String,
    },
    /// The confirmation of moving auto fix to this thread.
    ConfirmAutoFixDestination {
        thread_id: String,
    },
    /// The confirmation was cancelled.
    CancelAutoFixDestination,
    /// Start the automatic count again (after the limit).
    ResetAutoFixBudget {
        thread_id: String,
    },
    /// Send the failing head to the agent now.
    SendToAgent {
        thread_id: String,
    },
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
    memory: BarMemory,
}

impl BarState {
    /// An automatic notice is now working on the failure: the bar gets out of the way until
    /// the next failure expands it again.
    pub fn collapse(&mut self) {
        self.expanded = false;
    }

    /// Takes in a newly loaded view. A move into failure expands the bar once; a failure that
    /// stays (polls that find the same failure, or one that returns after a failed fetch) never
    /// expands it again, so a bar the person closed stays closed. The first view after a link or
    /// an application start is a baseline, not a transition.
    pub fn observe(&mut self, view: &ThreadPullRequest) {
        let Some(link) = &view.link else {
            *self = Self::default();
            return;
        };
        if self.memory.observe(Some(link.link_revision), glyph(view)) {
            self.expanded = true;
        }
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

#[must_use]
pub fn badge(view: &ThreadPullRequest) -> Option<PullRequestBadge> {
    Some(PullRequestBadge {
        number: view.number()?,
        glyph: glyph(view)?,
    })
}

#[cfg(test)]
mod protocol_parity_tests;
#[cfg(test)]
pub(crate) mod tests;
