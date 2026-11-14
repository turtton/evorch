//! Ending a lane by hand from the attention list (ADR 0044 §9).
//!
//! A person chooses Close or Discard on a row. The window then reads the lane from the ledger
//! (off the UI thread), shows what it found next to the meaning of the operation, and asks for
//! a reason. Confirming sends the revisions that read returned to the trusted host path, which
//! records `human_closed` or `discarded` under the surface and client the host authenticated.
//! The ledger compares those revisions inside the close, so a lane that moved after the screen
//! was read is refused and nothing is recorded.
//!
//! Only the ledger entry ends: no run is stopped, no thread or worktree deleted, nothing
//! detached. Review state, evidence and pull request facts are shown for reference and never
//! decide whether the person may close.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use event_bus::lane::{LaneCloseOutcome, LaneCloseResult};
use storage::Database;
use storage::lane_control::LaneClosePreview;
use workbench_protocol::lane_attention::LaneAttentionKind;

use super::WorkbenchState;
use crate::model::lane_attention::AttentionAction;
use crate::model::lane_close::{self, CloseChoice};
use crate::model::tasks::AgentRunSource;

/// Reads what a person needs to decide, from one snapshot of the ledger. Blocking: it always
/// runs on a worker thread.
pub trait LaneClosePreviewReader: Send + Sync + 'static {
    /// # Errors
    /// A message when the ledger cannot be read or the lane is not attached.
    fn read(&self, lane_id: &str) -> Result<LaneClosePreview, String>;
}

/// Reads through a read-only connection to the shared database.
pub struct StorageLaneClosePreviewReader {
    db_path: PathBuf,
}

impl StorageLaneClosePreviewReader {
    #[must_use]
    pub const fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }
}

impl LaneClosePreviewReader for StorageLaneClosePreviewReader {
    fn read(&self, lane_id: &str) -> Result<LaneClosePreview, String> {
        Database::open_read_only(&self.db_path)
            .and_then(|db| db.lane_close_preview(lane_id))
            .map_err(|error| error.to_string())
    }
}

/// What the person confirmed. It carries no actor: the host fills in the surface and client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneCloseOrder {
    pub lane_id: String,
    pub ready_seq: Option<u64>,
    pub expected_management_revision: u64,
    pub expected_request_revision: u64,
    pub discard: bool,
    pub note: String,
}

/// Why the host did not record the close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneCloseFailure {
    /// The lane moved after the screen was read; nothing was recorded.
    pub stale: bool,
    pub message: String,
}

/// The trusted human path to the ledger. The desktop installs the adapter that authenticates
/// its own provenance; nothing an agent can reach implements this.
pub trait LaneCloseHost: Send + Sync {
    /// # Errors
    /// [`LaneCloseFailure`] with `stale` set when a revision no longer matches.
    fn close(&self, order: LaneCloseOrder) -> Result<LaneCloseResult, LaneCloseFailure>;
}

/// What the dialog has read so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClosePhase {
    Loading,
    Loaded(Box<LaneClosePreview>),
    Unavailable(String),
}

/// The open confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseDialog {
    pub choice: CloseChoice,
    pub lane_id: String,
    /// The lane's thread, when the sidebar has it.
    pub lane_thread: Option<String>,
    pub title: String,
    pub phase: ClosePhase,
    /// The reason; required.
    pub note: String,
    /// What the last confirmation answered, when it did not record the close.
    pub notice: Option<String>,
    /// A confirmation was refused as stale: reload before deciding again.
    pub stale: bool,
    /// What the screen is held to: the revisions the list item named, or `None` when the
    /// next read is what the person is shown (a lane chosen from the sidebar, or after
    /// Reload). The ledger still compares the revisions of the read that was displayed.
    listed: Option<Listed>,
    seq: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Listed {
    management_revision: u64,
    ready_seq: Option<u64>,
}

impl CloseDialog {
    /// Why this screen cannot decide, in a sentence; `None` when it can. Reading and typing
    /// never change the ledger, so a blocker is only ever a reason not to send.
    #[must_use]
    pub fn blocker(&self) -> Option<String> {
        if self.stale {
            return Some("The lane changed after this screen was read. Reload first.".to_owned());
        }
        match &self.phase {
            ClosePhase::Loading => Some("Reading the lane from the ledger…".to_owned()),
            ClosePhase::Unavailable(reason) => Some(reason.clone()),
            ClosePhase::Loaded(preview) => {
                if let Some(close) = &preview.current_close {
                    return Some(format!(
                        "This lane is already ended ({}). Nothing more to record.",
                        lane_close::close_label(close)
                    ));
                }
                self.listed.and_then(|listed| {
                    lane_close::moved_since_listed(
                        preview,
                        listed.management_revision,
                        listed.ready_seq,
                    )
                })
            }
        }
    }

    /// The reason is required and bounded by what the ledger accepts.
    #[must_use]
    pub fn note_error(&self) -> Option<&'static str> {
        let note = self.note.trim();
        if note.is_empty() {
            Some("A reason is required.")
        } else if note.len() > lane_close::NOTE_MAX_BYTES {
            Some("The reason is too long.")
        } else {
            None
        }
    }

    /// Whether Confirm may be pressed.
    #[must_use]
    pub fn can_confirm(&self) -> bool {
        matches!(self.phase, ClosePhase::Loaded(_))
            && self.blocker().is_none()
            && self.note_error().is_none()
    }
}

