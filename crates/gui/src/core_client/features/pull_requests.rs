//! Desktop feature of the `pull_requests` area (PR 15, PR12 of the parity series): the pull
//! request of every thread the screen shows, as the core holds it, drawn with the embedded
//! workbench's bar, badge and dialogs, and the commands that change it (link, unlink,
//! refresh, candidates, the automatic fix policy and budget, the manual notice).
//!
//! The screen never runs `gh` and never opens the database. It sends commands
//! that quote the revisions it read, so a link or a policy change made from
//! another client in the meantime is refused by the core (`stale_revision`) and
//! the screen reads the thread again. A command whose outcome the client could
//! not see (the reply was lost, or the core says `outcome_unknown`) is never
//! shown as done: the operation is asked for, and one the core no longer knows
//! stays "unknown" until the thread is read again.
//!
//! As in the other areas an event only says something changed; the thread is
//! then asked for again. Whether the automatic fix budget allows a notice, and
//! whether the person is told of its limit, is the core's decision: this screen
//! shows `AutoFixLimitReached` as it arrives and decides nothing. Nor does it read
//! a locator: the core says whether the text names a pull request.
//!
//! Reads happen in `maintain`, whether or not the tab is shown: the selected thread first,
//! then the thread of the open "Link PR…" dialog, then the sidebar rows that asked, at most
//! [`MAX_IN_FLIGHT`] at a time.
use std::collections::{BTreeMap, BTreeSet};

mod dialogs;
pub(crate) mod view;

use super::layout::bounded_list;
use super::settings::SettingsIntent;
use super::{Availability, Feature, FeatureContext, FeatureId, FeatureReply, Visibility};
use crate::panes::pull_request_widgets::{
    self as widgets, BarChoice, BarMemory, BarView, CandidateChoice, PullRequestBadge, READ_FAILED,
    Status, candidate_offer, now_ns,
};
use crate::theme::icons;
use crate::theme::text::{h3, muted};
use crate::theme::tokens::{SP_2, SP_4, palette};
use crate::theme::widgets::{
    accessible, empty_state, primary_button, reason_label, soft_frame, spinner,
};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::{ErrorCode, RetryAdvice},
    events::UiEvent,
    handshake::Capability,
    ids::{OperationId, RunId, ThreadId},
    namespaces::pull_requests::{
        CandidateState, CandidateView, ConfirmCandidate, DismissCandidate,
        PullRequestOperationKind, PullRequestOperationView, PullRequestRef, PullRequestsCommand,
        PullRequestsEvent, PullRequestsQuery, PullRequestsReply, PullRequestsResult,
        RefreshPullRequest, RequestManualNotice, ResetAutoFixBudget, SetAutoFix, ThreadPullRequest,
        UnlinkPullRequest,
    },
    operation::{CancelReason, OperationState, OperationView},
    queries::{Query, QueryReply},
    wire::Revision,
};

/// Most thread reads in flight at once (the embedded workbench's bound).
pub const MAX_IN_FLIGHT: usize = 4;

#[derive(Debug, Clone)]
enum Pending {
    Thread(ThreadId),
    Operation(OperationId),
    Command(OperationId),
}

/// A control pressed on the bar of a thread's conversation, for this feature. `seen` is
/// what the bar drew from: a press is applied only while the thread still reads the same,
/// so a read landing before the next pass never turns it into another command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestBarAction {
    pub thread: ThreadId,
    pub choice: BarChoice,
    seen: BarSeen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BarSeen {
    lifecycle_revision: Revision,
    link_revision: Option<Revision>,
    policy_revision: Option<Revision>,
    head_sha: Option<String>,
}

impl BarSeen {
    fn of(view: &ThreadPullRequest) -> Self {
        Self {
            lifecycle_revision: view.lifecycle_revision,
            link_revision: view.link.as_ref().map(|link| link.link_revision),
            policy_revision: view.auto_fix.as_ref().map(|auto| auto.policy_revision),
            head_sha: view
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.facts.as_ref())
                .map(|facts| facts.head_sha.clone()),
        }
    }
}

/// The bar of the selected thread, as the conversation draws it.
pub struct PullRequestBar<'a> {
    pub thread: &'a ThreadId,
    pub view: BarView<'a>,
    seen: BarSeen,
}

impl PullRequestBar<'_> {
    pub fn action(&self, choice: BarChoice) -> PullRequestBarAction {
        PullRequestBarAction {
            thread: self.thread.clone(),
            choice,
            seen: self.seen.clone(),
        }
    }
}

/// "Link PR" or "Ignore" pressed on a suggestion in a tool card. `seen` is what the chip
/// drew from, as for the bar: a press is applied only while the thread still reads the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestChipAction {
    pub thread: ThreadId,
    pub candidate_id: u64,
    pub choice: CandidateChoice,
    seen: BarSeen,
}

