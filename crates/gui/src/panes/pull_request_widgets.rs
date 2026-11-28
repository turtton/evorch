//! Pull request words and widgets shared by the embedded workbench and the core client
//! (ADR 0043, ADR 0045): the bar above the composer, the sidebar badge, the candidate row, the
//! "Link PR…" dialog and the auto fix confirmation, and the rules that turn a pull request's
//! facts into glyphs, words and controls.
//!
//! Pure over protocol types and plain values: nothing here reads the runtime, storage or the
//! event bus. Each side builds the view from what it read (the embedded database, or the
//! core's replies) and gets back what was pressed, for its owner to send. A state that cannot
//! be established (no facts, a failed fetch, no checks, a partial list of checks) is never
//! drawn as success.

use std::time::Duration;

use egui::{Align, Color32, Layout, RichText, Sense, Ui};
use workbench_protocol::namespaces::pull_requests::{PullRequestState, ReviewDecision};

use crate::format;
use crate::theme::icons;
use crate::theme::text::{h3, medium, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::{ghost, primary_button, surface_frame};

/// Facts older than this, in a window that tracks the thread, read as stale: the poll
/// interval is at most five minutes, so a snapshot this old means polling is not working.
pub const STALE_AFTER_SECONDS: i64 = 15 * 60;

const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// The wall clock, nanoseconds since the Unix epoch.
#[must_use]
pub fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
        .unwrap_or(0)
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

/// How the checks of a pull request stand (the runtime's verdict, over labels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No check is known, or the list is known to be partial and nothing failed in it.
    Unknown,
    /// Some check has not finished.
    Pending,
    /// A finished check failed.
    Failing,
    /// Every check finished and passed (skipped and neutral ones count as passed).
    Passing,
    /// All finished, none failed, but not all passed (cancelled, stale, needs a person, …).
    Unsettled,
}

/// One check as both sides hold it: the snake_case labels of the facts (`completed`,
/// `failure`, …). A label nobody knows is shown as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckRow<'a> {
    pub name: &'a str,
    pub status: &'a str,
    pub conclusion: Option<&'a str>,
}

const COMPLETED: &str = "completed";

impl CheckRow<'_> {
    fn completed(&self) -> bool {
        self.status == COMPLETED
    }

    /// A finished check that failed (the condition that makes the pull request fail).
    #[must_use]
    pub fn failed(&self) -> bool {
        self.completed()
            && matches!(
                self.conclusion,
                Some("failure" | "timed_out" | "startup_failure")
            )
    }

    fn passed(&self) -> bool {
        matches!(self.conclusion, Some("success" | "neutral" | "skipped"))
    }
}

