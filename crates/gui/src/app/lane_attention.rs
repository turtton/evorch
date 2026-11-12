//! The lane attention list in the Workbench (ADR 0044 Phase 2, 担当 D): reading it from the
//! ledger and running the existing operations it offers.
//!
//! Storage derives the list ([`storage::Database::lane_attention`]) from the lane ledger. This
//! module only asks, off the UI thread, and shows the answer. It owns no timer and no rule:
//!
//! - **what triggers a read**: a lane or question event, a lane-delivery diagnostic, the window
//!   regaining focus, an unreadable-history notice, a lead the sidebar newly shows, the user's
//!   Refresh and the user's own operation on an item. Events are hints; an event lost on the way
//!   costs a read later, never an item;
//! - **which answer wins**: every read gets a number when it starts and the board refuses an
//!   answer that is not newer than the one it holds ([`AttentionBoard::apply`]);
//! - **what an item can do**: only operations that already exist (open the lane, continue the
//!   lead, resume held notices, answer a question the ledger leaves to the user). Forwarding,
//!   nudging and verification have no operation here until they are merged.
//!
//! Reading, redrawing and polling never change the ledger, the provider input or the
//! notification history.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use event_bus::lane::LaneAttentionFilter;
use event_bus::{Event, EventKind, LedgerEvent, ToolEvent};
use storage::Database;
use workbench_protocol::lane_attention::{LaneAttentionItem, LaneAttentionKind};

use super::WorkbenchState;
use crate::model::lane_attention::{AttentionAction, AttentionBoard, AttentionOp, LeadRead, wire};
use crate::model::tasks::AgentRunSource;

/// Items asked for per page (the ledger's own page limit).
const PAGE: usize = 20;
/// Pages read for one lead before the list is shown as cut short.
const MAX_PAGES: usize = 25;
/// Starts over this many times when a page's snapshot moved under the read.
const MAX_RESTARTS: usize = 3;
/// Reads in flight at once.
const MAX_IN_FLIGHT: usize = 4;

/// Reads one lead's attention list. Blocking: it always runs on a worker thread.
pub trait LaneAttentionReader: Send + Sync + 'static {
    /// # Errors
    /// A message when the ledger cannot be read or holds a value the wire refuses.
    fn read(&self, lead_thread_id: &str) -> Result<LeadRead, String>;
}

/// Reads through a read-only connection to the shared database, converting each page to the
/// wire form at this boundary.
pub struct StorageLaneAttentionReader {
    db_path: PathBuf,
}

impl StorageLaneAttentionReader {
    #[must_use]
    pub const fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }
}

impl LaneAttentionReader for StorageLaneAttentionReader {
    fn read(&self, lead_thread_id: &str) -> Result<LeadRead, String> {
        let db = Database::open_read_only(&self.db_path).map_err(|error| error.to_string())?;
        let filter = LaneAttentionFilter {
            lead_thread_id: lead_thread_id.to_owned(),
            project_id: None,
        };
        'restart: for _ in 0..MAX_RESTARTS {
            let mut items: Vec<LaneAttentionItem> = Vec::new();
            let mut cursor = None;
            for page_number in 0..MAX_PAGES {
                let page = match db.lane_attention(filter.clone(), cursor.take(), PAGE) {
                    Ok(page) => page,
                    Err(storage::StorageError::LaneOperation(
                        event_bus::lane::LaneOperationError::StaleCursor,
                    )) => continue 'restart,
                    Err(error) => return Err(error.to_string()),
                };
                let converted = wire::page(&page).map_err(|error| error.to_string())?;
                items.extend(converted.items);
                match page.next_cursor {
                    Some(_) if page_number + 1 == MAX_PAGES => {
                        return Ok(LeadRead {
                            questions: stored_questions(&db, &items)?,
                            items,
                            snapshot: converted.snapshot,
                            truncated: true,
                        });
                    }
                    Some(next) => cursor = Some(next),
                    None => {
                        return Ok(LeadRead {
                            questions: stored_questions(&db, &items)?,
                            items,
                            snapshot: converted.snapshot,
                            truncated: false,
                        });
                    }
                }
            }
        }
        Err("the attention list kept changing while it was read; refresh to try again".into())
    }
}

