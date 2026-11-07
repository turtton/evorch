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

/// How a chip reads; colour is never the only signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipTone {
    Muted,
    Good,
    Warning,
}

/// One status of the auto fix, with the hover that says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoFixChip {
    pub text: String,
    pub tone: ChipTone,
    pub hover: Option<String>,
}

/// A button's state: when it cannot act, `hover` says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub enabled: bool,
    pub hover: String,
}

impl Control {
    fn on(hover: &str) -> Self {
        Self {
            enabled: true,
            hover: hover.to_owned(),
        }
    }

    fn off(why: &str) -> Self {
        Self {
            enabled: false,
            hover: why.to_owned(),
        }
    }
}

/// The auto fix controls and statuses of one thread's bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoFixControls {
    /// Auto fix is on and this thread is the one it notifies.
    pub on_here: bool,
    pub toggle: Control,
    pub send: Control,
    /// Present only once the limit stopped auto fix.
    pub reset: Option<Control>,
    pub chips: Vec<AutoFixChip>,
    pub link_revision: u64,
    pub policy_revision: u64,
    /// The head the "send" acts on (what the bar shows).
    pub head_sha: Option<String>,
    /// Turning auto fix on here moves it from this thread, which needs the person's
    /// confirmation.
    pub moves_from: Option<String>,
}

/// The reason stored when the third automatic notice stopped auto fix.
const LIMIT_REACHED: &str = "limit_reached";

/// Whole minutes left of a cooldown, rounded up.
fn minutes_left(now_ns: i64, until_ns: i64) -> i64 {
    let nanos = until_ns.saturating_sub(now_ns).max(0);
    (nanos + 59 * NANOS_PER_SECOND) / (60 * NANOS_PER_SECOND)
}