/// The pull requests the tool calls of the selected thread suggested and the person has
/// neither linked nor ignored, to be offered on the cards of those calls.
#[derive(Clone)]
pub struct PullRequestChips<'a> {
    pub thread: &'a ThreadId,
    candidates: Vec<&'a CandidateView>,
    can_write: bool,
    seen: BarSeen,
}

impl PullRequestChips<'_> {
    /// Whether the offers may be pressed now.
    pub fn can_write(&self) -> bool {
        self.can_write
    }

    /// The suggestions of one call, as (candidate id, `owner/repo#number`). A call is named
    /// by its run and its id together: another run may use the same call id.
    pub fn offers(&self, run: &RunId, call_id: &str) -> Vec<(u64, String)> {
        self.candidates
            .iter()
            .filter(|c| &c.source_run_id == run && c.source_call_id == call_id)
            .map(|c| (c.candidate_id, view::label(&c.pull_request)))
            .collect()
    }

    pub fn action(&self, candidate_id: u64, choice: CandidateChoice) -> PullRequestChipAction {
        PullRequestChipAction {
            thread: self.thread.clone(),
            candidate_id,
            choice,
            seen: self.seen.clone(),
        }
    }
}

/// The command a press on a suggestion sends: it quotes the revision the suggestion was read at.
fn candidate_command(
    thread: &ThreadId,
    view: &ThreadPullRequest,
    candidate_id: u64,
    choice: CandidateChoice,
) -> PullRequestsCommand {
    match choice {
        CandidateChoice::Link => {
            PullRequestsCommand::ConfirmPullRequestCandidate(ConfirmCandidate {
                operation_id: new_operation(),
                thread: thread.clone(),
                candidate_id,
                expected_revision: view.lifecycle_revision,
            })
        }
        CandidateChoice::Ignore => {
            PullRequestsCommand::DismissPullRequestCandidate(DismissCandidate {
                operation_id: new_operation(),
                thread: thread.clone(),
                candidate_id,
            })
        }
    }
}

/// What the Projects tab asked of the pull requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullRequestIntent {
    /// A row of `thread` is shown: read its pull request for the badge.
    Want(ThreadId),
    /// "Link PR…" was chosen in the thread's menu.
    OpenLink(ThreadId),
    /// The settings changed what tracking is on: every thread is read again.
    Refresh,
}

/// What the Projects tab shows of the pull requests: the badges, the threads already read
/// (or being read), and whether "Link PR…" is offered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullRequestMenu {
    badges: BTreeMap<ThreadId, PullRequestBadge>,
    known: BTreeSet<ThreadId>,
    link_offered: bool,
}

impl PullRequestMenu {
    pub fn badge(&self, thread: &ThreadId) -> Option<PullRequestBadge> {
        self.badges.get(thread).copied()
    }

    pub fn is_known(&self, thread: &ThreadId) -> bool {
        self.known.contains(thread)
    }

    pub fn link_offered(&self) -> bool {
        self.link_offered
    }
}

#[derive(Default)]
struct Entry {
    view: Option<ThreadPullRequest>,
    stale: bool,
    in_flight: bool,
    /// Why the last read failed, until the thread is read again. A failed read is not
    /// retried on its own: a change, a reselection, a reconnect or "Link PR…" reads it again.
    error: Option<String>,
    memory: BarMemory,
    expanded: bool,
}

fn read_failed(entry: &Entry) -> Option<Status> {
    Some(Status::ReadFailed {
        message: entry.error.clone()?,
    })
}

impl Entry {
    fn wanted() -> Self {
        Self {
            stale: true,
            ..Self::default()
        }
    }
}

#[derive(Default)]
pub struct PullRequestsFeature {
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    selected: Option<ThreadId>,
    threads: BTreeMap<ThreadId, Entry>,
    /// The operations this screen started, running or finished.
    operations: BTreeMap<OperationId, PullRequestOperationView>,
    /// Operations whose end is to be asked for.
    to_check: BTreeSet<OperationId>,
    /// Operations the core no longer knows while their end was not seen: their
    /// result is unknown, and they no longer keep the screen busy.
    lost: BTreeSet<OperationId>,
    /// The outcome of the selected thread's last operation; operations of other threads
    /// never write it.
    status: String,
    /// What the core told the person about the selected thread (limit stops, merged, ...),
    /// newest last.
    notices: Vec<String>,
    link: Option<dialogs::LinkDialog>,
    /// A move of the notice destination the person has not confirmed yet.
    destination: Option<dialogs::DestinationMove>,
    bar_actions: Vec<PullRequestBarAction>,
    chip_actions: Vec<PullRequestChipAction>,
    intents: Vec<PullRequestIntent>,
}

