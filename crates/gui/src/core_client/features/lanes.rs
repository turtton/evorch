//! Desktop feature of the `lanes` area: the lanes the selected thread leads,
//! one lane opened for its detail, and the human operations on a lane: set its
//! policy, detach it, reattach it, deliver again the notices held for its
//! lead, and end it by a person's decision (close it as done, unverified, or
//! discard it; see `close`).
//!
//! The lane ledger is the truth. Lists and details are read from the core; an
//! event only names a lane that changed and the reads are repeated, so a late
//! event never moves the screen back. Every operation names the management
//! revision the screen showed, and the lead's current root run where one is
//! needed; the core refuses an older one. Continuing a lane has no gate here.
//!
//! Besides its tab, the feature serves two places outside it: the lane items of a lane
//! thread's menu in the Projects tab ([`LanesFeature::menu`], [`LaneIntent`]) and the lane
//! card of a lane thread's conversation ([`LanesFeature::card`], [`LaneCardAction`]). Both
//! read through a directory of the lanes the core listed or announced, by lane thread. The
//! tab's lists are read only while the tab is shown; what the menu and the card need is read
//! whenever they ask.
use std::collections::{BTreeMap, BTreeSet};

mod card;
mod close;

pub use crate::panes::lane_widgets::CloseChoice;
pub use card::{CardChoice, CardPlace, LaneCardView, lane_body};

use super::layout::bounded_list;
use super::{
    Availability, Feature, FeatureContext, FeatureError, FeatureId, FeatureReply, Navigation,
    Visibility,
};
use crate::panes::lane_widgets::{
    MEANING, STALE_NOTICE, already_closed_notice, closed_notice, not_recorded, policy_badge,
    policy_word, status_badge,
};
use crate::theme::text::{h3, medium, muted};
use crate::theme::tokens::palette;
use crate::theme::widgets::{empty_state, ghost, soft_frame, surface_frame};
use workbench_protocol::{
    commands::{Command, CommandResult},
    errors::ErrorCode,
    events::UiEvent,
    handshake::Capability,
    ids::{PageCursor, RunId, ThreadId},
    lane_attention::LaneId,
    lane_management::{
        LaneCloseRecordKind, LaneFamilyRevision, LaneManagementAction, LaneManagementOperationId,
        LaneManagementPolicy, LaneManagementRequest, LaneManagementState, ManageLane,
        ManagedLanePolicy,
    },
    namespaces::{
        ExecutionQuery, ExecutionReply, LanesCommand, LanesEvent, LanesQuery, LanesReply,
        LanesResult,
        execution::{RunDetailsQuery, RunScope},
        lanes::{LaneDetail, LaneStartState, LaneSummary, ListLanes, ResumeLaneDeliveries},
    },
    queries::{PageRequest, Query, QueryReply},
    views::Page,
    wire::Revision,
};

#[derive(Debug, Clone)]
enum Pending {
    List {
        thread: ThreadId,
        append: bool,
    },
    Detail {
        lane: LaneId,
    },
    /// The lead's root runs, for the tab (`card: false`) or the lane card.
    LeadRoot {
        thread: ThreadId,
        card: bool,
    },
    Detached {
        thread: ThreadId,
        append: bool,
    },
    Management {
        lane: LaneId,
    },
    Command(&'static str),
    /// A lane operation chosen from a thread menu in the Projects tab: its outcome is shown
    /// there, not in the Lanes tab.
    SidebarCommand(&'static str),
    /// The read of the close preview the open dialog waits for (its `read_seq`).
    ClosePreview {
        generation: u64,
        seq: u64,
    },
    /// A close or discard that was sent, for the dialog of `generation`; at most one is in
    /// flight.
    CloseCommand {
        generation: u64,
    },
    /// A page of the lanes `lead` manages, read to find the lanes a menu or card asked for.
    Lookup {
        lead: ThreadId,
    },
    CardDetail {
        thread: ThreadId,
    },
    CardCommand {
        thread: ThreadId,
        label: &'static str,
    },
}

/// A reattach waits for the lane's management state (its family revision) and
/// the lead's current root run before it is sent.
struct Reattach {
    lane: LaneId,
    state: Option<LaneManagementState>,
}

/// A lead's current root run, read page by page: the last root listed is the current one.
#[derive(Default)]
struct LeadRoot {
    thread: Option<ThreadId>,
    root: Option<RunId>,
    next: Option<PageCursor>,
    stale: bool,
}

impl LeadRoot {
    fn follow(&mut self, thread: Option<ThreadId>) {
        if self.thread != thread {
            *self = Self {
                stale: thread.is_some(),
                thread,
                ..Self::default()
            };
        }
    }

    fn invalidate(&mut self) {
        self.stale = self.thread.is_some();
    }

    fn due(&mut self) -> Option<(ThreadId, Option<PageCursor>)> {
        let thread = self.thread.clone()?;
        if !self.stale && self.next.is_none() {
            return None;
        }
        let cursor = if self.stale { None } else { self.next.take() };
        self.stale = false;
        Some((thread, cursor))
    }

    fn answer(
        &mut self,
        thread: &ThreadId,
        page: Page<workbench_protocol::namespaces::execution::RunDetail>,
    ) {
        if self.thread.as_ref() != Some(thread) {
            return;
        }
        if let Some(last) = page.items.last() {
            self.root = Some(last.summary.id.clone());
        }
        self.next = page.next_cursor;
    }
}

/// What a lane thread's menu or card asked to find among the lanes of one lead.
#[derive(Default)]
struct Lookup {
    wanted: BTreeSet<ThreadId>,
    cursor: Option<PageCursor>,
    due: bool,
}

/// The lane card of the selected thread, when it is a lane.
struct Card {
    thread: ThreadId,
    lead: ThreadId,
    detail: Option<LaneDetail>,
    detail_stale: bool,
    root: LeadRoot,
    status: String,
}

impl Card {
    fn new(thread: ThreadId, lead: ThreadId) -> Self {
        let mut root = LeadRoot::default();
        root.follow(Some(lead.clone()));
        Self {
            thread,
            lead,
            detail: None,
            detail_stale: true,
            root,
            status: String::new(),
        }
    }
}

/// What the Projects tab's thread menu asked of the lanes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneIntent {
    /// Find the lane of `thread`, a lane of `lead`: its menu is open, or its row is on screen
    /// (for the badge of a lane whose close stands).
    Lookup { thread: ThreadId, lead: ThreadId },
    /// The core refused to read the lane of `thread`; the user asked to read it again.
    Retry { thread: ThreadId, lead: ThreadId },
    /// Change the lane's management, naming the revision the menu showed.
    Manage {
        lane: LaneId,
        revision: Revision,
        action: LaneManagementAction,
    },
    /// Open the confirmation for ending the lane; `title` names it as the menu did.
    Close {
        lane: LaneId,
        title: String,
        choice: CloseChoice,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneCardAction {
    ResumeNotifications { lane: LaneId },
}

/// How a lane thread's menu reads its lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneMenuEntry {
    /// Its lead manages it: the menu offers the lane's operations at this revision.
    Managed { lane: LaneId, revision: Revision },
    /// The core lists no lane its lead manages for the thread.
    NotManaged,
    /// The core refused the read that looks the lane up; it is read again only when asked.
    Unreadable,
}

/// The lanes the menus can offer operations for, by lane thread. A thread not in it has not
/// been read yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaneMenu {
    entries: BTreeMap<ThreadId, LaneMenuEntry>,
    /// How the close of each managed lane that has one stands.
    closes: BTreeMap<ThreadId, LaneCloseRecordKind>,
    /// Lane threads whose lookup was asked for and is not answered yet.
    requested: BTreeSet<ThreadId>,
    /// Why the last lane operation chosen from a menu was not applied.
    notice: Option<String>,
}

impl LaneMenu {
    pub fn entry(&self, thread: &ThreadId) -> Option<&LaneMenuEntry> {
        self.entries.get(thread)
    }