/// The stored form of each question the list asks the user, so the answer card never depends
/// on an event this window may have missed.
fn stored_questions(
    db: &Database,
    items: &[LaneAttentionItem],
) -> Result<std::collections::BTreeMap<String, event_bus::UserQuestion>, String> {
    let mut questions = std::collections::BTreeMap::new();
    for item in items {
        if let LaneAttentionKind::QuestionPending { question, .. } = &item.kind
            && let Some(stored) = db
                .user_question(question.as_str())
                .map_err(|error| error.to_string())?
        {
            questions.insert(question.to_string(), stored);
        }
    }
    Ok(questions)
}

/// An operation with a side effect, held until the ledger confirms its item unchanged.
struct PendingAction {
    action: AttentionAction,
    /// The first read number that began after the choice; an earlier read proves nothing.
    min_seq: u64,
}

struct Loaded {
    lead: String,
    seq: u64,
    result: Result<LeadRead, String>,
}

/// The attention side of the Workbench: the board and the reads under way.
pub(super) struct AttentionState {
    reader: Option<Arc<dyn LaneAttentionReader>>,
    pub(super) board: AttentionBoard,
    next_seq: u64,
    queued: BTreeSet<String>,
    in_flight: BTreeSet<String>,
    /// Asked for again while a read of it was in flight: that read may predate the request.
    dirty: BTreeSet<String>,
    /// Leads the sidebar has shown; each is read once when first seen.
    known_leads: BTreeSet<String>,
    seen_gap_epoch: u64,
    focused: bool,
    /// Operations chosen by the user and waiting for a read made after the choice.
    pending: Vec<PendingAction>,
    results: Receiver<Loaded>,
    sender: Sender<Loaded>,
}

impl Default for AttentionState {
    fn default() -> Self {
        let (sender, results) = channel();
        Self {
            reader: None,
            board: AttentionBoard::default(),
            next_seq: 0,
            queued: BTreeSet::new(),
            in_flight: BTreeSet::new(),
            dirty: BTreeSet::new(),
            known_leads: BTreeSet::new(),
            seen_gap_epoch: 0,
            focused: true,
            pending: Vec::new(),
            results,
            sender,
        }
    }
}

impl AttentionState {
    pub(super) fn request(&mut self, lead: &str) {
        if self.in_flight.contains(lead) {
            self.dirty.insert(lead.to_owned());
        } else {
            self.queued.insert(lead.to_owned());
        }
    }

    fn request_all(&mut self) {
        for lead in self.known_leads.clone() {
            self.request(&lead);
        }
    }
}