/// The verdict of a list of checks. A failure counts even when the list is partial: a known
/// failure does not become less true because more checks exist.
#[must_use]
pub fn verdict_of_labels(checks: &[CheckRow<'_>], complete: bool) -> Verdict {
    if checks.iter().any(CheckRow::failed) {
        return Verdict::Failing;
    }
    if checks.iter().any(|check| !check.completed()) {
        return Verdict::Pending;
    }
    if checks.is_empty() || !complete {
        return Verdict::Unknown;
    }
    if checks.iter().all(CheckRow::passed) {
        Verdict::Passing
    } else {
        Verdict::Unsettled
    }
}

/// Whether the next fetch should come soon: a check is still running, or the list is not the
/// whole list.
#[must_use]
pub fn checks_pending(checks: &[CheckRow<'_>], complete: bool) -> bool {
    !complete || checks.iter().any(|check| !check.completed())
}

/// The glyph class of a thread's pull request; `None` without a link. `state` is `None`
/// before the first successful fetch.
#[must_use]
pub fn glyph_from(
    linked: bool,
    state: Option<PullRequestState>,
    fetch_failed: bool,
    verdict: Verdict,
) -> Option<PrGlyph> {
    if !linked {
        return None;
    }
    let Some(state) = state else {
        return Some(PrGlyph::Unknown);
    };
    Some(match state {
        PullRequestState::Merged => PrGlyph::Merged,
        PullRequestState::Closed => PrGlyph::Closed,
        // The newest answer is a failure: what was stored may be old.
        PullRequestState::Open if fetch_failed => PrGlyph::Unknown,
        PullRequestState::Open => match verdict {
            Verdict::Passing => PrGlyph::Passing,
            Verdict::Failing => PrGlyph::Failing,
            Verdict::Pending => PrGlyph::Pending,
            Verdict::Unsettled | Verdict::Unknown => PrGlyph::Unknown,
        },
    })
}

/// What the one-line CI summary counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CiCounts {
    pub open: bool,
    pub verdict: Verdict,
    pub total: usize,
    pub failed: usize,
    pub finished: usize,
}

impl CiCounts {
    #[must_use]
    pub fn of(open: bool, verdict: Verdict, checks: &[CheckRow<'_>]) -> Self {
        Self {
            open,
            verdict,
            total: checks.len(),
            failed: checks.iter().filter(|check| check.failed()).count(),
            finished: checks.iter().filter(|check| check.completed()).count(),
        }
    }
}

/// The one-line CI summary: `CI passing 5/5`, `CI failing 2/5`, `CI running 3/5 done`, …
/// `facts` is `None` before the first successful fetch.
#[must_use]
pub fn ci_words(facts: Option<CiCounts>, fetch_failed: bool) -> String {
    let Some(CiCounts {
        open,
        verdict,
        total,
        failed,
        finished,
    }) = facts
    else {
        return if fetch_failed {
            "Not fetched".to_owned()
        } else {
            "Waiting for the first fetch".to_owned()
        };
    };
    if !open {
        return String::new();
    }
    if fetch_failed && failed == 0 {
        return "CI unknown".to_owned();
    }
    match verdict {
        Verdict::Failing => format!("CI failing {failed}/{total}"),
        Verdict::Pending => format!("CI running {finished}/{total} done"),
        Verdict::Passing => format!("CI passing {total}/{total}"),
        Verdict::Unsettled => format!("CI unsettled {finished}/{total}"),
        Verdict::Unknown if total == 0 => "No checks".to_owned(),
        Verdict::Unknown => format!("CI incomplete {total} checks"),
    }
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

#[must_use]
pub const fn review_word(decision: ReviewDecision) -> &'static str {
    match decision {
        ReviewDecision::Approved => "Approved",
        ReviewDecision::ChangesRequested => "Changes requested",
        ReviewDecision::ReviewRequired => "Review required",
        ReviewDecision::None => "None",
    }
}

/// How a check ended, in words. A label nobody knows is shown as it is.
#[must_use]
pub fn conclusion_word(conclusion: Option<&str>) -> &str {
    match conclusion {
        Some("timed_out") => "timed out",
        Some("action_required") => "action required",
        Some("startup_failure") => "startup failure",
        Some("other") | None => "unfinished",
        Some(word) => word,
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
/// request whose newest fetch worked (`open_and_fetch_ok`): after a merge the branch is
/// expected to move on.
#[must_use]
pub fn mismatches_of(
    open_and_fetch_ok: bool,
    head_branch: &str,
    head_sha: &str,
    local_head: Option<&str>,
) -> Vec<Mismatch> {
    if !open_and_fetch_ok {
        return Vec::new();
    }
    match local_head {
        None => vec![Mismatch::NoLocalCheckout {
            branch: head_branch.to_owned(),
        }],
        Some(local) if local != head_sha => vec![Mismatch::LocalHeadDiffers {
            local: local.to_owned(),
            remote: head_sha.to_owned(),
        }],
        Some(_) => Vec::new(),
    }
}

/// One thing a person should know about how far the shown facts can be trusted. The kinds
/// are independent: each is shown whenever it holds, whatever the others say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Nothing newer than [`STALE_AFTER_SECONDS`] arrived.
    Stale,
    /// The newest fetch failed; the facts shown are the last ones that arrived.
    FetchFailed {
        /// The class of the failure (`unauthenticated`, `network`, …).
        kind: String,
        /// The stored, sanitized message.
        message: String,
    },
    /// Reading the thread again failed: what is shown may be out of date (core client only).
    ReadFailed { message: String },
}

pub const READ_FAILED: &str = "Could not read the latest state";

/// Whether the last successful fetch is older than [`STALE_AFTER_SECONDS`].
#[must_use]
pub fn is_stale_at(fetched_at_ns: Option<i64>, now_ns: i64) -> bool {
    fetched_at_ns
        .is_some_and(|at| now_ns.saturating_sub(at) > STALE_AFTER_SECONDS * NANOS_PER_SECOND)
}

/// Every status that holds, in the order they are shown. `failure` is the kind and the
/// message of the newest fetch, when it failed.
#[must_use]
pub fn statuses_of(
    fetched_at_ns: Option<i64>,
    failure: Option<(&str, &str)>,
    now_ns: i64,
) -> Vec<Status> {
    let mut found = Vec::new();
    if is_stale_at(fetched_at_ns, now_ns) {
        found.push(Status::Stale);
    }
    if let Some((kind, message)) = failure {
        found.push(Status::FetchFailed {
            kind: kind.to_owned(),
            message: message.to_owned(),
        });
    }
    found
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
    pub(crate) fn on(hover: &str) -> Self {
        Self {
            enabled: true,
            hover: hover.to_owned(),
        }
    }

    pub(crate) fn off(why: &str) -> Self {
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

/// The phase of the notice reserved for the shown head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeState {
    /// The agent took it.
    Sent,
    /// Reserved or prepared, not taken yet.
    Sending,
}

/// What the auto fix controls are computed from, as plain values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoFixInput<'a> {
    /// The thread whose bar this is.
    pub thread: &'a str,
    /// The revision of the thread's active link.
    pub link_revision: u64,
    /// The pull request is open.
    pub open: bool,
    /// The open pull request has a failed check.
    pub failing: bool,
    /// A check is still running, or the list is partial (or nothing was fetched yet).
    pub checks_pending: bool,
    pub enabled: bool,
    pub destination: Option<&'a str>,
    /// The link revision the policy was made for.
    pub policy_link_revision: Option<u64>,
    pub policy_revision: u64,
    pub used: u32,
    pub reserved: u32,
    pub limit: u32,
    /// The budget is spent and only a reset starts it again.
    pub limit_reached: bool,
    /// The trusted global switch for automatic sends; manual sends do not depend on it.
    pub global_enabled: bool,
    pub cooldown_until_ns: Option<i64>,
    pub stop_reason: Option<&'a str>,
    pub head_notice: Option<NoticeState>,
    /// This thread is stopped (durable Stop): a notice to it needs an explicit Resume.
    pub this_stopped: bool,
    /// Notices are held back after repeated wakes without progress.
    pub suppressed: bool,
    /// The shown head is known to be the agent's push.
    pub head_from_agent: bool,
    pub head_sha: Option<&'a str>,
}

/// The reason stored when the last automatic notice stopped auto fix.
const LIMIT_REACHED: &str = "limit_reached";

/// Whole minutes left of a cooldown, rounded up.
fn minutes_left(now_ns: i64, until_ns: i64) -> i64 {
    let nanos = until_ns.saturating_sub(now_ns).max(0);
    (nanos + 59 * NANOS_PER_SECOND) / (60 * NANOS_PER_SECOND)
}

/// The controls and statuses of the auto fix, as words. Pure: it says what the controls can
/// do; the operations check their own revisions.
#[must_use]
pub fn auto_fix_controls_of(input: &AutoFixInput<'_>, now_ns: i64) -> AutoFixControls {
    let this = input.thread;
    let limit = input.limit;
    let destination = input.destination;
    let on_here = input.enabled
        && destination == Some(this)
        && input.policy_link_revision == Some(input.link_revision);
    let moves_from = destination.filter(|d| *d != this).map(str::to_owned);
    let open = input.open;
    let failing = open && input.failing;
    let limit_reached = input.limit_reached;
    let mut chips = Vec::new();

    let toggle = if !open {
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
        let left = limit.saturating_sub(input.used + input.reserved);
        chips.push(AutoFixChip {
            text: format!("{left} left"),
            tone: if left == 0 {
                ChipTone::Warning
            } else {
                ChipTone::Muted
            },
            hover: Some(format!(
                "{} of {limit} automatic sends used",
                input.used + input.reserved
            )),
        });
    } else if input.enabled && moves_from.is_some() {
        chips.push(AutoFixChip {
            text: "On in another thread".to_owned(),
            tone: ChipTone::Muted,
            hover: Some("Another thread receives this pull request's CI failures".to_owned()),
        });
    }
    if on_here && !input.global_enabled {
        chips.push(AutoFixChip {
            text: "Off by settings".to_owned(),
            tone: ChipTone::Warning,
            hover: Some(
                "Automatic sends are turned off in the settings; Send to agent still works"
                    .to_owned(),
            ),
        });
    }
    if input.enabled
        && let Some(until) = input.cooldown_until_ns.filter(|until| *until > now_ns)
    {
        chips.push(AutoFixChip {
            text: format!("cooldown {}m", minutes_left(now_ns, until)),
            tone: ChipTone::Muted,
            hover: Some("The next automatic send waits for the cooldown".to_owned()),
        });
    }
    if limit_reached {
        let hover = (input.stop_reason == Some(LIMIT_REACHED))
            .then(|| "Auto fix turned itself off".to_owned());
        chips.push(AutoFixChip {
            text: format!("Limit reached {}/{limit}", input.used),
            tone: ChipTone::Warning,
            hover,
        });
    }

    let sent = input.head_notice == Some(NoticeState::Sent);
    let sending = input.head_notice == Some(NoticeState::Sending);
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
        if input.checks_pending {
            chips.push(AutoFixChip {
                text: "Waiting for checks".to_owned(),
                tone: ChipTone::Muted,
                hover: Some("Auto fix waits until every check has finished".to_owned()),
            });
        } else if !input.head_from_agent {
            chips.push(AutoFixChip {
                text: "External commit".to_owned(),
                tone: ChipTone::Warning,
                hover: Some(
                    "Not confirmed as the agent's push, so it is not sent automatically".to_owned(),
                ),
            });
        }
    }
    if input.this_stopped {
        chips.push(AutoFixChip {
            text: "Stopped".to_owned(),
            tone: ChipTone::Warning,
            hover: Some("Stopped by you; resume the thread to send again".to_owned()),
        });
    } else if input.suppressed {
        chips.push(AutoFixChip {
            text: "Paused".to_owned(),
            tone: ChipTone::Warning,
            hover: Some("No progress after repeated sends; a message of yours resumes it".into()),
        });
    }

    let send = if sent {
        Control::off("Already sent for this commit")
    } else if sending {
        Control::off("Sending to the agent")
    } else if !failing {
        Control::off("No failed check to send")
    } else if input.this_stopped {
        Control::off("Stopped; resume the thread first")
    } else if input.suppressed {
        Control::off("Paused after repeated sends; a message of yours resumes it")
    } else {
        Control::on("Send this failure to the agent now")
    };

    let reset = limit_reached.then(|| Control::on("Start the count again and turn auto fix on"));

    AutoFixControls {
        on_here,
        toggle,
        send,
        reset,
        chips,
        link_revision: input.link_revision,
        policy_revision: input.policy_revision,
        head_sha: input.head_sha.map(str::to_owned),
        moves_from,
    }
}

/// The memory that makes a failure expand the bar exactly once.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BarMemory {
    /// The link revision the memory below is about.
    link_revision: Option<u64>,
    /// Whether the last view this memory saw was failing; `None` before the first view.
    was_failing: Option<bool>,
}

impl BarMemory {
    /// Takes in a newly read view: its link revision (`None` without a link) and its glyph.
    /// Returns whether the bar expands now. A move into failure expands it once; a failure
    /// that stays (polls that find the same failure, or one that returns after a failed
    /// fetch) never expands it again, so a bar the person closed stays closed. The first view
    /// after a link or a start is a baseline, not a transition.
    pub fn observe(&mut self, link_revision: Option<u64>, glyph: Option<PrGlyph>) -> bool {
        let Some(link_revision) = link_revision else {
            *self = Self::default();
            return false;
        };
        if self.link_revision != Some(link_revision) {
            self.link_revision = Some(link_revision);
            self.was_failing = None;
        }
        // A view that cannot say how the checks stand (a failed fetch, no facts yet, no checks)
        // keeps what was last established: recovering into the same failure is no transition.
        if glyph == Some(PrGlyph::Unknown) {
            return false;
        }
        let failing = glyph == Some(PrGlyph::Failing);
        let expand = failing && self.was_failing == Some(false);
        self.was_failing = Some(failing);
        expand
    }
}

/// What the sidebar row shows: the number and the glyph class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullRequestBadge {
    pub number: u64,
    pub glyph: PrGlyph,
}