    /// How the ledger's close of the thread's lane stands, if the lane was read and it has one.
    pub fn current_close(&self, thread: &ThreadId) -> Option<LaneCloseRecordKind> {
        self.closes.get(thread).copied()
    }

    /// Whether the thread's lane was read, or is being read: nothing more is to be asked.
    pub fn is_known(&self, thread: &ThreadId) -> bool {
        self.entries.contains_key(thread) || self.requested.contains(thread)
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }
}

#[derive(Default)]
pub struct LanesFeature {
    next_tag: u64,
    pending: BTreeMap<u64, Pending>,
    thread: Option<ThreadId>,
    lanes: Vec<LaneSummary>,
    next_cursor: Option<PageCursor>,
    opened: Option<LaneId>,
    detail: Option<LaneDetail>,
    /// The tab lead's latest root run, as the core last listed it.
    lead_root: LeadRoot,
    /// Lanes opened from this thread that no lead manages now, as the core lists them. A
    /// detached lane is not in the lead's list, and is offered here for reattaching.
    detached: Vec<LaneSummary>,
    detached_next: Option<PageCursor>,
    /// Pages of detached lanes loaded now, and how many the screen shows or has asked for: a
    /// refresh re-reads the pages the user had loaded, instead of dropping all but the first.
    detached_pages: usize,
    detached_target_pages: usize,
    reattach: Option<Reattach>,
    /// The confirmation for closing or discarding a lane, while it is open.
    close: Option<close::CloseDialog>,
    /// The generation the next dialog opens with.
    close_generation: u64,
    status: String,
    list_stale: bool,
    detached_stale: bool,
    detail_stale: bool,
    more_requested: bool,
    detached_more_requested: bool,
    /// Every lane the core listed or announced, by its thread.
    directory: BTreeMap<ThreadId, LaneSummary>,
    /// Lane threads a lookup did not find under their lead.
    missing: BTreeSet<ThreadId>,
    /// Lane threads whose lookup the core refused.
    unreadable: BTreeSet<ThreadId>,
    /// The outcome of the last lane operation chosen from a menu, while it was not applied.
    sidebar_notice: Option<String>,
    lookups: BTreeMap<ThreadId, Lookup>,
    card: Option<Card>,
    intents: Vec<LaneIntent>,
    card_actions: Vec<LaneCardAction>,
}

impl LanesFeature {
    fn tag(&mut self, pending: Pending) -> u64 {
        self.next_tag += 1;
        self.pending.insert(self.next_tag, pending);
        self.next_tag
    }

    fn switch_thread(&mut self, thread: Option<ThreadId>) {
        self.thread = thread;
        self.lanes.clear();
        self.next_cursor = None;
        self.opened = None;
        self.detail = None;
        self.lead_root = LeadRoot::default();
        self.lead_root.follow(self.thread.clone());
        self.detached.clear();
        self.detached_next = None;
        self.detached_pages = 0;
        self.detached_target_pages = 0;
        self.reattach = None;
        self.close = None;
        self.status.clear();
        self.list_stale = self.thread.is_some();
        self.detached_stale = self.thread.is_some();
    }

