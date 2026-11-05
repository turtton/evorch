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
use workbench_protocol::lane_attention::LaneAttentionItem;

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
                            items,
                            snapshot: converted.snapshot,
                            truncated: true,
                        });
                    }
                    Some(next) => cursor = Some(next),
                    None => {
                        return Ok(LeadRead {
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
            results,
            sender,
        }
    }
}

impl AttentionState {
    fn request(&mut self, lead: &str) {
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
            if self.attention.dirty.remove(&loaded.lead) {
                self.attention.queued.insert(loaded.lead.clone());
            }
            self.attention
                .board
                .apply(&loaded.lead, loaded.seq, loaded.result);
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

    /// Runs what the user chose on an item. Everything but a refresh needs the item the
    /// latest read still holds, unchanged: a screen older than that reads again instead.
    pub(super) fn apply_attention_action(&mut self, ctx: &egui::Context, action: AttentionAction) {
        use crate::panes::agent::LaneAction;
        if action.op == AttentionOp::Refresh {
            if action.lead.is_empty() {
                self.refresh_lane_attention();
            } else {
                self.attention.request(&action.lead);
            }
            return;
        }
        let current = action
            .item
            .as_ref()
            .is_some_and(|(id, revision)| self.attention.board.holds(&action.lead, id, *revision));
        if !current {
            self.push_notice("That item changed; the list was read again.");
            self.attention.request(&action.lead);
            return;
        }
        let lead = action.lead.clone();
        match action.op {
            AttentionOp::OpenLane(thread) => self.open_notification_thread(ctx, thread),
            AttentionOp::ContinueLead => {
                self.apply_lane_action(LaneAction::ContinueLead { lead: lead.clone() })
            }
            AttentionOp::ResumeDelivery => {
                self.apply_lane_action(LaneAction::ResumeDelivery { lead: lead.clone() })
            }
            AttentionOp::Refresh => {}
        }
        self.attention.request(&lead);
    }
}