/// The share of the conversation area the bar may take, expanded.
pub const MAX_HEIGHT_SHARE: f32 = 0.4;

/// The tallest the bar may be for a conversation area of `conversation_height`.
#[must_use]
pub fn max_bar_height(conversation_height: f32) -> f32 {
    conversation_height * MAX_HEIGHT_SHARE
}

pub const REFRESH_LABEL: &str = "Refresh";
pub const UNLINK_LABEL: &str = "Unlink";
pub const AUTO_FIX_LABEL: &str = "Auto fix";
pub const SEND_LABEL: &str = "Send to agent";
pub const RESET_LABEL: &str = "Reset count";

/// The colour, icon and word of a glyph class. Colour is never the only signal.
#[must_use]
pub fn glyph_style(glyph: PrGlyph) -> (&'static str, Color32) {
    match glyph {
        PrGlyph::Passing => (icons::CHECK, palette().SUCCESS),
        PrGlyph::Failing => (icons::X, palette().ERROR_FG),
        PrGlyph::Pending => (icons::CIRCLE, palette().RUNNING),
        PrGlyph::Merged => (icons::GIT_MERGE, palette().MERGED_FG),
        PrGlyph::Closed => (icons::PROHIBIT, palette().TEXT_MUTED),
        PrGlyph::Unknown => (icons::QUESTION, palette().TEXT_MUTED),
    }
}