/// The controls and statuses of the auto fix, as words. Pure: it says what the controls can
/// do; the operations check their own revisions. `None` without a link or a read of the
/// state.
#[must_use]
pub fn auto_fix_controls(view: &ThreadPullRequest, now_ns: i64) -> Option<AutoFixControls> {
    let link = view.link.as_ref()?;
    let auto = view.auto_fix.as_ref()?;
    let state = &auto.state;
    let this = view.thread_id.as_str();
    let destination = state.destination_thread_id.as_deref();
    let on_here = state.enabled
        && destination == Some(this)
        && state.link_revision == Some(link.link_revision);
    let moves_from = destination.filter(|d| *d != this).map(str::to_owned);
    let open = view
        .facts
        .as_ref()
        .is_some_and(|facts| facts.state == PullRequestState::Open);
    let failing = open && is_failing(view);
    let limit_reached = state.used >= PR_AUTO_FIX_LIMIT && !state.enabled;
    let mut chips = Vec::new();

    let toggle = if !view.tracked_here {
        Control::off("Not tracked in this window")
    } else if !open {
        Control::off("The pull request is not open")
    } else if limit_reached {
        Control::off("Limit reached; reset the count to turn auto fix on")
    } else if on_here {
        Control::on("Stop sending CI failures to the agent automatically")
    } else if moves_from.is_some() {
        Control::on("Send CI failures to this thread instead of the one that has it now")
    } else {
        Control::on("Send CI failures to the agent automatically")
    };

    if on_here {
        let left = PR_AUTO_FIX_LIMIT.saturating_sub(state.used + state.reserved);
        chips.push(AutoFixChip {
            text: format!("{left} left"),
            tone: if left == 0 {
                ChipTone::Warning
            } else {
                ChipTone::Muted
            },
            hover: Some(format!(
                "{} of {PR_AUTO_FIX_LIMIT} automatic sends used",
                state.used + state.reserved
            )),
        });
    } else if state.enabled && moves_from.is_some() {
        chips.push(AutoFixChip {
            text: "On in another thread".to_owned(),
            tone: ChipTone::Muted,
            hover: Some("Another thread receives this pull request's CI failures".to_owned()),
        });
    }
    if on_here && !auto.global_enabled {
        chips.push(AutoFixChip {
            text: "Off by settings".to_owned(),
            tone: ChipTone::Warning,
            hover: Some(
                "Automatic sends are turned off in the settings; Send to agent still works"
                    .to_owned(),
            ),
        });
    }
    if state.enabled
        && let Some(until) = state.cooldown_until_ns.filter(|until| *until > now_ns)
    {
        chips.push(AutoFixChip {
            text: format!("cooldown {}m", minutes_left(now_ns, until)),
            tone: ChipTone::Muted,
            hover: Some("The next automatic send waits for the cooldown".to_owned()),
        });
    }
    if limit_reached {
        let hover = (state.stop_reason.as_deref() == Some(LIMIT_REACHED))
            .then(|| "Auto fix turned itself off".to_owned());
        chips.push(AutoFixChip {
            text: format!("Limit reached {}/{PR_AUTO_FIX_LIMIT}", state.used),
            tone: ChipTone::Warning,
            hover,
        });
    }

    let head_notice = auto.head_notice.as_ref();
    let sent = head_notice.is_some_and(|n| n.phase == PrNoticePhase::Adopted);
    let sending = head_notice
        .is_some_and(|n| matches!(n.phase, PrNoticePhase::Reserved | PrNoticePhase::Ready));
    if sent {
        chips.push(AutoFixChip {
            text: "Sent".to_owned(),
            tone: ChipTone::Good,
            hover: Some("This commit's failure was sent to the agent".to_owned()),
        });
    } else if sending {
        chips.push(AutoFixChip {
            text: "Sending".to_owned(),
            tone: ChipTone::Muted,
            hover: Some("Preparing the failure for the agent".to_owned()),
        });
    } else if on_here && failing {
        if is_pending(view.facts.as_ref()) {
            chips.push(AutoFixChip {
                text: "Waiting for checks".to_owned(),
                tone: ChipTone::Muted,
                hover: Some("Auto fix waits until every check has finished".to_owned()),
            });
        } else if auto.head_origin == HeadAttribution::Unknown {
            chips.push(AutoFixChip {
                text: "External commit".to_owned(),
                tone: ChipTone::Warning,
                hover: Some(
                    "Not confirmed as the agent's push, so it is not sent automatically".to_owned(),
                ),
            });
        }
    }
    if auto.this_stopped {
        chips.push(AutoFixChip {
            text: "Stopped".to_owned(),
            tone: ChipTone::Warning,
            hover: Some("Stopped by you; resume the thread to send again".to_owned()),
        });
    } else if auto.suppressed {
        chips.push(AutoFixChip {
            text: "Paused".to_owned(),
            tone: ChipTone::Warning,
            hover: Some("No progress after repeated sends; a message of yours resumes it".into()),
        });
    }

    let send = if !view.tracked_here {
        Control::off("Not tracked in this window")
    } else if sent {
        Control::off("Already sent for this commit")
    } else if sending {
        Control::off("Sending to the agent")
    } else if !failing {
        Control::off("No failed check to send")
    } else if auto.this_stopped {
        Control::off("Stopped; resume the thread first")
    } else if auto.suppressed {
        Control::off("Paused after repeated sends; a message of yours resumes it")
    } else {
        Control::on("Send this failure to the agent now")
    };

    let reset = limit_reached.then(|| {
        if !view.tracked_here {
            Control::off("Not tracked in this window")
        } else if moves_from.is_some() {
            Control::off("Reset it in the thread that turned auto fix on")
        } else {
            Control::on("Start the count again and turn auto fix on")
        }
    });

    Some(AutoFixControls {
        on_here,
        toggle,
        send,
        reset,
        chips,
        link_revision: link.link_revision,
        policy_revision: state.policy_revision,
        head_sha: view.facts.as_ref().map(|facts| facts.head_sha.clone()),
        moves_from,
    })
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
    /// The link revision the memory below is about.
    link_revision: Option<u64>,
    /// Whether the last view this state saw was failing; `None` before the first view.
    was_failing: Option<bool>,
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
pub(crate) mod tests;