const MAX_NOTICES: usize = 16;

fn new_operation() -> OperationId {
    OperationId::new(format!("gui-pr-{}", uuid::Uuid::new_v4())).expect("a uuid fits")
}

fn failure_label(code: ErrorCode, unknown: bool) -> &'static str {
    if unknown {
        return "Pull request: the outcome could not be confirmed. Read it again to check.";
    }
    match code {
        ErrorCode::StaleRevision => {
            "Pull request: it changed elsewhere. The latest state was read; try again."
        }
        ErrorCode::PermissionDenied => "Pull request: this core does not hold the thread",
        ErrorCode::InvalidRequest => "Pull request: not a pull request (URL or owner/repo#N)",
        ErrorCode::NotFound => "Pull request: not found",
        ErrorCode::Conflict => "Pull request: not possible in the current state",
        ErrorCode::UnsupportedCapability => "Pull request tracking is disabled",
        ErrorCode::Draining => "Pull request: the core is stopping or restarting",
        ErrorCode::LimitExceeded => limit_label(None),
        _ => "Pull request: the operation failed",
    }
}

/// A refusal for a limit, as the person reads it. The core says which limit by its
/// advice: `Never` is the memory of accepted commands being full, which only a
/// restart of the core empties; `UserActionRequired` is the table of running
/// operations being full, which clears as they finish. Neither is sent again by
/// the screen.
fn limit_label(retry: Option<RetryAdvice>) -> &'static str {
    match retry {
        Some(RetryAdvice::Never) => {
            "The core's memory of accepted operations is full, so it takes no new ones until it restarts. Nothing is sent again."
        }
        Some(RetryAdvice::UserActionRequired) => {
            "Too many operations are running. Try again once they finish; nothing is sent again automatically."
        }
        _ => "The operation was refused at a limit. Nothing is sent again.",
    }
}

fn success_label(kind: PullRequestOperationKind) -> &'static str {
    match kind {
        PullRequestOperationKind::Link | PullRequestOperationKind::ConfirmCandidate => {
            "Pull request linked."
        }
        PullRequestOperationKind::Unlink => "Pull request unlinked.",
        PullRequestOperationKind::Refresh => "",
        PullRequestOperationKind::DismissCandidate => "Suggestion ignored.",
        PullRequestOperationKind::SetAutoFix
        | PullRequestOperationKind::ResetAutoFixBudget
        | PullRequestOperationKind::ChangeAutoFixDestination => "Auto fix updated.",
        PullRequestOperationKind::RequestManualNotice => "Send to agent requested.",
    }
}

fn cancel_label(reason: CancelReason) -> &'static str {
    match reason {
        CancelReason::Requested => "Operation cancelled.",
        CancelReason::Restart | CancelReason::Shutdown => {
            "Not done: the core restarted or stopped."
        }
    }
}

/// The notification kinds the core names (the runtime's labels, kept here as words).
const MERGED: &str = "merged";
const CLOSED: &str = "closed";
const REOPENED: &str = "reopened";
const CHECKS_FAILED: &str = "checks_failed";
const CHECKS_PASSED: &str = "checks_passed";
const REVIEW_APPROVED: &str = "review_approved";
const REVIEW_CHANGES_REQUESTED: &str = "review_changes_requested";

fn notice_text(kind: &str, label: &str) -> String {
    match kind {
        CHECKS_FAILED => format!("CI failed on {label}"),
        MERGED => format!("{label} merged"),
        CLOSED => format!("{label} closed"),
        REOPENED => format!("{label} reopened"),
        CHECKS_PASSED => format!("CI passed on {label}"),
        REVIEW_APPROVED => format!("{label} approved"),
        REVIEW_CHANGES_REQUESTED => format!("Changes requested on {label}"),
        other => format!("{label}: {other}"),
    }
}

fn limit_notice(reference: &PullRequestRef, limit: u32) -> String {
    format!(
        "Auto fix stopped on {} after {limit} sends",
        view::label(reference)
    )
}