fn age(now_ns: i64, at_ns: i64) -> String {
    let nanos = u64::try_from(now_ns.saturating_sub(at_ns)).unwrap_or(0);
    format::time_ago(Duration::from_nanos(nanos))
}

/// The facts of the newest successful fetch, as the bar's details show them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactsRows<'a> {
    pub state: PullRequestState,
    pub base_branch: &'a str,
    pub head_branch: &'a str,
    pub head_sha: &'a str,
    pub review: ReviewDecision,
    pub checks: Vec<CheckRow<'a>>,
    pub checks_complete: bool,
}

/// Everything the bar draws, built by its owner from what it read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarView<'a> {
    pub number: Option<u64>,
    /// `owner/repo#number`.
    pub label: Option<String>,
    /// The cleaned title, drawn as plain text.
    pub title: Option<&'a str>,
    pub glyph: PrGlyph,
    pub ci: String,
    pub statuses: Vec<Status>,
    pub fetched_at_ns: Option<i64>,
    /// The message of the newest fetch, when it failed.
    pub failure_message: Option<&'a str>,
    pub facts: Option<FactsRows<'a>>,
    pub local_head: Option<&'a str>,
    pub mismatches: Vec<Mismatch>,
    pub auto_fix: Option<AutoFixControls>,
    pub expanded: bool,
    /// Whether the caret that expands and collapses the bar is drawn.
    pub collapsible: bool,
    /// Wall clock, nanoseconds since the Unix epoch.
    pub now_ns: i64,
    /// `false` greys out every action (another client holds the thread, or an operation runs).
    pub actions_enabled: bool,
}

