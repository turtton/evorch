//! The confirmation for ending a lane by hand (ADR 0044 §9).
//!
//! It shows what the ledger says (the brief, the latest Ready, and the review, evidence and
//! pull request as reference only), what the operation means, and requires a reason. It
//! decides nothing: it returns Confirm, Reload or Cancel, and the app sends the revisions it
//! read to the trusted host path.

use crate::app::lane_close::{CloseDialog, ClosePhase};
use crate::model::lane_close;
use crate::panes::lane_widgets::{self, CloseBodyView, CloseDialogView, ReadyView};

pub use crate::panes::lane_widgets::{CloseDialogAction, NOTE_LABEL};

pub fn lane_close_dialog(
    ctx: &egui::Context,
    dialog: &mut CloseDialog,
    running: bool,
) -> Option<CloseDialogAction> {
    let blocker = dialog.blocker();
    let note_error = dialog.note_error();
    let can_confirm = dialog.can_confirm();
    let loading = matches!(dialog.phase, ClosePhase::Loading);
    let body = match &dialog.phase {
        ClosePhase::Loaded(preview) => {
            let brief = &preview.lane.brief;
            Some(CloseBodyView {
                ready_warning: lane_close::ready_warning(preview),
                open_questions: lane_close::open_questions_line(preview.open_question_count),
                objective: &brief.objective,
                acceptance_criteria: &brief.acceptance_criteria,
                ready: preview.ready.as_ref().map(|ready| ReadyView {
                    seq: ready.report_seq,
                    for_reference: lane_close::ready_answers_current_brief(preview) == Some(false),
                    summary: &ready.summary,
                    evidence: lane_close::evidence_line(ready),
                    artifacts: lane_close::artifact_lines(ready),
                    artifact_count: ready.artifact_refs.len()
                        + usize::from(ready.artifact_revision.is_some()),
                }),
                review: preview.readiness.map(lane_close::review_line),
                pull_request: Some(lane_close::pr_line(&preview.pr_context)),
            })
        }
        ClosePhase::Loading | ClosePhase::Unavailable(_) => None,
    };
    lane_widgets::close_dialog(
        ctx,
        CloseDialogView {
            id: "lane-close-dialog",
            choice: dialog.choice,
            title: &dialog.title,
            running,
            blocker,
            loading,
            body,
            note: &mut dialog.note,
            note_max_height: None,
            note_error,
            notice: dialog.notice.as_deref(),
            can_confirm,
        },
    )
}