impl PullRequestsFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    fn running(&self) -> bool {
        self.operations
            .iter()
            .any(|(id, view)| self.is_running(id, view))
    }

    fn is_running(&self, id: &OperationId, view: &PullRequestOperationView) -> bool {
        !view.operation.state.is_final() && !self.lost.contains(id)
    }

    fn operation_running(&self, id: &OperationId) -> bool {
        self.operations
            .get(id)
            .is_some_and(|view| self.is_running(id, view))
    }

    fn view_of(&self, thread: &ThreadId) -> Option<&ThreadPullRequest> {
        self.threads.get(thread)?.view.as_ref()
    }

    fn want(&mut self, thread: ThreadId) {
        self.threads.entry(thread).or_insert_with(Entry::wanted);
    }

    fn mark_stale(&mut self, thread: &ThreadId) {
        if let Some(entry) = self.threads.get_mut(thread) {
            entry.stale = true;
        }
    }

    fn follow(&mut self, selected: Option<ThreadId>) {
        self.selected = selected.clone();
        self.status.clear();
        self.notices.clear();
        if let Some(thread) = selected {
            self.threads
                .entry(thread)
                .or_insert_with(Entry::wanted)
                .stale = true;
        }
    }

    /// Say an outcome on the tab, when it is about the thread the tab shows.
    fn say(&mut self, thread: &ThreadId, text: impl Into<String>) {
        if self.selected.as_ref() == Some(thread) {
            self.status = text.into();
        }
    }

    fn operation_thread(&self, id: &OperationId) -> Option<ThreadId> {
        self.operations.get(id).map(|view| view.thread.clone())
    }

    fn operation_lost(&mut self, id: &OperationId) {
        let Some(thread) = self.operation_thread(id) else {
            return;
        };
        self.lost.insert(id.clone());
        self.mark_stale(&thread);
        let text = failure_label(ErrorCode::Internal, true);
        self.say(&thread, text);
        self.link_note(id, text.to_owned());
    }

    fn link_note(&mut self, id: &OperationId, note: String) {
        if let Some(dialog) = &mut self.link
            && dialog.operation.as_ref() == Some(id)
        {
            dialog.note = Some(note);
        }
    }

    fn operation_update(&mut self, view: PullRequestOperationView) {
        let id = view.operation.operation_id.clone();
        let Some(known) = self.operations.get(&id) else {
            return;
        };
        // An end is final: an older reply arriving after the completion
        // (still `Pending`) does not make the screen busy again.
        if known.operation.state.is_final() && !view.operation.state.is_final() {
            return;
        }
        let state = view.operation.state;
        let kind = view.kind;
        let thread = view.thread.clone();
        let message = view.message.clone();
        let unknown = view.outcome_unknown;
        self.operations.insert(id.clone(), view);
        self.lost.remove(&id);
        match state {
            OperationState::Pending => {}
            OperationState::Ready => {
                self.say(&thread, success_label(kind));
                self.mark_stale(&thread);
                if self
                    .link
                    .as_ref()
                    .is_some_and(|dialog| dialog.operation.as_ref() == Some(&id))
                {
                    self.link = None;
                }
            }
            OperationState::Failed { code } => {
                let status = match &message {
                    Some(message) if code != ErrorCode::StaleRevision && !unknown => {
                        format!("{}: {message}", failure_label(code, false))
                    }
                    _ => failure_label(code, unknown).to_owned(),
                };
                self.say(&thread, status);
                let note = match message {
                    Some(message) if code == ErrorCode::InvalidRequest && !unknown => message,
                    _ => failure_label(code, unknown).to_owned(),
                };
                self.link_note(&id, note);
                self.mark_stale(&thread);
            }
            OperationState::Cancelled { reason } => {
                self.say(&thread, cancel_label(reason));
                self.link_note(&id, cancel_label(reason).to_owned());
                self.mark_stale(&thread);
            }
        }
    }

    fn notice(&mut self, text: String) {
        if self.notices.len() >= MAX_NOTICES {
            self.notices.remove(0);
        }
        self.notices.push(text);
    }

    fn send(&mut self, ctx: &mut FeatureContext<'_>, command: PullRequestsCommand) -> OperationId {
        let id = command.operation_id().clone();
        self.operations.insert(
            id.clone(),
            PullRequestOperationView {
                operation: OperationView {
                    operation_id: id.clone(),
                    state: OperationState::Pending,
                    revision: Revision(0),
                    result: None,
                },
                kind: command.kind(),
                thread: command.thread().clone(),
                lifecycle_revision: None,
                policy_revision: None,
                notice_id: None,
                message: None,
                outcome_unknown: false,
            },
        );
        let tag = self.tag(Pending::Command(id.clone()));
        ctx.command(tag, Command::PullRequests(command));
        id
    }

    /// The selected thread first, then the dialog's thread, then the rows that asked.
    fn read(&mut self, ctx: &mut FeatureContext<'_>) {
        let mut in_flight = self
            .threads
            .values()
            .filter(|entry| entry.in_flight)
            .count();
        let order: Vec<ThreadId> = self
            .selected
            .iter()
            .chain(self.link.as_ref().map(|dialog| &dialog.thread))
            .chain(self.threads.keys())
            .cloned()
            .collect();
        for thread in order {
            if in_flight >= MAX_IN_FLIGHT {
                break;
            }
            let Some(entry) = self.threads.get_mut(&thread) else {
                continue;
            };
            if !entry.stale || entry.in_flight {
                continue;
            }
            entry.stale = false;
            entry.in_flight = true;
            entry.error = None;
            in_flight += 1;
            let tag = self.tag(Pending::Thread(thread.clone()));
            ctx.query(
                tag,
                Query::PullRequests(PullRequestsQuery::GetThreadPullRequest { thread }),
            );
        }
    }

    fn apply_bar(&mut self, ctx: &mut FeatureContext<'_>, action: PullRequestBarAction) {
        let PullRequestBarAction {
            thread,
            choice,
            seen,
        } = action;
        let Some(entry) = self.threads.get_mut(&thread) else {
            return;
        };
        if choice == BarChoice::ToggleExpanded {
            entry.expanded = !entry.expanded;
            return;
        }
        let Some(view) = entry.view.clone().filter(|_| entry.error.is_none()) else {
            return;
        };
        if BarSeen::of(&view) != seen {
            self.say(&thread, failure_label(ErrorCode::StaleRevision, false));
            return;
        }
        if !view.owned_here || !view.tracking_enabled || view.link.is_none() || self.running() {
            return;
        }
        let controls = view::controls(&view, now_ns());
        let command = match choice {
            BarChoice::ToggleExpanded => None,
            BarChoice::Refresh => Some(PullRequestsCommand::RefreshPullRequest(
                RefreshPullRequest {
                    operation_id: new_operation(),
                    thread: thread.clone(),
                },
            )),
            BarChoice::Unlink => Some(PullRequestsCommand::UnlinkPullRequest(UnlinkPullRequest {
                operation_id: new_operation(),
                thread: thread.clone(),
                expected_revision: view.lifecycle_revision,
            })),
            BarChoice::ToggleAutoFix => controls
                .filter(|controls| controls.toggle.enabled)
                .and_then(|controls| {
                    if let Some(previous) =
                        controls.moves_from.clone().filter(|_| !controls.on_here)
                    {
                        self.ask_destination(
                            &thread,
                            &previous,
                            &controls,
                            dialogs::Change::TurnOn,
                        );
                        return None;
                    }
                    Some(PullRequestsCommand::SetAutoFix(SetAutoFix {
                        operation_id: new_operation(),
                        thread: thread.clone(),
                        link_revision: Revision(controls.link_revision),
                        policy_revision: Revision(controls.policy_revision),
                        enabled: !controls.on_here,
                    }))
                }),
            BarChoice::ResetCount => controls
                .filter(|controls| controls.reset.as_ref().is_some_and(|reset| reset.enabled))
                .and_then(|controls| {
                    if let Some(previous) = controls.moves_from.clone() {
                        self.ask_destination(
                            &thread,
                            &previous,
                            &controls,
                            dialogs::Change::ResetBudget,
                        );
                        return None;
                    }
                    Some(PullRequestsCommand::ResetAutoFixBudget(
                        ResetAutoFixBudget {
                            operation_id: new_operation(),
                            thread: thread.clone(),
                            link_revision: Revision(controls.link_revision),
                            policy_revision: Revision(controls.policy_revision),
                            confirmed_previous: None,
                        },
                    ))
                }),
            BarChoice::SendToAgent => {
                controls
                    .filter(|controls| controls.send.enabled)
                    .and_then(|controls| {
                        let expected_head = controls.head_sha?;
                        Some(PullRequestsCommand::RequestManualNotice(
                            RequestManualNotice {
                                operation_id: new_operation(),
                                thread: thread.clone(),
                                link_revision: Revision(controls.link_revision),
                                expected_head,
                            },
                        ))
                    })
            }
        };
        if let Some(command) = command {
            self.send(ctx, command);
        }
    }

    fn apply_chip(&mut self, ctx: &mut FeatureContext<'_>, action: PullRequestChipAction) {
        let PullRequestChipAction {
            thread,
            candidate_id,
            choice,
            seen,
        } = action;
        let Some(entry) = self.threads.get(&thread) else {
            return;
        };
        let Some(view) = entry.view.clone().filter(|_| entry.error.is_none()) else {
            return;
        };
        if BarSeen::of(&view) != seen {
            self.say(&thread, failure_label(ErrorCode::StaleRevision, false));
            return;
        }
        let offered = view
            .candidates
            .iter()
            .any(|c| c.candidate_id == candidate_id && c.state == CandidateState::Presented);
        if offered && view.owned_here && view.tracking_enabled && !self.running() {
            self.send(ctx, candidate_command(&thread, &view, candidate_id, choice));
        }
    }

    /// Queue what was pressed on a suggestion in a tool card; it is applied at the next pass.
    pub fn enqueue_chip(&mut self, action: PullRequestChipAction) {
        self.chip_actions.push(action);
    }

    /// The suggestions of the selected thread that are still open.
    pub fn chips(&self) -> Option<PullRequestChips<'_>> {
        let thread = self.selected.as_ref()?;
        let entry = self.threads.get(thread)?;
        let view = entry.view.as_ref().filter(|view| view.tracking_enabled)?;
        let candidates: Vec<_> = view
            .candidates
            .iter()
            .filter(|candidate| candidate.state == CandidateState::Presented)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        Some(PullRequestChips {
            thread,
            candidates,
            can_write: view.owned_here && !self.running() && entry.error.is_none(),
            seen: BarSeen::of(view),
        })
    }

    pub fn menu(&self) -> PullRequestMenu {
        let mut badges = BTreeMap::new();
        let mut tracking_off = false;
        for (thread, entry) in &self.threads {
            let Some(view) = &entry.view else {
                continue;
            };
            if !view.tracking_enabled {
                tracking_off = true;
                continue;
            }
            if let Some(badge) = view::badge(view) {
                badges.insert(thread.clone(), badge);
            }
        }
        PullRequestMenu {
            badges,
            known: self.threads.keys().cloned().collect(),
            link_offered: !tracking_off,
        }
    }

    /// Queue what a thread's menu or row asked for; it is applied at the next pass.
    pub fn enqueue_sidebar(&mut self, intent: PullRequestIntent) {
        self.intents.push(intent);
    }

    /// Queue what was pressed on the bar; it is applied at the next pass. Returns whether
    /// it opens the confirmation of moving auto fix, which the Pull requests tab draws.
    pub fn enqueue_bar(&mut self, action: PullRequestBarAction) -> bool {
        let confirms = match action.choice {
            BarChoice::ToggleAutoFix | BarChoice::ResetCount => self
                .view_of(&action.thread)
                .filter(|view| BarSeen::of(view) == action.seen)
                .and_then(|view| view::controls(view, now_ns()))
                .is_some_and(|controls| {
                    controls.moves_from.is_some()
                        && (action.choice == BarChoice::ResetCount || !controls.on_here)
                }),
            _ => false,
        };
        self.bar_actions.push(action);
        confirms
    }

    /// The bar of the selected thread, while it has a linked pull request.
    pub fn bar(&self) -> Option<PullRequestBar<'_>> {
        let thread = self.selected.as_ref()?;
        let entry = self.threads.get(thread)?;
        let view = entry.view.as_ref().filter(|view| view.tracking_enabled)?;
        let mut bar = view::bar_view(
            view,
            view::BarShape {
                expanded: entry.expanded,
                collapsible: true,
                actions_enabled: view.owned_here && !self.running() && entry.error.is_none(),
                now_ns: now_ns(),
            },
        )?;
        bar.statuses.extend(read_failed(entry));
        Some(PullRequestBar {
            thread,
            view: bar,
            seen: BarSeen::of(view),
        })
    }

    fn draw_tab(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let Some(thread) = self.selected.clone() else {
            empty_state(
                ui,
                "No thread selected",
                "Pick a thread to see its pull request.",
                None,
            );
            return;
        };
        if !self.status.is_empty() {
            reason_label(ui, palette().TEXT_MUTED, &self.status);
        }
        for notice in &self.notices {
            ui.colored_label(palette().WARNING_FG, notice);
        }
        let Some(view) = self.view_of(&thread).cloned() else {
            match self
                .threads
                .get(&thread)
                .and_then(|entry| entry.error.as_ref())
            {
                Some(error) => {
                    reason_label(ui, palette().ERROR_FG, error);
                }
                None => {
                    ui.horizontal(|ui| {
                        spinner(ui);
                        ui.label(muted("Reading the pull request…"));
                    });
                }
            }
            return;
        };
        if !view.tracking_enabled {
            tracking_off(ui, ctx);
            return;
        }
        let busy = self.running();
        if busy {
            ui.label(muted("Working…"));
        }
        let read_error = self.threads.get(&thread).and_then(read_failed);
        if let Some(Status::ReadFailed { message }) = &read_error {
            reason_label(
                ui,
                palette().ERROR_FG,
                format!("{READ_FAILED}: {message}. What is shown may be out of date."),
            );
        }
        let can_write = view.owned_here && !busy && read_error.is_none();
        let shape = view::BarShape {
            expanded: true,
            collapsible: false,
            actions_enabled: can_write,
            now_ns: now_ns(),
        };
        let seen = BarSeen::of(&view);
        match view::bar_view(&view, shape) {
            Some(mut bar) => {
                bar.statuses.extend(read_error);
                let details = ui.available_height() * 0.6;
                let choice = soft_frame(palette().SURFACE)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        widgets::bar_inline(ui, &bar, details)
                    })
                    .inner;
                if let Some(choice) = choice.filter(|choice| *choice != BarChoice::ToggleExpanded) {
                    self.apply_bar(
                        ctx,
                        PullRequestBarAction {
                            thread: thread.clone(),
                            choice,
                            seen,
                        },
                    );
                }
            }
            None => {
                ui.label("No pull request is linked to this thread.");
                if !view.owned_here {
                    ui.label(muted("Not tracked in this window"))
                        .on_hover_text("Another window polls this pull request, or none does");
                }
                let link = ui
                    .add_enabled_ui(can_write, |ui| primary_button(ui, "Link PR…"))
                    .inner;
                if link.clicked() && can_write {
                    self.open_link(thread.clone());
                }
            }
        }
        self.draw_candidates(ui, ctx, &thread, &view, can_write);
    }

    fn draw_candidates(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut FeatureContext<'_>,
        thread: &ThreadId,
        view: &ThreadPullRequest,
        can_write: bool,
    ) {
        let candidates: Vec<_> = view
            .candidates
            .iter()
            .filter(|candidate| candidate.state == CandidateState::Presented)
            .collect();
        if candidates.is_empty() {
            return;
        }
        ui.separator();
        ui.label(h3("Suggested pull requests"));
        let mut commands = Vec::new();
        bounded_list(ui, "pull-request-candidates", |ui| {
            for candidate in candidates {
                let label = view::label(&candidate.pull_request);
                let choice = ui
                    .push_id(candidate.candidate_id, |ui| {
                        ui.horizontal_wrapped(|ui| candidate_offer(ui, &label, can_write))
                            .inner
                    })
                    .inner;
                commands.extend(
                    choice.map(|choice| {
                        candidate_command(thread, view, candidate.candidate_id, choice)
                    }),
                );
            }
        });
        if can_write {
            for command in commands {
                self.send(ctx, command);
            }
        }
    }
}