/// What was pressed on the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarChoice {
    Refresh,
    Unlink,
    ToggleAutoFix,
    SendToAgent,
    ResetCount,
    ToggleExpanded,
}

/// Draws the bar as a bottom panel. `conversation_height` is the height of the area the bar
/// shares with the transcript; the bar never grows past 40% of it.
pub fn bar(ui: &mut Ui, view: &BarView<'_>, conversation_height: f32) -> Option<BarChoice> {
    let mut asked = None;
    let frame = egui::Frame::new()
        .fill(palette().SURFACE)
        .stroke(egui::Stroke::new(1.0, palette().BORDER))
        .inner_margin(egui::Margin::symmetric(SP_3 as i8, SP_1 as i8));
    egui::Panel::bottom(ui.id().with("pull-request-bar"))
        .resizable(false)
        .show_separator_line(false)
        .frame(frame)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = SP_1;
            let fixed = ui.spacing().interact_size.y + SP_1 * 4.0 + 2.0;
            if view.expanded {
                let room = (max_bar_height(conversation_height) - fixed).max(ROW_DENSE);
                egui::ScrollArea::vertical()
                    .id_salt("pull-request-details")
                    .max_height(room)
                    .auto_shrink([false, true])
                    .show(ui, |ui| details(ui, view));
                ui.separator();
            }
            summary_row(ui, view, &mut asked);
        });
    asked
}

/// Draws the bar in place for a pane of any width: the summary, then the controls, each
/// wrapping, then the details (when expanded) scrolling within `details_height`.
pub fn bar_inline(ui: &mut Ui, view: &BarView<'_>, details_height: f32) -> Option<BarChoice> {
    let mut asked = None;
    ui.spacing_mut().item_spacing.y = SP_1;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = SP_2;
        summary_facts(ui, view, &mut asked);
    });
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = SP_2;
        auto_fix_toggle(ui, view, &mut asked);
        send_and_reset(ui, view, &mut asked, ControlOrder::LeftToRight);
        refresh_and_unlink(ui, view, &mut asked, ControlOrder::LeftToRight);
    });
    if view.expanded {
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("pull-request-details")
            .max_height(details_height.max(ROW_DENSE))
            .auto_shrink([false, true])
            .show(ui, |ui| details(ui, view));
    }
    asked
}

/// One line of status: its words, colour and, when there is more to read, the hover.
struct StatusLine {
    text: String,
    color: Color32,
    hover: Option<String>,
}

/// Every status that holds, each on its own (never one hiding another), then when the facts
/// were last fetched.
fn status_lines(view: &BarView<'_>) -> Vec<StatusLine> {
    let mut lines: Vec<StatusLine> = view
        .statuses
        .iter()
        .map(|status| match status {
            Status::Stale => StatusLine {
                text: "Stale".to_owned(),
                color: palette().WARNING_FG,
                hover: Some("The facts are older than the polling interval allows".into()),
            },
            Status::FetchFailed { kind, message } => StatusLine {
                text: format!("Fetch failed ({kind})"),
                color: palette().WARNING_FG,
                hover: Some(message.clone()),
            },
            Status::ReadFailed { message } => StatusLine {
                text: READ_FAILED.to_owned(),
                color: palette().ERROR_FG,
                hover: Some(message.clone()),
            },
        })
        .collect();
    if !lines.is_empty() {
        lines.push(StatusLine {
            text: view.fetched_at_ns.map_or_else(
                || "never fetched".to_owned(),
                |at| format!("last fetched {}", age(view.now_ns, at)),
            ),
            color: palette().TEXT_MUTED,
            hover: None,
        });
    }
    lines
}

/// A ghost button with an icon, named `label` for accessibility.
fn icon_action(ui: &mut Ui, enabled: bool, icon: &str, label: &str, hover: &str) -> bool {
    let response = ui.add_enabled(enabled, ghost(RichText::new(icons::with_icon(icon, label))));
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    response
        .on_hover_text(hover)
        .on_disabled_hover_text(hover)
        .clicked()
}

/// The direction a row adds its controls in. A right-to-left row adds the rightmost first,
/// so both read Auto fix, Send to agent, Reset count, Refresh, Unlink from the left.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ControlOrder {
    RightToLeft,
    LeftToRight,
}

