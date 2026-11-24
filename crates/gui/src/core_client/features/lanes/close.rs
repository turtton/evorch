//! Ending a lane by a person's decision: close it as done (unverified) or discard it.
//!
//! The ledger records `human_closed` or `discarded`. Both only end the ledger entry: no run
//! is stopped, no thread or worktree is deleted and nothing is detached. What the person
//! reads comes from the core's close preview, and the revisions that preview named are the
//! ones the close command carries; a lane that moved since is refused by the ledger and
//! nothing is recorded. The dialog never sends a request again by itself.
use workbench_protocol::{
    lane_attention::LaneId,
    lane_management::{CloseLane, LaneCloseRecordKind, MAX_CLOSE_NOTE_BYTES},
    namespaces::lanes::{LaneArtifactRevisionKind, LaneClosePreview, LaneCurrentClose},
};

use crate::panes::lane_widgets::{self, CloseBodyView, CloseDialogView, ReadyView};
pub(super) use crate::panes::lane_widgets::{
    CloseChoice, CloseDialogAction as DialogAction, NO_READY_WARNING, OLDER_BRIEF_WARNING,
};
#[cfg(test)]
pub(super) use crate::panes::lane_widgets::{MEANING, REMAINS, RUNNING_WARNING};

/// The kind of a recorded close in words that never read like a verified one.
pub(super) const fn close_label(kind: LaneCloseRecordKind) -> &'static str {
    lane_widgets::close_kind_label(kind)
}

/// The row's badge of a lane whose close stands, named "Lane close: …" for assistive
/// technology as the sidebar's is.
pub(super) fn badge(ui: &mut egui::Ui, close: &LaneCurrentClose) {
    lane_widgets::close_badge(
        ui,
        lane_widgets::close_badge_text(close.kind),
        close_label(close.kind),
    );
}

/// The warning a person closing a lane as done reads about its Ready, if any. A Ready for an
/// earlier brief is for reference only.
pub(super) fn ready_warning(preview: &LaneClosePreview) -> Option<&'static str> {
    match &preview.ready {
        None => Some(NO_READY_WARNING),
        Some(ready) if ready.brief_revision != preview.brief_revision => Some(OLDER_BRIEF_WARNING),
        Some(_) => None,
    }
}

/// What the dialog has read so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClosePhase {
    Loading,
    Loaded(Box<LaneClosePreview>),
    Unavailable(String),
}

/// The open confirmation.
#[derive(Debug, Clone)]
pub(super) struct CloseDialog {
    /// Which opening of a confirmation this is, counted by the feature and never reused: an
    /// answer for an earlier one (a read or a close that was sent) must not act on this one,
    /// not even for the same lane.
    pub(super) generation: u64,
    pub(super) lane: LaneId,
    pub(super) choice: CloseChoice,
    /// The start of the lane's objective, as the row or detail that opened the dialog showed.
    pub(super) title: String,
    pub(super) phase: ClosePhase,
    pub(super) note: String,
    /// What the last confirmation answered, when it did not record the close.
    pub(super) notice: Option<String>,
    /// The lane moved after the preview was read: read again before deciding.
    pub(super) stale: bool,
    /// Which read the screen waits for; an answer for an older one is dropped.
    pub(super) read_seq: u64,
    /// A read has to be asked for the next time the screen is drawn.
    pub(super) read_due: bool,
}

impl CloseDialog {
    pub(super) fn new(generation: u64, lane: LaneId, choice: CloseChoice, title: String) -> Self {
        Self {
            generation,
            lane,
            choice,
            title,
            phase: ClosePhase::Loading,
            note: String::new(),
            notice: None,
            stale: false,
            read_seq: 0,
            read_due: true,
        }
    }

    /// Why this screen cannot decide, in a sentence; `None` when it can. Reading and typing
    /// change nothing, so a blocker is only ever a reason not to send.
    pub(super) fn blocker(&self, sending: bool) -> Option<String> {
        if sending {
            return Some("Waiting for the close result…".to_owned());
        }
        if self.stale {
            return Some(lane_widgets::STALE_BLOCKER.to_owned());
        }
        match &self.phase {
            ClosePhase::Loading => Some(lane_widgets::LOADING_BLOCKER.to_owned()),
            ClosePhase::Unavailable(reason) => Some(reason.clone()),
            ClosePhase::Loaded(preview) => preview
                .current_close
                .as_ref()
                .map(|close| lane_widgets::already_ended(close_label(close.kind))),
        }
    }