const OPEN_FEATURES: &str = "Open Features";

/// What the tab says while tracking is off: where to turn it on, and the button that opens it.
fn tracking_off(ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
    ui.vertical_centered(|ui| {
        ui.add_space(SP_4);
        ui.label(h3("Pull request view is off"));
        ui.label(muted(
            "Turn it on in \u{2699} \u{203a} Features to track this thread's pull request.",
        ));
        ui.add_space(SP_2);
        let open = ui.button(icons::with_icon(icons::SLIDERS_HORIZONTAL, OPEN_FEATURES));
        accessible(&open, OPEN_FEATURES);
        if open.clicked() {
            ctx.settings(SettingsIntent::Features);
        }
    });
}

impl Feature for PullRequestsFeature {
    fn id(&self) -> FeatureId {
        FeatureId::PullRequests
    }

    fn title(&self) -> &'static str {
        "Pull requests"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Commands, Capability::Queries],
        }
    }

    fn as_pull_requests(&self) -> Option<&PullRequestsFeature> {
        Some(self)
    }

    fn as_pull_requests_mut(&mut self) -> Option<&mut PullRequestsFeature> {
        Some(self)
    }

    fn reset(&mut self) {
        // What the core said is read again; only the threads the screen still shows are.
        let keep: BTreeSet<ThreadId> = self
            .selected
            .iter()
            .chain(self.link.as_ref().map(|dialog| &dialog.thread))
            .chain(self.destination.as_ref().map(|change| &change.thread))
            .cloned()
            .collect();
        self.threads.retain(|thread, _| keep.contains(thread));
        for entry in self.threads.values_mut() {
            entry.view = None;
            entry.in_flight = false;
            entry.stale = true;
            entry.error = None;
        }
        self.pending
            .retain(|_, pending| matches!(pending, Pending::Command(_)));
        for (id, view) in &self.operations {
            if !view.operation.state.is_final() && !self.lost.contains(id) {
                self.to_check.insert(id.clone());
            }
        }
    }

    fn event(&mut self, event: &UiEvent) {
        let UiEvent::PullRequests(event) = event else {
            return;
        };
        match event {
            PullRequestsEvent::PullRequestChanged {
                thread,
                pull_request,
                notifications,
                ..
            } => {
                self.mark_stale(thread);
                if self.selected.as_ref() == Some(thread) {
                    let label = view::label(pull_request);
                    for kind in notifications {
                        self.notice(notice_text(kind, &label));
                    }
                }
            }
            PullRequestsEvent::AutoFixLimitReached {
                thread,
                pull_request,
                limit,
                ..
            } => {
                self.mark_stale(thread);
                if self.selected.as_ref() == Some(thread) {
                    self.notice(limit_notice(pull_request, *limit));
                }
            }
            PullRequestsEvent::PullRequestOperationCompleted { operation } => {
                self.operation_update(operation.clone());
            }
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        match (pending, reply) {
            (Pending::Thread(thread), FeatureReply::Query(result)) => {
                let Some(entry) = self.threads.get_mut(&thread) else {
                    return;
                };
                entry.in_flight = false;
                match result {
                    Ok(QueryReply::PullRequests(PullRequestsReply::Thread(view)))
                        if view.thread == thread =>
                    {
                        let link_revision = view.link.as_ref().map(|link| link.link_revision.get());
                        if entry.memory.observe(link_revision, view::glyph(&view)) {
                            entry.expanded = true;
                        }
                        if link_revision.is_none() {
                            entry.expanded = false;
                        }
                        entry.view = Some(*view);
                    }
                    Ok(_) => {}
                    // Said where the thread's pull request is awaited: its tab, its bar and
                    // its "Link PR…" dialog. A view read before stays shown, marked out of
                    // date, and nothing is done from it until a read succeeds.
                    Err(error) => entry.error = Some(error.message),
                }
            }
            (Pending::Operation(id), FeatureReply::Query(result)) => match result {
                Ok(QueryReply::PullRequests(PullRequestsReply::Operation(view))) => {
                    self.operation_update(view);
                }
                Ok(_) => {}
                Err(error) if error.code == Some(ErrorCode::NotFound) => self.operation_lost(&id),
                Err(error) => {
                    if let Some(thread) = self.operation_thread(&id) {
                        self.say(&thread, error.message);
                    }
                }
            },
            (Pending::Command(id), FeatureReply::Command(result)) => match result {
                Ok(reply) => {
                    if let Some(CommandResult::PullRequests(PullRequestsResult::Operation(view))) =
                        reply.result.map(|boxed| *boxed)
                    {
                        self.operation_update(view);
                    }
                }
                Err(error) => {
                    if error.outcome_unknown {
                        // The command may have been applied: its operation is
                        // asked for, never sent again under a new id and never
                        // shown as done.
                        let text = failure_label(ErrorCode::Internal, true);
                        if let Some(thread) = self.operation_thread(&id) {
                            self.say(&thread, text);
                        }
                        self.link_note(&id, text.to_owned());
                        self.to_check.insert(id);
                    } else {
                        let text = match error.code {
                            Some(ErrorCode::LimitExceeded) => limit_label(error.retry).to_owned(),
                            Some(ErrorCode::InvalidRequest) if !error.message.is_empty() => {
                                error.message.clone()
                            }
                            Some(code) => failure_label(code, false).to_owned(),
                            None => error.message.clone(),
                        };
                        self.link_note(&id, text.clone());
                        if let Some(view) = self.operations.remove(&id) {
                            self.say(&view.thread, text);
                            self.mark_stale(&view.thread);
                        }
                        if let Some(dialog) = &mut self.link
                            && dialog.operation.as_ref() == Some(&id)
                        {
                            dialog.operation = None;
                        }
                    }
                }
            },
            _ => {}
        }
    }

    fn maintain(&mut self, ctx: &mut FeatureContext<'_>, visibility: Visibility) {
        let selected = ctx.selection().thread.clone();
        if selected != self.selected {
            self.follow(selected);
        }
        if !visibility.available {
            self.intents.clear();
            self.bar_actions.clear();
            self.chip_actions.clear();
            return;
        }
        for intent in std::mem::take(&mut self.intents) {
            match intent {
                PullRequestIntent::Want(thread) => self.want(thread),
                PullRequestIntent::OpenLink(thread) => self.open_link(thread),
                PullRequestIntent::Refresh => self
                    .threads
                    .values_mut()
                    .for_each(|entry| entry.stale = true),
            }
        }
        for action in std::mem::take(&mut self.bar_actions) {
            self.apply_bar(ctx, action);
        }
        for action in std::mem::take(&mut self.chip_actions) {
            self.apply_chip(ctx, action);
        }
        self.read(ctx);
        for id in std::mem::take(&mut self.to_check) {
            let tag = self.tag(Pending::Operation(id.clone()));
            ctx.query(
                tag,
                Query::PullRequests(PullRequestsQuery::GetPullRequestOperation {
                    operation_id: id,
                }),
            );
        }
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        self.draw_tab(ui, ctx);
        self.draw_destination(ui.ctx(), ctx);
        self.draw_link(ui.ctx(), ctx);
    }
}

#[cfg(test)]
mod tests;