    /// The lead's current root has been read: no root query is in flight or due.
    fn root_settled(&self) -> bool {
        !self.lead_root.stale
            && self.lead_root.next.is_none()
            && !self
                .pending
                .values()
                .any(|pending| matches!(pending, Pending::LeadRoot { card: false, .. }))
    }

    fn command(&mut self, ctx: &mut FeatureContext<'_>, label: &'static str, command: Command) {
        let tag = self.tag(Pending::Command(label));
        ctx.command(tag, command);
    }

    fn manage(
        &mut self,
        ctx: &mut FeatureContext<'_>,
        pending: Pending,
        lane: &LaneId,
        revision: Revision,
        action: LaneManagementAction,
        family: Option<LaneFamilyRevision>,
    ) {
        let operation = format!("gui-lane-{}", uuid::Uuid::new_v4());
        let Ok(operation_id) = LaneManagementOperationId::new(operation) else {
            return;
        };
        let tag = self.tag(pending);
        ctx.command(
            tag,
            Command::Lanes(LanesCommand::ManageLane(ManageLane {
                request: LaneManagementRequest {
                    operation_id,
                    lane_id: lane.clone(),
                    expected_management_revision: revision,
                    action,
                },
                expected_family_revision: family,
            })),
        );
    }

    fn refresh_after_command(&mut self) {
        self.list_stale = true;
        self.detached_stale = true;
        self.detail_stale = self.opened.is_some();
        self.lead_root.invalidate();
    }

    fn open_close(&mut self, lane: LaneId, choice: CloseChoice, title: String) {
        self.close_generation += 1;
        self.close = Some(close::CloseDialog::new(
            self.close_generation,
            lane,
            choice,
            title,
        ));
    }

    pub fn menu(&self) -> LaneMenu {
        let mut entries: BTreeMap<ThreadId, LaneMenuEntry> = self
            .missing
            .iter()
            .map(|thread| (thread.clone(), LaneMenuEntry::NotManaged))
            .chain(
                self.unreadable
                    .iter()
                    .map(|thread| (thread.clone(), LaneMenuEntry::Unreadable)),
            )
            .collect();
        let mut closes = BTreeMap::new();
        for (thread, lane) in &self.directory {
            let entry =
                if lane.policy == LaneManagementPolicy::Detached || lane.lead_thread_id.is_none() {
                    LaneMenuEntry::NotManaged
                } else {
                    if let Some(close) = &lane.current_close {
                        closes.insert(thread.clone(), close.kind);
                    }
                    LaneMenuEntry::Managed {
                        lane: lane.lane_id.clone(),
                        revision: lane.management_revision,
                    }
                };
            entries.insert(thread.clone(), entry);
        }
        // What was asked for waits in a lookup or, until the next pass, in the queue: a row
        // must not ask again meanwhile.
        let requested = self
            .lookups
            .values()
            .flat_map(|lookup| lookup.wanted.iter().cloned())
            .chain(self.intents.iter().filter_map(|intent| match intent {
                LaneIntent::Lookup { thread, .. } | LaneIntent::Retry { thread, .. } => {
                    Some(thread.clone())
                }
                _ => None,
            }))
            .collect();
        LaneMenu {
            entries,
            closes,
            requested,
            notice: self.sidebar_notice.clone(),
        }
    }

    /// Queue what a thread menu asked for; it is applied at the next pass.
    pub fn enqueue_sidebar(&mut self, intent: LaneIntent) {
        self.intents.push(intent);
    }

    /// How many asks of the menu wait for the next pass.
    #[cfg(test)]
    pub(crate) fn queued_sidebar_intents(&self) -> usize {
        self.intents.len()
    }

    /// Queue what was pressed on the lane card; it is sent at the next pass.
    pub fn enqueue_card(&mut self, action: LaneCardAction) {
        self.card_actions.push(action);
    }

    /// The card of the selected thread, once its lane was found.
    pub fn card(&self) -> Option<(&ThreadId, LaneCardView<'_>)> {
        let card = self.card.as_ref()?;
        let summary = self.directory.get(&card.thread)?;
        let detail = card
            .detail
            .as_ref()
            .filter(|detail| detail.summary.lane_id == summary.lane_id);
        Some((
            &card.thread,
            LaneCardView {
                lead: &card.lead,
                summary,
                detail,
                can_resume: card.root.root.is_some(),
                status: &card.status,
            },
        ))
    }

    fn lookup(&mut self, thread: ThreadId, lead: ThreadId) {
        if self.directory.contains_key(&thread)
            || self.missing.contains(&thread)
            || self.unreadable.contains(&thread)
        {
            return;
        }
        let in_flight = self
            .pending
            .values()
            .any(|pending| matches!(pending, Pending::Lookup { lead: l } if *l == lead));
        let lookup = self.lookups.entry(lead).or_default();
        if lookup.wanted.insert(thread) && !in_flight && !lookup.due {
            lookup.cursor = None;
            lookup.due = true;
        }
    }

    fn apply_intent(&mut self, ctx: &mut FeatureContext<'_>, intent: LaneIntent) {
        match intent {
            LaneIntent::Lookup { thread, lead } => self.lookup(thread, lead),
            LaneIntent::Retry { thread, lead } => {
                self.unreadable.remove(&thread);
                self.lookup(thread, lead);
            }
            LaneIntent::Manage {
                lane,
                revision,
                action,
            } => {
                let label = match action {
                    LaneManagementAction::Detach => "Detach lane",
                    _ => "Lane policy",
                };
                self.sidebar_notice = None;
                self.manage(
                    ctx,
                    Pending::SidebarCommand(label),
                    &lane,
                    revision,
                    action,
                    None,
                );
            }
            LaneIntent::Close {
                lane,
                title,
                choice,
            } => self.open_close(lane, choice, title),
        }
    }