struct Loaded {
    seq: u64,
    result: Result<LaneClosePreview, String>,
}

/// The close side of the Workbench: the installed paths and the open dialog.
pub(super) struct LaneCloseState {
    reader: Option<Arc<dyn LaneClosePreviewReader>>,
    host: Option<Arc<dyn LaneCloseHost>>,
    pub(super) dialog: Option<CloseDialog>,
    next_seq: u64,
    results: Receiver<Loaded>,
    sender: Sender<Loaded>,
}

impl Default for LaneCloseState {
    fn default() -> Self {
        let (sender, results) = channel();
        Self {
            reader: None,
            host: None,
            dialog: None,
            next_seq: 0,
            results,
            sender,
        }
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Offer Close and Discard on the attention list, reading through `reader` and recording
    /// through `host`.
    #[must_use]
    pub fn with_lane_close(
        mut self,
        reader: Arc<dyn LaneClosePreviewReader>,
        host: Arc<dyn LaneCloseHost>,
    ) -> Self {
        self.lane_close.reader = Some(reader);
        self.lane_close.host = Some(host);
        self
    }

    /// Whether this window can end a lane by hand.
    #[must_use]
    pub const fn can_close_lanes(&self) -> bool {
        self.lane_close.reader.is_some() && self.lane_close.host.is_some()
    }

    /// The badge of every lane thread whose close stands, from what the ledger last said.
    pub(super) fn lane_close_badges(
        &self,
    ) -> std::collections::BTreeMap<workspace_ui::ThreadId, lane_close::LaneCloseBadge> {
        self.sidebar
            .threads
            .iter()
            .filter(|thread| thread.lane.is_some())
            .filter_map(|thread| {
                let close = self.lane_detail(&thread.id)?.close.as_ref()?;
                Some((thread.id.clone(), lane_close::LaneCloseBadge::of(close)))
            })
            .collect()
    }

    /// The open confirmation, if any.
    #[must_use]
    pub const fn lane_close_dialog(&self) -> Option<&CloseDialog> {
        self.lane_close.dialog.as_ref()
    }

    /// Opens the confirmation for the item the person chose, if the list still holds it.
    /// Opening changes nothing: the ledger is only read.
    pub(super) fn begin_lane_close(&mut self, action: &AttentionAction, choice: CloseChoice) {
        if !self.can_close_lanes() {
            self.push_notice("Ending a lane is not available in this window.");
            return;
        }
        let item = action.item.as_ref().and_then(|(id, revision)| {
            self.attention
                .board
                .items()
                .into_iter()
                .find(|item| {
                    item.lead_thread.as_str() == action.lead
                        && item.id.as_str() == id
                        && item.revision.get() == *revision
                })
                .cloned()
        });
        let Some(item) = item else {
            self.push_notice("That item changed; the list was read again.");
            self.attention.request(&action.lead);
            return;
        };
        let listed_ready_seq = match &item.kind {
            LaneAttentionKind::ReadyUnclosed { ready_seq, .. } => Some(ready_seq.0),
            _ => None,
        };
        let thread = self.sidebar.threads.iter().find(|thread| {
            thread
                .lane
                .as_ref()
                .is_some_and(|lane| lane.lane_id == item.lane.as_str())
        });
        let (lane_thread, title) = thread.map_or_else(
            || (None, item.lane.as_str().to_owned()),
            |thread| (Some(thread.id.to_string()), thread.title.clone()),
        );
        self.lane_close.dialog = Some(CloseDialog {
            choice,
            lane_id: item.lane.as_str().to_owned(),
            lane_thread,
            title,
            phase: ClosePhase::Loading,
            note: String::new(),
            notice: None,
            stale: false,
            listed: Some(Listed {
                management_revision: item.management_revision.get(),
                ready_seq: listed_ready_seq,
            }),
            seq: 0,
        });
        self.start_close_read();
    }

    /// Opens the confirmation for a managed lane chosen from the sidebar. The lane needs no
    /// attention item and no Ready: the next read is what the person is shown and decides on.
    pub(super) fn begin_lane_close_for_thread(
        &mut self,
        thread: &workspace_ui::ThreadId,
        choice: CloseChoice,
    ) {
        if !self.can_close_lanes() {
            self.push_notice("Ending a lane is not available in this window.");
            return;
        }
        let Some((lane_id, title)) = self
            .sidebar
            .threads
            .iter()
            .find(|candidate| candidate.id == *thread)
            .and_then(|candidate| {
                candidate
                    .lane
                    .as_ref()
                    .filter(|lane| lane.policy.is_managed())
                    .map(|lane| (lane.lane_id.clone(), candidate.title.clone()))
            })
        else {
            self.push_notice("Only a lane its lead manages can be ended here.");
            return;
        };
        self.lane_close.dialog = Some(CloseDialog {
            choice,
            lane_id,
            lane_thread: Some(thread.to_string()),
            title,
            phase: ClosePhase::Loading,
            note: String::new(),
            notice: None,
            stale: false,
            listed: None,
            seq: 0,
        });
        self.start_close_read();
    }

    /// Reads the lane again for the open dialog, keeping what the person typed.
    pub(super) fn reload_lane_close(&mut self) {
        if let Some(dialog) = &mut self.lane_close.dialog {
            dialog.phase = ClosePhase::Loading;
            dialog.notice = None;
            dialog.stale = false;
            // A refused confirmation means the screen was older than the ledger. The new
            // read replaces what the person is held to; nothing is sent until they confirm
            // it again.
            dialog.listed = None;
        }
        self.start_close_read();
    }

    pub(super) fn cancel_lane_close(&mut self) {
        self.lane_close.dialog = None;
    }

    fn start_close_read(&mut self) {
        let Some(reader) = self.lane_close.reader.clone() else {
            return;
        };
        let Some(dialog) = &mut self.lane_close.dialog else {
            return;
        };
        self.lane_close.next_seq += 1;
        let seq = self.lane_close.next_seq;
        dialog.seq = seq;
        let lane_id = dialog.lane_id.clone();
        let sender = self.lane_close.sender.clone();
        let (spawner, waker) = self.lanes.runner();
        spawner(Box::new(move || {
            let result = reader.read(&lane_id);
            let _ = sender.send(Loaded { seq, result });
            waker();
        }));
    }

    /// Takes the finished reads; one that is not the latest asked for is dropped.
    pub(super) fn pump_lane_close(&mut self) {
        while let Ok(loaded) = self.lane_close.results.try_recv() {
            let Some(dialog) = &mut self.lane_close.dialog else {
                continue;
            };
            if dialog.seq != loaded.seq {
                continue;
            }
            dialog.phase = match loaded.result {
                Ok(preview) => {
                    // The person is shown this read, so it is what they decide on.
                    dialog.listed.get_or_insert(Listed {
                        management_revision: preview.context.management_revision,
                        ready_seq: preview.context.latest_ready_seq,
                    });
                    ClosePhase::Loaded(Box::new(preview))
                }
                Err(reason) => ClosePhase::Unavailable(reason),
            };
        }
    }

    /// Sends the confirmed close to the trusted host path with the revisions the dialog read.
    pub(super) fn confirm_lane_close(&mut self) {
        let Some(host) = self.lane_close.host.clone() else {
            return;
        };
        let Some(dialog) = &mut self.lane_close.dialog else {
            return;
        };
        let ClosePhase::Loaded(preview) = &dialog.phase else {
            return;
        };
        if !dialog.can_confirm() {
            return;
        }
        let order = LaneCloseOrder {
            lane_id: dialog.lane_id.clone(),
            ready_seq: preview.context.latest_ready_seq,
            expected_management_revision: preview.context.management_revision,
            expected_request_revision: preview.context.request_revision,
            discard: dialog.choice.discards(),
            note: dialog.note.trim().to_owned(),
        };
        match host.close(order) {
            Ok(result) => {
                let lane_thread = dialog.lane_thread.clone();
                let words = lane_close::close_label(&result);
                let notice = if result.outcome == LaneCloseOutcome::AlreadyClosed {
                    format!("The lane was already ended ({words}); nothing was recorded.")
                } else {
                    format!("Lane ended: {words}. No run was stopped and nothing was deleted.")
                };
                self.lane_close.dialog = None;
                self.push_notice(notice);
                self.refresh_lane_attention();
                if let Some(thread) = lane_thread {
                    self.request_delivery_reads(&thread);
                }
            }
            Err(failure) => {
                dialog.stale = failure.stale;
                dialog.notice = Some(if failure.stale {
                    "The lane changed after this screen was read; nothing was recorded. Reload to see it now.".to_owned()
                } else {
                    format!("Nothing was recorded: {}", failure.message)
                });
            }
        }
    }

    /// Whether the lane of the open confirmation has a run going in its thread. The close
    /// never stops it; the screen only says so.
    fn lane_close_running(&self) -> bool {
        let Some(dialog) = &self.lane_close.dialog else {
            return false;
        };
        self.sidebar.threads.iter().any(|thread| {
            dialog.lane_thread.as_deref() == Some(thread.id.to_string().as_str())
                && matches!(
                    thread.state(&self.phases),
                    workspace_ui::ThreadState::Running
                )
        })
    }

    /// Draws the confirmation and runs its answer.
    pub(super) fn render_lane_close_dialog(&mut self, ctx: &egui::Context) {
        let running = self.lane_close_running();
        let Some(dialog) = &mut self.lane_close.dialog else {
            return;
        };
        match crate::panes::lane_close::lane_close_dialog(ctx, dialog, running) {
            Some(crate::panes::lane_close::CloseDialogAction::Confirm) => {
                self.confirm_lane_close();
            }
            Some(crate::panes::lane_close::CloseDialogAction::Reload) => self.reload_lane_close(),
            Some(crate::panes::lane_close::CloseDialogAction::Cancel) => self.cancel_lane_close(),
            None => {}
        }
    }
}