fn refresh_and_unlink(
    ui: &mut Ui,
    view: &BarView<'_>,
    asked: &mut Option<BarChoice>,
    order: ControlOrder,
) {
    let enabled = view.actions_enabled;
    let mut unlink = |ui: &mut Ui| {
        if icon_action(
            ui,
            enabled,
            icons::LINK_BREAK,
            UNLINK_LABEL,
            "Stop tracking this pull request for the thread",
        ) {
            *asked = Some(BarChoice::Unlink);
        }
    };
    let refresh = |ui: &mut Ui| {
        icon_action(
            ui,
            enabled,
            icons::ARROWS_CLOCKWISE,
            REFRESH_LABEL,
            "Fetch the pull request now",
        )
    };
    let refreshed = match order {
        ControlOrder::RightToLeft => {
            unlink(ui);
            refresh(ui)
        }
        ControlOrder::LeftToRight => {
            let refreshed = refresh(ui);
            unlink(ui);
            refreshed
        }
    };
    if refreshed {
        *asked = Some(BarChoice::Refresh);
    }
}

fn send_and_reset(
    ui: &mut Ui,
    view: &BarView<'_>,
    asked: &mut Option<BarChoice>,
    order: ControlOrder,
) {
    let Some(controls) = &view.auto_fix else {
        return;
    };
    let enabled = view.actions_enabled;
    let reset = |ui: &mut Ui| {
        controls
            .reset
            .as_ref()
            .is_some_and(|reset| control_button(ui, RESET_LABEL, reset, enabled).clicked())
    };
    let send = |ui: &mut Ui| control_button(ui, SEND_LABEL, &controls.send, enabled).clicked();
    let (reset, send) = match order {
        ControlOrder::RightToLeft => {
            let reset = reset(ui);
            (reset, send(ui))
        }
        ControlOrder::LeftToRight => {
            let send = send(ui);
            (reset(ui), send)
        }
    };
    if reset {
        *asked = Some(BarChoice::ResetCount);
    }
    if send {
        *asked = Some(BarChoice::SendToAgent);
    }
}

fn auto_fix_toggle(ui: &mut Ui, view: &BarView<'_>, asked: &mut Option<BarChoice>) {
    let Some(controls) = &view.auto_fix else {
        return;
    };
    let toggle_enabled = controls.toggle.enabled && view.actions_enabled;
    let mut on = controls.on_here;
    let toggle = ui.add_enabled(toggle_enabled, egui::Checkbox::new(&mut on, AUTO_FIX_LABEL));
    toggle.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Checkbox,
            toggle_enabled,
            controls.on_here,
            AUTO_FIX_LABEL,
        )
    });
    if hover(toggle, &controls.toggle.hover).clicked() {
        *asked = Some(BarChoice::ToggleAutoFix);
    }
}

fn summary_row(ui: &mut Ui, view: &BarView<'_>, asked: &mut Option<BarChoice>) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = SP_2;
        refresh_and_unlink(ui, view, asked, ControlOrder::RightToLeft);
        send_and_reset(ui, view, asked, ControlOrder::RightToLeft);
        auto_fix_toggle(ui, view, asked);
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = SP_2;
            summary_facts(ui, view, asked);
        });
    });
}

fn summary_facts(ui: &mut Ui, view: &BarView<'_>, asked: &mut Option<BarChoice>) {
    if view.collapsible {
        let caret = if view.expanded {
            icons::CARET_DOWN
        } else {
            icons::CARET_UP
        };
        let toggle = ui.add(ghost(RichText::new(caret)));
        toggle.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::WidgetType::Button,
                true,
                if view.expanded {
                    "Collapse pull request"
                } else {
                    "Expand pull request"
                },
            )
        });
        if toggle.clicked() {
            *asked = Some(BarChoice::ToggleExpanded);
        }
    }
    let (icon, color) = glyph_style(view.glyph);
    if let Some(number) = view.number {
        ui.label(medium(format!("#{number}")));
    }
    ui.label(RichText::new(icon).color(color));
    let state = view
        .facts
        .as_ref()
        .map_or("Unknown", |facts| state_word(facts.state));
    ui.label(RichText::new(state).color(color));
    if !view.ci.is_empty() {
        ui.label(RichText::new(&view.ci).color(color));
    }
    for line in status_lines(view) {
        let label = ui.label(RichText::new(line.text).size(FONT_SMALL).color(line.color));
        if let Some(hover) = line.hover {
            label.on_hover_text(hover);
        }
    }
    if let Some(controls) = &view.auto_fix {
        for chip in &controls.chips {
            let color = match chip.tone {
                ChipTone::Muted => palette().TEXT_MUTED,
                ChipTone::Good => palette().SUCCESS,
                ChipTone::Warning => palette().WARNING_FG,
            };
            let label = ui.label(RichText::new(&chip.text).size(FONT_SMALL).color(color));
            if let Some(text) = &chip.hover {
                label.on_hover_text(text);
            }
        }
    }
    // Last, so the title takes the room that is left and is cut at the end. It is
    // plain text from a third party. egui shows the whole text of a label it had to
    // cut when the pointer rests on it (a second hover text would stack on that one).
    if let Some(title) = view.title {
        ui.add(egui::Label::new(title).truncate());
    } else if let Some(facts) = &view.facts {
        // No title was fetched (yet): the branch says which pull request this is.
        ui.add(egui::Label::new(muted(facts.head_branch)).truncate())
            .on_hover_text("Title not fetched");
    }
}