    fn apply_card_action(&mut self, ctx: &mut FeatureContext<'_>, action: LaneCardAction) {
        let LaneCardAction::ResumeNotifications { lane } = action;
        let Some(card) = &self.card else {
            return;
        };
        let (Some(detail), Some(root)) = (&card.detail, card.root.root.clone()) else {
            return;
        };
        if detail.summary.lane_id != lane {
            return;
        }
        let command = Command::Lanes(LanesCommand::ResumeLaneDeliveries(ResumeLaneDeliveries {
            lane_id: lane,
            expected_management_revision: detail.summary.management_revision,
            lead_thread_id: card.lead.clone(),
            expected_lead_root_run_id: root,
        }));
        let thread = card.thread.clone();
        let tag = self.tag(Pending::CardCommand {
            thread,
            label: "Resume notifications",
        });
        ctx.command(tag, command);
    }

    /// The card follows the selected thread when the core says it is a lane.
    fn follow_card(&mut self, ctx: &FeatureContext<'_>) {
        let selected = ctx.selection().thread.clone();
        let lead = selected.as_ref().and_then(|thread| {
            ctx.cache()
                .threads
                .get(thread.as_str())
                .and_then(|summary| summary.lead_thread.clone())
        });
        match (selected, lead) {
            (Some(thread), Some(lead)) => {
                if self
                    .card
                    .as_ref()
                    .is_none_or(|card| card.thread != thread || card.lead != lead)
                {
                    // Selecting the thread again reads a lane the core refused before.
                    self.unreadable.remove(&thread);
                    self.card = Some(Card::new(thread, lead));
                }
            }
            _ => self.card = None,
        }
    }

    fn read_lookups(&mut self, ctx: &mut FeatureContext<'_>) {
        let due: Vec<(ThreadId, Option<PageCursor>)> = self
            .lookups
            .iter_mut()
            .filter(|(_, lookup)| lookup.due)
            .map(|(lead, lookup)| {
                lookup.due = false;
                (lead.clone(), lookup.cursor.take())
            })
            .collect();
        for (lead, cursor) in due {
            let tag = self.tag(Pending::Lookup { lead: lead.clone() });
            ctx.query(tag, list_query(lead, false, cursor));
        }
    }

    fn read_card(&mut self, ctx: &mut FeatureContext<'_>) {
        let Some(card) = &self.card else {
            return;
        };
        let (thread, lead) = (card.thread.clone(), card.lead.clone());
        match self.directory.get(&thread).map(|lane| lane.lane_id.clone()) {
            Some(lane) => {
                let in_flight = self.pending.values().any(
                    |pending| matches!(pending, Pending::CardDetail { thread: t } if *t == thread),
                );
                if let Some(card) = self.card.as_mut().filter(|card| card.detail_stale)
                    && !in_flight
                {
                    card.detail_stale = false;
                    let tag = self.tag(Pending::CardDetail { thread });
                    ctx.query(tag, Query::Lanes(LanesQuery::GetLane { lane_id: lane }));
                }
            }
            None => self.lookup(thread, lead),
        }
        if let Some((lead, cursor)) = self.card.as_mut().and_then(|card| card.root.due()) {
            let tag = self.tag(Pending::LeadRoot {
                thread: lead.clone(),
                card: true,
            });
            ctx.query(tag, roots_query(lead, cursor));
        }
    }

    fn read_close(&mut self, ctx: &mut FeatureContext<'_>) {
        if let Some(dialog) = &mut self.close
            && dialog.read_due
        {
            dialog.read_due = false;
            dialog.read_seq += 1;
            dialog.phase = close::ClosePhase::Loading;
            dialog.stale = false;
            let (lane, generation, seq) = (dialog.lane.clone(), dialog.generation, dialog.read_seq);
            let tag = self.tag(Pending::ClosePreview { generation, seq });
            ctx.query(
                tag,
                Query::Lanes(LanesQuery::GetLaneClosePreview { lane_id: lane }),
            );
        }
    }

    /// The reads of the tab: the lead's lanes, its detached lanes, the opened lane and the
    /// lead's root run.
    fn read_tab(&mut self, ctx: &mut FeatureContext<'_>) {
        let Some(thread) = self.thread.clone() else {
            return;
        };
        if self.list_stale || self.more_requested {
            let append = self.more_requested && !self.list_stale;
            self.list_stale = false;
            self.more_requested = false;
            let tag = self.tag(Pending::List {
                thread: thread.clone(),
                append,
            });
            let cursor = append.then(|| self.next_cursor.clone()).flatten();
            ctx.query(tag, list_query(thread.clone(), false, cursor));
        }
        if self.detached_stale || self.detached_more_requested {
            let append = self.detached_more_requested && !self.detached_stale;
            if !append {
                // A refresh answers a request for more as well: it reads one page past the
                // ones shown, and the next pages follow its reply.
                self.detached_target_pages = self
                    .detached_target_pages
                    .max(self.detached_pages.max(1) + usize::from(self.detached_more_requested));
            }
            self.detached_stale = false;
            self.detached_more_requested = false;
            let tag = self.tag(Pending::Detached {
                thread: thread.clone(),
                append,
            });
            let cursor = append.then(|| self.detached_next.clone()).flatten();
            ctx.query(tag, list_query(thread.clone(), true, cursor));
        }
        if self.detail_stale {
            self.detail_stale = false;
            if let Some(lane) = self.opened.clone() {
                let tag = self.tag(Pending::Detail { lane: lane.clone() });
                ctx.query(tag, Query::Lanes(LanesQuery::GetLane { lane_id: lane }));
            }
        }
        if let Some((lead, cursor)) = self.lead_root.due() {
            let tag = self.tag(Pending::LeadRoot {
                thread: lead.clone(),
                card: false,
            });
            ctx.query(tag, roots_query(lead, cursor));
        }
    }