    /// The reason is required and bounded by what the ledger accepts.
    pub(super) fn note_error(&self) -> Option<&'static str> {
        let note = self.note.trim();
        if note.is_empty() {
            Some(lane_widgets::REASON_REQUIRED)
        } else if note.len() > MAX_CLOSE_NOTE_BYTES {
            Some(lane_widgets::REASON_TOO_LONG)
        } else {
            None
        }
    }

    pub(super) fn can_confirm(&self, sending: bool) -> bool {
        matches!(self.phase, ClosePhase::Loaded(_))
            && self.blocker(sending).is_none()
            && self.note_error().is_none()
    }

    /// The command for what is shown now: the revisions and the Ready are the preview's.
    pub(super) fn order(&self, sending: bool) -> Option<CloseLane> {
        let ClosePhase::Loaded(preview) = &self.phase else {
            return None;
        };
        self.can_confirm(sending).then(|| CloseLane {
            lane_id: self.lane.clone(),
            kind: self.choice.human_kind(),
            ready_seq: preview.ready.as_ref().map(|ready| ready.report_seq),
            expected_management_revision: preview.management_revision,
            expected_request_revision: preview.request_revision,
            note: self.note.trim().to_owned(),
        })
    }

    /// Whether an event for the lane means the preview on screen is no longer the lane.
    pub(super) fn moved_by(
        &self,
        lane: &workbench_protocol::namespaces::lanes::LaneSummary,
    ) -> bool {
        let ClosePhase::Loaded(preview) = &self.phase else {
            return false;
        };
        lane.management_revision != preview.management_revision
            || lane.brief_revision != preview.brief_revision
            || lane.current_close != preview.current_close
    }
}

/// Draws the confirmation. It decides nothing: it returns what was pressed.
pub(super) fn draw(
    ctx: &egui::Context,
    dialog: &mut CloseDialog,
    sending: bool,
) -> Option<DialogAction> {
    let blocker = dialog.blocker(sending);
    let note_error = dialog.note_error();
    let can_confirm = dialog.can_confirm(sending);
    let loading = matches!(dialog.phase, ClosePhase::Loading);
    let (running, body) = match &dialog.phase {
        ClosePhase::Loaded(preview) => (preview.running, Some(body(preview))),
        ClosePhase::Loading | ClosePhase::Unavailable(_) => (false, None),
    };
    lane_widgets::close_dialog(
        ctx,
        CloseDialogView {
            id: "core-client-lane-close",
            choice: dialog.choice,
            title: &dialog.title,
            running,
            blocker,
            loading,
            body,
            note: &mut dialog.note,
            note_max_height: Some(120.0),
            note_error,
            notice: dialog.notice.as_deref(),
            can_confirm,
        },
    )
}

/// What the core's preview says, as the shared confirmation shows it. The core sends no
/// review state or pull request facts with it, so those lines are left out.
fn body(preview: &LaneClosePreview) -> CloseBodyView<'_> {
    CloseBodyView {
        ready_warning: ready_warning(preview),
        open_questions: lane_widgets::open_questions_line(u64::from(preview.open_question_count)),
        objective: &preview.objective,
        acceptance_criteria: &preview.acceptance_criteria,
        ready: preview.ready.as_ref().map(|ready| {
            let revision = ready.artifact_revision.as_ref().map(|revision| {
                let kind = match revision.kind {
                    LaneArtifactRevisionKind::CommitSha => "commit",
                    LaneArtifactRevisionKind::Sha256 => "sha256",
                };
                (kind, revision.value.as_str())
            });
            let total = usize::try_from(ready.artifact_ref_count).unwrap_or(usize::MAX);
            ReadyView {
                seq: ready.report_seq.0,
                for_reference: ready.brief_revision != preview.brief_revision,
                summary: &ready.summary,
                evidence: lane_widgets::evidence_line(revision.is_some(), total),
                artifacts: lane_widgets::artifact_lines(revision, &ready.artifact_refs, total),
                artifact_count: total + usize::from(revision.is_some()),
            }
        }),
        review: None,
        pull_request: None,
    }
}