/// A button whose hover says why it cannot act while it cannot.
fn control_button(ui: &mut Ui, label: &str, control: &Control, actions: bool) -> egui::Response {
    let enabled = control.enabled && actions;
    let button = ui.add_enabled(enabled, ghost(RichText::new(label)));
    button.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, label));
    hover(button, &control.hover)
}

/// The hover of a control, shown also while it is disabled (egui keeps the two apart).
fn hover(response: egui::Response, text: &str) -> egui::Response {
    response
        .on_hover_text(text.to_owned())
        .on_disabled_hover_text(text.to_owned())
}

fn details(ui: &mut Ui, view: &BarView<'_>) {
    ui.spacing_mut().item_spacing.y = SP_1;
    let Some(facts) = &view.facts else {
        ui.label(muted(match view.failure_message {
            Some(message) => format!("Not fetched yet: {message}"),
            None => "Waiting for the first fetch".to_owned(),
        }));
        return;
    };
    if let Some(label) = &view.label {
        ui.label(medium(label));
    }
    if let Some(title) = view.title {
        ui.add(egui::Label::new(title).wrap());
    }
    checks(ui, facts, view.failure_message.is_none());
    ui.add_space(SP_1);
    egui::Grid::new("pull-request-facts")
        .num_columns(2)
        .spacing([SP_3, SP_1])
        .show(ui, |ui| {
            let row = |ui: &mut Ui, name: &str, value: RichText| {
                ui.label(muted(name));
                ui.label(value);
                ui.end_row();
            };
            row(ui, "State", RichText::new(state_word(facts.state)));
            row(
                ui,
                "Branch",
                RichText::new(format!("{} ← {}", facts.base_branch, facts.head_branch)),
            );
            row(
                ui,
                "Head",
                RichText::new(facts.head_sha).monospace().size(FONT_SMALL),
            );
            if let Some(local) = view.local_head {
                row(
                    ui,
                    "Local HEAD",
                    RichText::new(local).monospace().size(FONT_SMALL),
                );
            }
            row(ui, "Review", RichText::new(review_word(facts.review)));
            if let Some(at) = view.fetched_at_ns {
                ui.label(muted("Fetched"));
                ui.label(age(view.now_ns, at))
                    .on_hover_text(format::utc_timestamp(
                        chrono::DateTime::from_timestamp_nanos(at),
                    ));
                ui.end_row();
            }
        });
    for line in status_lines(view) {
        ui.label(RichText::new(line.text).color(line.color));
    }
    for mismatch in &view.mismatches {
        ui.label(
            RichText::new(icons::with_icon(icons::WARNING, mismatch.text()))
                .color(palette().WARNING_FG),
        );
    }
}

/// Failures one by one, the checks still running one by one, and everything that passed on one
/// line.
fn checks(ui: &mut Ui, facts: &FactsRows<'_>, fetch_worked: bool) {
    if facts.state != PullRequestState::Open && facts.checks.is_empty() {
        return;
    }
    let mut passed = 0;
    for check in &facts.checks {
        if check.failed() {
            ui.label(
                RichText::new(icons::with_icon(
                    icons::X,
                    format!("{} · {}", check.name, conclusion_word(check.conclusion)),
                ))
                .color(palette().ERROR_FG),
            );
        } else if !check.completed() {
            ui.label(
                RichText::new(icons::with_icon(
                    icons::CIRCLE,
                    format!("{} · running", check.name),
                ))
                .color(palette().RUNNING),
            );
        } else if check.passed() {
            passed += 1;
        } else {
            ui.label(
                RichText::new(icons::with_icon(
                    icons::QUESTION,
                    format!("{} · {}", check.name, conclusion_word(check.conclusion)),
                ))
                .color(palette().TEXT_MUTED),
            );
        }
    }
    if passed > 0 {
        // A pass is only drawn as success while the list is the whole list and the newest fetch
        // worked; otherwise it is a count, in neutral.
        let (icon, color) = if fetch_worked && facts.checks_complete {
            (icons::CHECK, palette().SUCCESS)
        } else {
            (icons::MINUS, palette().TEXT_MUTED)
        };
        ui.label(RichText::new(icons::with_icon(icon, format!("{passed} passed"))).color(color));
    }
    if !facts.checks_complete {
        ui.label(muted("The list of checks is incomplete"));
    }
}

/// `#12 ✓`: the pull request's number and its glyph. The glyph class also has a word, in the
/// accessible name and the hover; merged reads `merged` outright.
pub fn pull_request_badge(ui: &mut Ui, badge: PullRequestBadge) {
    let (icon, color) = glyph_style(badge.glyph);
    let text = match badge.glyph {
        PrGlyph::Merged => format!("#{} merged", badge.number),
        _ => format!("#{} {icon}", badge.number),
    };
    let label = format!("Pull request #{}: {}", badge.number, badge.glyph.word());
    let response = ui.add(
        egui::Label::new(RichText::new(text).size(FONT_SMALL).color(color)).sense(Sense::hover()),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, &label));
    response.on_hover_text(label);
}