    /// Draws the open confirmation and runs its answer.
    fn draw_close(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let sending = self
            .pending
            .values()
            .any(|pending| matches!(pending, Pending::CloseCommand { .. }));
        let Some(dialog) = &mut self.close else {
            return;
        };
        match close::draw(ui.ctx(), dialog, sending) {
            Some(close::DialogAction::Confirm) => {
                if let Some(order) = dialog.order(sending) {
                    let generation = dialog.generation;
                    let tag = self.tag(Pending::CloseCommand { generation });
                    ctx.command(tag, Command::Lanes(LanesCommand::CloseLane(order)));
                }
            }
            Some(close::DialogAction::Reload) => {
                dialog.read_due = true;
                dialog.notice = None;
            }
            Some(close::DialogAction::Cancel) => self.close = None,
            None => {}
        }
    }

    fn drive_reattach(&mut self, ctx: &mut FeatureContext<'_>) {
        if !self.root_settled() {
            return;
        }
        let (Some(flow), Some(lead), Some(root)) = (
            &self.reattach,
            self.thread.clone(),
            self.lead_root.root.clone(),
        ) else {
            return;
        };
        let Some(state) = flow.state.clone() else {
            return;
        };
        let lane = flow.lane.clone();
        self.reattach = None;
        self.manage(
            ctx,
            Pending::Command("Reattach lane"),
            &lane,
            state.management_revision,
            LaneManagementAction::Reattach {
                lead_thread_id: lead,
                expected_lead_root_run_id: root,
            },
            state.family_revision,
        );
    }

    fn draw_detached_row(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut FeatureContext<'_>,
        detached: LaneSummary,
    ) {
        let lane = detached.lane_id;
        ui.push_id(("detached", lane.as_str()), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Detached: {}", detached.objective_preview));
                if self.reattach.is_none()
                    && ui
                        .add(ghost("Reattach"))
                        .on_hover_text("Make this thread the lane's lead again")
                        .clicked()
                {
                    self.reattach = Some(Reattach {
                        lane: lane.clone(),
                        state: None,
                    });
                    let tag = self.tag(Pending::Management { lane: lane.clone() });
                    ctx.query(
                        tag,
                        Query::Lanes(LanesQuery::GetLaneManagement {
                            lane_id: lane.clone(),
                        }),
                    );
                    self.lead_root.root = None;
                    self.lead_root.invalidate();
                }
            });
        });
    }

    fn draw_lane_row(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &mut FeatureContext<'_>,
        lane: &LaneSummary,
    ) {
        ui.push_id(lane.lane_id.as_str(), |ui| {
            soft_frame(palette().SURFACE).show(ui, |ui| {
                ui.set_width(ui.available_width());
                if ui.add(ghost(medium(&lane.objective_preview))).clicked() {
                    self.opened = Some(lane.lane_id.clone());
                    self.detail = None;
                    self.detail_stale = true;
                }
                // The badges keep their text on one line: a narrow tab moves them to the
                // next row instead of breaking a word into letters.
                ui.horizontal_wrapped(|ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                    policy_badge(ui, policy_word(lane.policy));
                    ui.label(muted(start_label(lane.start)));
                    if let Some(status) = lane.latest_report_status {
                        status_badge(ui, status);
                    }
                    if let Some(current) = &lane.current_close {
                        close::badge(ui, current);
                    }
                });
                if lane.policy == LaneManagementPolicy::Detached {
                    return;
                }
                ui.horizontal_wrapped(|ui| {
                    let (toggle, target) = match lane.policy {
                        LaneManagementPolicy::Attached => (
                            "Report completion only (summary)",
                            ManagedLanePolicy::Summary,
                        ),
                        _ => ("Report milestones (attached)", ManagedLanePolicy::Attached),
                    };
                    if ui.add(ghost(toggle)).clicked() {
                        self.manage(
                            ctx,
                            Pending::Command("Lane policy"),
                            &lane.lane_id,
                            lane.management_revision,
                            LaneManagementAction::SetPolicy { policy: target },
                            None,
                        );
                    }
                    if ui.add(ghost("Detach lane")).clicked() {
                        self.manage(
                            ctx,
                            Pending::Command("Detach lane"),
                            &lane.lane_id,
                            lane.management_revision,
                            LaneManagementAction::Detach,
                            None,
                        );
                    }
                    for choice in [CloseChoice::Complete, CloseChoice::Discard] {
                        if ui
                            .add(ghost(choice.menu_label()))
                            .on_hover_text(MEANING)
                            .clicked()
                        {
                            self.open_close(
                                lane.lane_id.clone(),
                                choice,
                                lane.objective_preview.clone(),
                            );
                        }
                    }
                });
            });
        });
    }

    fn draw_detail(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        let Some(detail) = self.detail.clone() else {
            return;
        };
        let Some(lead) = self.thread.clone() else {
            return;
        };
        let project = ctx
            .cache()
            .threads
            .get(detail.summary.thread_id.as_str())
            .map(|thread| thread.project.clone());
        let root = self
            .lead_root
            .root
            .clone()
            .filter(|_| detail.summary.lead_thread_id.as_ref() == Some(&lead));
        let mut choice = None;
        let mut open = false;
        surface_frame(palette().SURFACE_RAISED).show(ui, |ui| {
            ui.label(h3(format!("Lane {}", detail.summary.lane_id.as_str())));
            ui.label(muted(format!(
                "{} · management revision {} · brief revision {} · generation {}",
                policy_word(detail.summary.policy),
                detail.summary.management_revision.0,
                detail.summary.brief_revision.0,
                detail.summary.generation.0
            )));
            let view = LaneCardView {
                lead: &lead,
                summary: &detail.summary,
                detail: Some(&detail),
                can_resume: root.is_some(),
                status: "",
            };
            choice = lane_body(ui, &view, CardPlace::LanesTab);
            open = project.is_some() && ui.add(ghost("Open lane")).clicked();
        });
        if let (Some(CardChoice::ResumeNotifications), Some(root)) = (choice, root) {
            self.command(
                ctx,
                "Resume notifications",
                Command::Lanes(LanesCommand::ResumeLaneDeliveries(ResumeLaneDeliveries {
                    lane_id: detail.summary.lane_id.clone(),
                    expected_management_revision: detail.summary.management_revision,
                    lead_thread_id: lead,
                    expected_lead_root_run_id: root,
                })),
            );
        }
        if let (true, Some(project)) = (open, project) {
            ctx.navigate(Navigation::SelectThread {
                project,
                thread: detail.summary.thread_id.clone(),
            });
        }
    }

    fn remember(&mut self, lanes: &[LaneSummary]) {
        for lane in lanes {
            self.missing.remove(&lane.thread_id);
            self.unreadable.remove(&lane.thread_id);
            self.directory.insert(lane.thread_id.clone(), lane.clone());
        }
    }

    fn lookup_answered(
        &mut self,
        lead: ThreadId,
        page: Option<(Vec<LaneSummary>, Option<PageCursor>)>,
    ) {
        let Some(mut lookup) = self.lookups.remove(&lead) else {
            return;
        };
        let Some((items, next)) = page else {
            // A refused read is not an answer: the menu says so, and the lane is read again
            // only when the user asks.
            self.unreadable.extend(lookup.wanted);
            return;
        };
        self.remember(&items);
        lookup
            .wanted
            .retain(|thread| !self.directory.contains_key(thread));
        if lookup.wanted.is_empty() {
            return;
        }
        match next {
            Some(cursor) => {
                lookup.cursor = Some(cursor);
                lookup.due = true;
                self.lookups.insert(lead, lookup);
            }
            None => self.missing.extend(lookup.wanted),
        }
    }
}