/// Whether `event` may have changed what the attention list says. A hint only.
fn may_change_attention(event: &Event) -> bool {
    match &event.kind {
        EventKind::Ledger(
            LedgerEvent::LaneCreated { .. }
            | LedgerEvent::LaneManagementChanged { .. }
            | LedgerEvent::LaneReportAppended { .. }
            | LedgerEvent::LaneBriefRevised { .. },
        ) => true,
        EventKind::Tool(ToolEvent::UserQuestionUpdated { question }) => {
            question.responder.is_some()
        }
        EventKind::Diagnostic(diagnostic) if diagnostic.source == "lane_monitor" => true,
        EventKind::Diagnostic(diagnostic) => diagnostic.source == "lane_delivery",
        _ => false,
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Show the lane attention list read by `reader`.
    #[must_use]
    pub fn with_lane_attention(mut self, reader: Arc<dyn LaneAttentionReader>) -> Self {
        self.attention.reader = Some(reader);
        self
    }

    /// What the ledger last said needs attention.
    #[must_use]
    pub const fn lane_attention(&self) -> &AttentionBoard {
        &self.attention.board
    }

    /// Reads every lead's list again: the user's Refresh, and what a headless host calls to
    /// resynchronise after it may have missed events.
    pub fn refresh_lane_attention(&mut self) {
        self.attention.request_all();
    }

    /// Notes an event: when it may have changed the list, the leads are read again.
    pub(super) fn note_attention_event(&mut self, event: &Event) {
        if self.attention.reader.is_some() && may_change_attention(event) {
            self.attention.request_all();
        }
    }

    /// The window regaining focus is the one moment nothing else says what happened meanwhile.
    pub(super) fn observe_attention_focus(&mut self, focused: Option<bool>) {
        let focused = focused.unwrap_or(true);
        if focused && !self.attention.focused {
            self.attention.request_all();
        }
        self.attention.focused = focused;
    }

    /// Drains finished reads and starts the due ones, once per frame or headless tick.
    pub(super) fn pump_attention(&mut self) {
        if self.attention.reader.is_none() {
            return;
        }
        while let Ok(loaded) = self.attention.results.try_recv() {
            self.attention.in_flight.remove(&loaded.lead);
            // Asked for again while this read ran: it may have taken its snapshot before the
            // change that prompted the request, so it can be shown but cannot confirm anything.
            let overtaken = self.attention.dirty.remove(&loaded.lead);
            if overtaken {
                self.attention.queued.insert(loaded.lead.clone());
            }
            let succeeded = loaded.result.is_ok();
            self.attention
                .board
                .apply(&loaded.lead, loaded.seq, loaded.result);
            if overtaken {
                self.defer_pending_actions(&loaded.lead);
            } else {
                self.settle_pending_actions(&loaded.lead, loaded.seq, succeeded);
            }
        }
        for lead in self.sidebar_leads() {
            if self.attention.known_leads.insert(lead.clone()) {
                self.attention.queued.insert(lead);
            }
        }
        let gap_epoch = self.notifications.gap_epoch();
        if gap_epoch != self.attention.seen_gap_epoch {
            self.attention.seen_gap_epoch = gap_epoch;
            self.attention.request_all();
        }
        self.start_attention_reads();
    }

    /// Leads of the managed lanes the sidebar shows.
    fn sidebar_leads(&self) -> BTreeSet<String> {
        self.sidebar
            .threads
            .iter()
            .filter_map(|thread| thread.lane.as_ref())
            .filter(|lane| lane.policy != workspace_ui::LanePolicy::Detached)
            .filter_map(|lane| lane.lead_thread_id.as_ref().map(ToString::to_string))
            .collect()
    }

    fn start_attention_reads(&mut self) {
        let Some(reader) = self.attention.reader.clone() else {
            return;
        };
        let (spawner, waker) = self.lanes.runner();
        while self.attention.in_flight.len() < MAX_IN_FLIGHT
            && let Some(lead) = self.attention.queued.pop_first()
        {
            self.attention.next_seq += 1;
            let seq = self.attention.next_seq;
            self.attention.in_flight.insert(lead.clone());
            let sender = self.attention.sender.clone();
            let waker = Arc::clone(&waker);
            let reader = Arc::clone(&reader);
            spawner(Box::new(move || {
                let result = reader.read(&lead);
                let _ = sender.send(Loaded { lead, seq, result });
                waker();
            }));
        }
    }

    /// Takes what the user chose on an item. Opening a lane has no side effect and runs at once.
    /// Every other operation waits for a read of the ledger made after the choice, and runs only
    /// if that read still reports the item at the revision the screen showed: the window's own
    /// projection can be older than another window's change, so it never decides alone.
    pub(super) fn apply_attention_action(&mut self, ctx: &egui::Context, action: AttentionAction) {
        match &action.op {
            AttentionOp::Refresh => {
                if action.lead.is_empty() {
                    self.refresh_lane_attention();
                } else {
                    self.attention.request(&action.lead);
                }
            }
            AttentionOp::OpenLane(thread) => {
                self.open_thread_from_attention(ctx, thread.clone());
            }
            AttentionOp::BeginClose(choice) => {
                let choice = *choice;
                self.begin_lane_close(&action, choice);
            }
            AttentionOp::ContinueThread(_)
            | AttentionOp::ResumeThread(_)
            | AttentionOp::ResumeDelivery => {
                let held = action.item.as_ref().is_some_and(|(id, revision)| {
                    self.attention.board.holds(&action.lead, id, *revision)
                });
                if !held {
                    self.push_notice("That item changed; the list was read again.");
                    self.attention.request(&action.lead);
                    return;
                }
                if self
                    .attention
                    .pending
                    .iter()
                    .any(|pending| pending.action == action)
                {
                    return;
                }
                let lead = action.lead.clone();
                self.attention.pending.push(PendingAction {
                    action,
                    min_seq: self.attention.next_seq + 1,
                });
                self.attention.request(&lead);
            }
        }
    }

    /// Keeps the held operations of `lead` for the read that is about to start: none of the reads
    /// begun so far can be trusted to include the change that overtook them.
    fn defer_pending_actions(&mut self, lead: &str) {
        let next = self.attention.next_seq + 1;
        for pending in &mut self.attention.pending {
            if pending.action.lead == lead {
                pending.min_seq = pending.min_seq.max(next);
            }
        }
    }

    /// Runs or drops the held operations of `lead` that read number `seq` can decide.
    fn settle_pending_actions(&mut self, lead: &str, seq: u64, succeeded: bool) {
        use crate::panes::agent::LaneAction;
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.attention.pending)
            .into_iter()
            .partition(|pending| pending.action.lead == lead && seq >= pending.min_seq);
        self.attention.pending = waiting;
        for pending in ready {
            let PendingAction { action, min_seq } = pending;
            let confirmed = succeeded
                && action.item.as_ref().is_some_and(|(id, revision)| {
                    self.attention
                        .board
                        .confirms(&action.lead, id, *revision, min_seq)
                });
            if !confirmed {
                self.push_notice(if succeeded {
                    "That item changed; nothing was done."
                } else {
                    "The lane list could not be confirmed; nothing was done."
                });
                continue;
            }
            match action.op {
                AttentionOp::ContinueThread(thread) => {
                    self.apply_lane_action(LaneAction::ContinueLead { lead: thread });
                }
                AttentionOp::ResumeThread(thread) => {
                    use workbench_protocol::lane_attention::{
                        LaneAttentionAction, LaneAttentionKind,
                    };
                    // The freshly confirmed item, not arbitrary UI arguments, grants
                    // this operation for exactly one suppressed destination.
                    let permitted = action.item.as_ref().is_some_and(|(id, revision)| {
                        self.attention.board.items().iter().any(|item| {
                            item.lead_thread.as_str() == action.lead.as_str()
                                && item.id.as_str() == id.as_str()
                                && item.revision.0 == *revision
                                && item.available_actions.contains(&LaneAttentionAction::Resume)
                                && matches!(&item.kind, LaneAttentionKind::RestoreFailed { destination_thread, suppressed: true, .. }
                                    if destination_thread.as_str() == thread.as_str())
                        })
                    });
                    if permitted {
                        self.apply_lane_action(LaneAction::ResumeDelivery { lead: thread });
                    } else {
                        self.push_notice(
                            "The recovery destination or allowed action changed; nothing was done.",
                        );
                    }
                }
                AttentionOp::ResumeDelivery => {
                    self.apply_lane_action(LaneAction::ResumeDelivery { lead: action.lead });
                }
                AttentionOp::OpenLane(_) | AttentionOp::Refresh | AttentionOp::BeginClose(_) => {}
            }
            self.attention.request(lead);
        }
    }
}