pub const LINK_LABEL: &str = "Link PR";
pub const IGNORE_LABEL: &str = "Ignore";

/// What was pressed on a suggested pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateChoice {
    Link,
    Ignore,
}

/// A suggested pull request: its `owner/repo#number`, "Link PR" (named "Link PR {label}") and
/// "Ignore". The label is built from the parsed key, never the tool output's own text.
pub fn candidate_offer(ui: &mut Ui, label: &str, enabled: bool) -> Option<CandidateChoice> {
    let mut choice = None;
    ui.label(label);
    let link = ui.add_enabled(
        enabled,
        ghost(RichText::new(LINK_LABEL).color(palette().ACCENT)),
    );
    link.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            enabled,
            format!("{LINK_LABEL} {label}"),
        )
    });
    if link.clicked() {
        choice = Some(CandidateChoice::Link);
    }
    let ignore = ui.add_enabled(enabled, ghost(muted(IGNORE_LABEL)));
    if ignore.clicked() {
        choice = Some(CandidateChoice::Ignore);
    }
    choice
}

pub const INPUT_LABEL: &str = "Pull request (URL or owner/repo#N)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkDialogAction {
    Link(String),
    Close,
}

/// What the dialog says about the typed locator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reading {
    /// Nothing typed yet.
    Empty,
    /// Not a github.com pull request; the text is the reason.
    Invalid(String),
    /// A pull request, with a warning when its repository differs from the project's.
    Valid {
        label: String,
        warning: Option<String>,
    },
    /// Text that the core reads when it is sent: the core decides whether it names a pull
    /// request, and says why not.
    Unchecked,
}

impl Reading {
    /// The reading of text nobody parses here.
    #[must_use]
    pub fn unchecked(input: &str) -> Self {
        if input.trim().is_empty() {
            Self::Empty
        } else {
            Self::Unchecked
        }
    }
}

/// The "Link PR…" dialog. `ready` says the thread can be linked now; `note` is a line under
/// the field (the core's answer, or why the dialog waits) in its colour.
pub fn link_dialog_view(
    ctx: &egui::Context,
    input: &mut String,
    reading: &Reading,
    ready: bool,
    note: Option<(&str, Color32)>,
) -> Option<LinkDialogAction> {
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::new("pull-request-link-dialog"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.5).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3("Link pull request"));
            ui.label(muted(INPUT_LABEL));
            let edit = ui.add(
                egui::TextEdit::singleline(input)
                    .hint_text("https://github.com/owner/repo/pull/123")
                    .desired_width(f32::INFINITY),
            );
            edit.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, INPUT_LABEL)
            });
            if !edit.has_focus() && input.is_empty() {
                edit.request_focus();
            }
            match reading {
                Reading::Empty | Reading::Unchecked => {}
                Reading::Invalid(reason) => {
                    ui.colored_label(palette().ERROR_FG, reason);
                }
                Reading::Valid { label, warning } => {
                    ui.label(label);
                    if let Some(warning) = warning {
                        ui.colored_label(palette().WARNING_FG, warning);
                    }
                }
            }
            if let Some((note, color)) = note {
                ui.colored_label(color, note);
            }
            let ready = ready && matches!(reading, Reading::Valid { .. } | Reading::Unchecked);
            let submitted =
                edit.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let link = ui
                    .add_enabled_ui(ready, |ui| primary_button(ui, "Link"))
                    .inner;
                if (link.clicked() || submitted) && ready {
                    action = Some(LinkDialogAction::Link(input.trim().to_owned()));
                }
                if ui.add(ghost("Cancel")).clicked() {
                    action = Some(LinkDialogAction::Close);
                }
            });
        });
    if action.is_none() && modal.should_close() {
        action = Some(LinkDialogAction::Close);
    }
    action
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationDialogAction {
    Move,
    Cancel,
}

pub const MOVE_LABEL: &str = "Move here";

/// The confirmation before auto fix moves to this thread from the one that has it, named
/// `previous_label` (its title, or its ID when unknown).
pub fn destination_dialog_view(
    ctx: &egui::Context,
    previous_label: &str,
) -> Option<DestinationDialogAction> {
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::new("pull-request-destination-dialog"))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.5).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3("Move auto fix here?"));
            ui.label(format!(
                "This pull request's CI failures go to \u{201c}{previous_label}\u{201d}. Turning \
                 auto fix on here sends them to this thread instead."
            ));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if primary_button(ui, MOVE_LABEL).clicked() {
                    action = Some(DestinationDialogAction::Move);
                }
                if ui.add(ghost("Cancel")).clicked() {
                    action = Some(DestinationDialogAction::Cancel);
                }
            });
        });
    if action.is_none() && modal.should_close() {
        action = Some(DestinationDialogAction::Cancel);
    }
    action
}

#[cfg(test)]
mod tests;