fn list_query(lead: ThreadId, detached: bool, cursor: Option<PageCursor>) -> Query {
    Query::Lanes(LanesQuery::ListLanes(ListLanes {
        lead_thread: lead,
        policy: None,
        detached,
        page: PageRequest {
            cursor,
            ..PageRequest::default()
        },
    }))
}

fn roots_query(thread: ThreadId, cursor: Option<PageCursor>) -> Query {
    Query::Execution(ExecutionQuery::ListRunDetails(RunDetailsQuery {
        thread,
        scope: RunScope::Roots,
        page: PageRequest {
            cursor,
            ..PageRequest::default()
        },
    }))
}

fn start_label(start: LaneStartState) -> &'static str {
    match start {
        LaneStartState::NotStarted => "not started",
        LaneStartState::StartBlocked => "start blocked",
        LaneStartState::OperatorStopped => "stopped (can resume)",
        LaneStartState::Started => "started",
    }
}

fn failure(label: &str, error: &FeatureError) -> String {
    if error.outcome_unknown {
        format!("{label}: outcome unknown, not resent — {}", error.message)
    } else {
        format!("{label}: {}", error.message)
    }
}

impl Feature for LanesFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Lanes
    }

    fn title(&self) -> &'static str {
        "Lanes"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Queries, Capability::Commands],
        }
    }

    fn as_lanes(&self) -> Option<&LanesFeature> {
        Some(self)
    }

    fn as_lanes_mut(&mut self) -> Option<&mut LanesFeature> {
        Some(self)
    }

    fn reset(&mut self) {
        self.lanes.clear();
        self.next_cursor = None;
        self.detail = None;
        self.lead_root = LeadRoot::default();
        self.lead_root.follow(self.thread.clone());
        self.detached.clear();
        self.detached_next = None;
        self.detached_pages = 0;
        self.detached_target_pages = 0;
        self.list_stale = self.thread.is_some();
        self.detached_stale = self.thread.is_some();
        self.detail_stale = self.opened.is_some();
        self.directory.clear();
        self.missing.clear();
        self.unreadable.clear();
        self.lookups.clear();
        if let Some(card) = &mut self.card {
            card.detail = None;
            card.detail_stale = true;
            card.root = LeadRoot::default();
            card.root.follow(Some(card.lead.clone()));
        }
        // What the dialog showed came from the core: it is read again, and nothing is sent
        // from the old reading. A close that was sent keeps its tag.
        if let Some(dialog) = &mut self.close {
            dialog.phase = close::ClosePhase::Loading;
            dialog.read_due = true;
        }
    }

    fn event(&mut self, event: &UiEvent) {
        let UiEvent::Lanes(LanesEvent::LaneChanged { lane, also_for }) = event else {
            return;
        };
        self.remember(std::slice::from_ref(lane));
        if let Some(card) = &mut self.card
            && card.thread == lane.thread_id
        {
            card.detail_stale = true;
        }
        // A lane another client moved to a lead that is not ours has no tie to us in the
        // event except `also_for`, or the row we already show for it.
        let ours = self.thread.is_some()
            && (lane.lead_thread_id == self.thread
                || self.thread.as_ref().is_some_and(|t| also_for.contains(t))
                || self.lanes.iter().any(|l| l.lane_id == lane.lane_id)
                || self.detached.iter().any(|l| l.lane_id == lane.lane_id));
        if ours {
            self.list_stale = true;
        }
        // A detached lane has no lead: it is told apart only by asking the core again.
        if self.thread.is_some() && (ours || lane.lead_thread_id.is_none()) {
            self.detached_stale = true;
        }
        if self.opened.as_ref() == Some(&lane.lane_id) {
            self.detail_stale = true;
        }
        if let Some(dialog) = &mut self.close
            && dialog.lane == lane.lane_id
            && dialog.moved_by(lane)
        {
            dialog.stale = true;
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(pending) = self.pending.remove(&tag) else {
            return;
        };
        match (pending, reply) {
            (Pending::Command(label), FeatureReply::Command(Ok(_))) => {
                self.status = format!("{label}: accepted");
                self.refresh_after_command();
            }
            (Pending::Command(label), FeatureReply::Command(Err(error))) => {
                self.status = failure(label, &error);
                if error.outcome_unknown {
                    self.pending.insert(tag, Pending::Command(label));
                }
                self.refresh_after_command();
            }
            (Pending::SidebarCommand(label), FeatureReply::Command(result)) => {
                self.sidebar_notice = match &result {
                    Ok(_) => None,
                    Err(error) => {
                        if error.outcome_unknown {
                            self.pending.insert(tag, Pending::SidebarCommand(label));
                        }
                        Some(failure(label, error))
                    }
                };
                self.refresh_after_command();
            }
            (Pending::CardCommand { thread, label }, FeatureReply::Command(result)) => {
                if let Err(error) = &result
                    && error.outcome_unknown
                {
                    self.pending.insert(
                        tag,
                        Pending::CardCommand {
                            thread: thread.clone(),
                            label,
                        },
                    );
                }
                if let Some(card) = self.card.as_mut().filter(|card| card.thread == thread) {
                    card.status = match &result {
                        Ok(_) => format!("{label}: accepted"),
                        Err(error) => failure(label, error),
                    };
                    card.detail_stale = true;
                    card.root.invalidate();
                }
            }
            (Pending::CloseCommand { generation }, FeatureReply::Command(Ok(reply))) => {
                let closed = match reply.result.as_deref() {
                    Some(CommandResult::Lanes(LanesResult::Closed(result))) => Some(result.clone()),
                    _ => None,
                };
                self.status = match closed {
                    Some(result) if result.already_closed => {
                        already_closed_notice(close::close_label(result.kind))
                    }
                    Some(result) => closed_notice(close::close_label(result.kind)),
                    None => "Close lane: accepted".to_owned(),
                };
                // Only the confirmation that sent it is finished by the answer.
                if self
                    .close
                    .as_ref()
                    .is_some_and(|dialog| dialog.generation == generation)
                {
                    self.close = None;
                }
                self.refresh_after_command();
            }
            (Pending::CloseCommand { generation }, FeatureReply::Command(Err(error))) => {
                let mine = self
                    .close
                    .as_ref()
                    .is_some_and(|dialog| dialog.generation == generation);
                if error.outcome_unknown {
                    // Not a success: the same tag answers again with what the core resolved.
                    self.pending
                        .insert(tag, Pending::CloseCommand { generation });
                    self.status = failure("Close lane", &error);
                    if let Some(dialog) = self.close.as_mut().filter(|_| mine) {
                        dialog.notice = Some(
                            "The outcome is unknown. It is not taken as recorded; the ledger is read again."
                                .to_owned(),
                        );
                        dialog.phase = close::ClosePhase::Loading;
                        dialog.read_due = true;
                    }
                } else if let Some(dialog) = self.close.as_mut().filter(|_| mine) {
                    let stale = error.code == Some(ErrorCode::StaleRevision)
                        || error.message.starts_with("stale");
                    dialog.stale |= stale;
                    dialog.notice = Some(if stale {
                        STALE_NOTICE.to_owned()
                    } else {
                        not_recorded(&error.message)
                    });
                } else {
                    self.status = failure("Close lane", &error);
                }
                self.refresh_after_command();
            }
            (
                Pending::ClosePreview { generation, seq },
                FeatureReply::Query(Ok(QueryReply::Lanes(LanesReply::LaneClosePreview(preview)))),
            ) => {
                if let Some(dialog) = &mut self.close
                    && dialog.generation == generation
                    && dialog.read_seq == seq
                {
                    dialog.phase = close::ClosePhase::Loaded(preview);
                    dialog.stale = false;
                }
            }
            (Pending::ClosePreview { generation, seq }, FeatureReply::Query(Err(error))) => {
                if let Some(dialog) = &mut self.close
                    && dialog.generation == generation
                    && dialog.read_seq == seq
                {
                    dialog.phase = close::ClosePhase::Unavailable(error.message);
                }
            }
            (
                Pending::Lookup { lead },
                FeatureReply::Query(Ok(QueryReply::Lanes(LanesReply::Lanes(page)))),
            ) => self.lookup_answered(lead, Some((page.items, page.next_cursor))),
            (Pending::Lookup { lead }, FeatureReply::Query(_)) => self.lookup_answered(lead, None),
            (
                Pending::CardDetail { thread },
                FeatureReply::Query(Ok(QueryReply::Lanes(LanesReply::Lane(detail)))),
            ) => {
                if let Some(card) = self.card.as_mut().filter(|card| card.thread == thread) {
                    card.detail = Some(*detail);
                }
            }
            (Pending::CardDetail { thread }, FeatureReply::Query(Err(error))) => {
                if let Some(card) = self.card.as_mut().filter(|card| card.thread == thread) {
                    card.status = error.message;
                }
            }
            (
                Pending::LeadRoot { thread, card: true },
                FeatureReply::Query(Ok(QueryReply::Execution(ExecutionReply::RunDetails(page)))),
            ) => {
                if let Some(card) = &mut self.card {
                    card.root.answer(&thread, page);
                }
            }
            // The card's own read: its refusal is the card's to show, not the tab's.
            (Pending::LeadRoot { thread, card: true }, FeatureReply::Query(Err(error))) => {
                if let Some(card) = self.card.as_mut().filter(|card| card.lead == thread) {
                    card.status = error.message;
                }
            }
            (_, FeatureReply::Query(Err(error))) => self.status = error.message,
            (
                Pending::List { thread, append },
                FeatureReply::Query(Ok(QueryReply::Lanes(LanesReply::Lanes(page)))),
            ) => {
                if self.thread.as_ref() != Some(&thread) {
                    return;
                }
                self.remember(&page.items);
                if append {
                    for lane in page.items {
                        if !self.lanes.iter().any(|l| l.lane_id == lane.lane_id) {
                            self.lanes.push(lane);
                        }
                    }
                } else {
                    self.lanes = page.items;
                }
                self.next_cursor = page.next_cursor;
            }
            (
                Pending::Detached { thread, append },
                FeatureReply::Query(Ok(QueryReply::Lanes(LanesReply::Lanes(page)))),
            ) => {
                if self.thread.as_ref() != Some(&thread) {
                    return;
                }
                self.remember(&page.items);
                if append {
                    for lane in page.items {
                        if !self.detached.iter().any(|l| l.lane_id == lane.lane_id) {
                            self.detached.push(lane);
                        }
                    }
                    self.detached_pages += 1;
                } else {
                    self.detached = page.items;
                    self.detached_pages = 1;
                }
                self.detached_next = page.next_cursor;
                // Pages the user had loaded before a refresh are read again, one at a time.
                if self.detached_pages < self.detached_target_pages && self.detached_next.is_some()
                {
                    self.detached_more_requested = true;
                }
            }
            (
                Pending::Detail { lane },
                FeatureReply::Query(Ok(QueryReply::Lanes(LanesReply::Lane(detail)))),
            ) => {
                if self.opened.as_ref() == Some(&lane) {
                    self.detail = Some(*detail);
                }
            }
            (
                Pending::Management { lane },
                FeatureReply::Query(Ok(QueryReply::Lanes(LanesReply::LaneManagement(state)))),
            ) => {
                if let Some(flow) = &mut self.reattach
                    && flow.lane == lane
                {
                    flow.state = Some(state);
                }
            }
            (
                Pending::LeadRoot {
                    thread,
                    card: false,
                },
                FeatureReply::Query(Ok(QueryReply::Execution(ExecutionReply::RunDetails(page)))),
            ) => self.lead_root.answer(&thread, page),
            _ => {}
        }
    }

    fn maintain(&mut self, ctx: &mut FeatureContext<'_>, visibility: Visibility) {
        let selected = ctx.selection().thread.clone();
        if selected != self.thread {
            self.switch_thread(selected);
        }
        self.follow_card(ctx);
        if !visibility.available {
            self.intents.clear();
            self.card_actions.clear();
            return;
        }
        for intent in std::mem::take(&mut self.intents) {
            self.apply_intent(ctx, intent);
        }
        for action in std::mem::take(&mut self.card_actions) {
            self.apply_card_action(ctx, action);
        }
        self.read_card(ctx);
        self.read_lookups(ctx);
        self.read_close(ctx);
        if visibility.is_visible() {
            self.read_tab(ctx);
            self.drive_reattach(ctx);
        }
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        if self.thread.is_none() {
            empty_state(
                ui,
                "No thread selected",
                "Select a lead thread to see the lanes it manages.",
                None,
            );
            self.draw_close(ui, ctx);
            return;
        }
        if !self.status.is_empty() {
            ui.label(&self.status);
        }
        if self.lanes.is_empty() {
            ui.label(muted("This thread manages no lanes."));
        }
        // Each list scrolls in its own bounded area, so that however many lanes the core
        // holds, the buttons that page through them stay inside the window.
        if !self.lanes.is_empty() {
            bounded_list(ui, "lanes-managed", |ui| {
                for lane in self.lanes.clone() {
                    self.draw_lane_row(ui, ctx, &lane);
                }
            });
        }
        if self.next_cursor.is_some() && ui.add(ghost("Load more lanes")).clicked() {
            self.more_requested = true;
        }
        if !self.detached.is_empty() {
            bounded_list(ui, "lanes-detached", |ui| {
                for detached in self.detached.clone() {
                    self.draw_detached_row(ui, ctx, detached);
                }
            });
        }
        if self.detached_next.is_some() && ui.add(ghost("Load more detached lanes")).clicked() {
            self.detached_more_requested = true;
        }
        self.draw_detail(ui, ctx);
        self.draw_close(ui, ctx);
    }
}

#[cfg(test)]
mod tests;
