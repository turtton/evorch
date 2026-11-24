//! Lane words and widgets shared by the embedded workbench and the core client (ADR 0044,
//! ADR 0045): the close confirmation, the lane items of a thread's menu, the badges of a
//! lane card and the sentences they show.
//!
//! Pure drawing over protocol types: nothing here reads the runtime, storage or the event
//! bus. Each side builds the view from what it read (the embedded ledger, or the core's
//! replies) and gets back what was pressed, for its owner to send.

use egui::{Align, Layout, RichText, Sense, Ui, WidgetInfo, WidgetType};
use workbench_protocol::lane_attention::LaneReadyState;
use workbench_protocol::lane_management::{
    HumanLaneCloseKind, LaneCloseRecordKind, LaneManagementPolicy, ManagedLanePolicy,
};
use workbench_protocol::namespaces::lanes::{LaneAttentionTag, LaneReportStatus};

use crate::theme::text::{h3, medium, muted};
use crate::theme::tokens::*;
use crate::theme::widgets::{badge, ghost, primary_button, surface_frame};

/// What closing means, shown in the confirmation and as the tooltip of its buttons.
pub const MEANING: &str = "This only records the lane as ended in the ledger. It does not stop its run, delete its thread or worktree, or detach it. It is your decision, not a verification.";

/// What closing leaves as it is, said next to the meaning so nobody reads a close as cleanup.
pub const REMAINS: &str = "Its open questions stay open. If the lane reports again, the close is lifted and the lane reads as active.";

/// Shown when the lane's thread has a run going: the close does not stop it.
pub const RUNNING_WARNING: &str = "This lane is running now. Ending it here does not stop the run; stop it yourself if you want it to end.";

/// Shown when a person closes a lane that has not reported Ready for its brief.
pub const NO_READY_WARNING: &str = "This lane has not reported Ready for its current brief. You are ending it without a result to read.";

/// Shown when the latest Ready was reported for an earlier brief: it is only for reference.
pub const OLDER_BRIEF_WARNING: &str = "The latest Ready was reported for an earlier brief. The brief was revised after it, so it is shown for reference only and is not a result for the current brief.";

/// The label (and accessible name) of the reason field of the confirmation.
pub const NOTE_LABEL: &str = "Reason (required)";

/// Why the confirmation cannot decide: the lane moved after it was read.
pub const STALE_BLOCKER: &str = "The lane changed after this screen was read. Reload first.";

/// Why the confirmation cannot decide yet: the lane is still being read.
pub const LOADING_BLOCKER: &str = "Reading the lane from the ledger…";

pub const REASON_REQUIRED: &str = "A reason is required.";
pub const REASON_TOO_LONG: &str = "The reason is too long.";

/// What a refused close says when the lane moved after the screen was read.
pub const STALE_NOTICE: &str =
    "The lane changed after this screen was read; nothing was recorded. Reload to see it now.";

/// Why the confirmation has nothing to record: a close already stands.
#[must_use]
pub fn already_ended(label: &str) -> String {
    format!("This lane is already ended ({label}). Nothing more to record.")
}

#[must_use]
pub fn already_closed_notice(label: &str) -> String {
    format!("The lane was already ended ({label}); nothing was recorded.")
}

#[must_use]
pub fn closed_notice(label: &str) -> String {
    format!("Lane ended: {label}. No run was stopped and nothing was deleted.")
}

#[must_use]
pub fn not_recorded(message: &str) -> String {
    format!("Nothing was recorded: {message}")
}

/// Which ending the person chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseChoice {
    /// The person judges the lane done, with no verification behind it.
    Complete,
    /// The person ends the lane without claiming it is done.
    Discard,
}

impl CloseChoice {
    /// The entry of a managed lane's sidebar menu.
    #[must_use]
    pub const fn menu_label(self) -> &'static str {
        match self {
            Self::Complete => "Close by human decision…",
            Self::Discard => "Discard…",
        }
    }

    /// The button on the attention row.
    #[must_use]
    pub const fn button(self) -> &'static str {
        match self {
            Self::Complete => "Close…",
            Self::Discard => "Discard…",
        }
    }

    /// The confirm button of the dialog.
    #[must_use]
    pub const fn confirm(self) -> &'static str {
        match self {
            Self::Complete => "Close lane",
            Self::Discard => "Discard lane",
        }
    }

    /// The dialog's title.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Complete => "Close lane (unverified)",
            Self::Discard => "Discard lane",
        }
    }

    /// How the ledger will word the result.
    #[must_use]
    pub const fn recorded_as(self) -> &'static str {
        match self {
            Self::Complete => "Recorded as: completed by human decision (unverified)",
            Self::Discard => "Recorded as: discarded",
        }
    }

    /// Whether the request ends the lane without claiming it is done.
    #[must_use]
    pub const fn discards(self) -> bool {
        matches!(self, Self::Discard)
    }

    #[must_use]
    pub const fn human_kind(self) -> HumanLaneCloseKind {
        match self {
            Self::Complete => HumanLaneCloseKind::Complete,
            Self::Discard => HumanLaneCloseKind::Discard,
        }
    }
}

/// The kind of a recorded close in words that never read like a verified one.
#[must_use]
pub const fn close_kind_label(kind: LaneCloseRecordKind) -> &'static str {
    match kind {
        LaneCloseRecordKind::LeadClosed => "closed by lead",
        LaneCloseRecordKind::HumanClosed => "completed by human decision (unverified)",
        LaneCloseRecordKind::Discarded => "discarded",
    }
}

#[must_use]
pub const fn close_badge_text(kind: LaneCloseRecordKind) -> &'static str {
    match kind {
        LaneCloseRecordKind::LeadClosed => "closed",
        LaneCloseRecordKind::HumanClosed => "closed (unverified)",
        LaneCloseRecordKind::Discarded => "discarded",
    }
}

/// The review of the Ready in a sentence.
#[must_use]
pub const fn review_line(readiness: LaneReadyState) -> &'static str {
    match readiness {
        LaneReadyState::NoEvidence => "Review: none, and no fixed artifact is named.",
        LaneReadyState::Unreviewed => "Review: none yet; a fixed artifact is named.",
        LaneReadyState::ReviewInProgress => "Review: under way.",
        LaneReadyState::ReviewRejected => "Review: rejected this Ready.",
        LaneReadyState::ReviewIncomplete => "Review: ended without a complete result.",
        LaneReadyState::AwaitingClose => "Review: approved.",
    }
}

/// Most artifact references the confirmation lists; the rest are only counted.
pub const ARTIFACT_REFS_SHOWN: usize = 5;
/// Longest reference shown, in characters.
const ARTIFACT_REF_CHARS: usize = 200;

/// The artifact a Ready names, exactly as the lane reported it: the fixed revision (its kind
/// and value), then at most [`ARTIFACT_REFS_SHOWN`] of `refs`, then how many of the `total`
/// references are not listed. Nothing is checked or fetched.
#[must_use]
pub fn artifact_lines(
    revision: Option<(&str, &str)>,
    refs: &[String],
    total: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some((kind, value)) = revision {
        lines.push(format!("revision ({kind}): {}", clipped(value)));
    }
    let shown = refs.len().min(ARTIFACT_REFS_SHOWN);
    lines.extend(refs.iter().take(shown).map(|reference| clipped(reference)));
    let rest = total.saturating_sub(shown);
    if rest > 0 {
        lines.push(format!("… and {rest} more"));
    }
    lines
}

fn clipped(text: &str) -> String {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(ARTIFACT_REF_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// The artifact a Ready names, in a sentence.
#[must_use]
pub fn evidence_line(has_revision: bool, refs: usize) -> String {
    match (has_revision, refs) {
        (false, 0) => "Evidence: no fixed artifact and no references.".to_owned(),
        (false, n) => format!("Evidence: {n} reference(s), no fixed artifact revision."),
        (true, n) => format!("Evidence: a fixed artifact revision and {n} reference(s)."),
    }
}

/// How many unanswered questions the lane still holds, in a sentence; `None` when none.
#[must_use]
pub fn open_questions_line(count: u64) -> Option<String> {
    match count {
        0 => None,
        1 => Some("1 question from this lane is still unanswered. It stays open.".to_owned()),
        n => Some(format!(
            "{n} questions from this lane are still unanswered. They stay open."
        )),
    }
}

#[must_use]
pub const fn policy_word(policy: LaneManagementPolicy) -> &'static str {
    match policy {
        LaneManagementPolicy::Attached => "attached",
        LaneManagementPolicy::Summary => "summary",
        LaneManagementPolicy::Detached => "detached",
    }
}

/// The status as the UI words it: `ready` is only a claim.
#[must_use]
pub const fn status_label(status: LaneReportStatus) -> &'static str {
    match status {
        LaneReportStatus::Working => "working",
        LaneReportStatus::Waiting => "waiting",
        LaneReportStatus::Blocked => "blocked",
        LaneReportStatus::Ready => "ready (unverified)",
        LaneReportStatus::Failed => "failed",
    }
}

/// Where a lane's questions and notices go (ADR 0044 §7, §8), as one line for the lane card.
/// `delivery_bound` is whether the window runs the durable delivery host: without it the
/// runtime routes every question to the user and notifies nobody, so the words must not
/// promise more than that.
#[must_use]
pub const fn routing_notice(policy: LaneManagementPolicy, delivery_bound: bool) -> &'static str {
    match (delivery_bound, policy) {
        (false, _) => {
            "Lane delivery is not running in this window: this lane's questions come to you, and its lead is neither notified nor woken."
        }
        (true, LaneManagementPolicy::Attached) => {
            "Questions go to the lead first, which answers them or passes them to you. Questions, ready and failed reports are saved for the lead and wake it once it has finished."
        }
        (true, LaneManagementPolicy::Summary) => {
            "Questions come to you. The lead only receives a short ready or failed report; questions do not wake it."
        }
        (true, LaneManagementPolicy::Detached) => {
            "Questions come to you. This lane is detached: its lead receives no reports and is never woken."
        }
    }
}

/// Why a lane wants attention, in the words of the Notifications attention section.
#[must_use]
pub const fn attention_tag_label(tag: LaneAttentionTag) -> &'static str {
    match tag {
        LaneAttentionTag::QuestionPending => "Question",
        LaneAttentionTag::Blocked => "Blocked",
        LaneAttentionTag::Failed => "Failed",
        LaneAttentionTag::RestoreFailed => "Restore failed",
        LaneAttentionTag::ReadyUnclosed => "Ready (awaiting close)",
        LaneAttentionTag::QuestionCapacityBlocked => "Question limit",
        LaneAttentionTag::WakeSuppressed => "Wake paused",
        LaneAttentionTag::StopPending => "Notices held",
        LaneAttentionTag::Stall => "Stalled",
    }
}

pub const RESUME_NOTIFICATIONS: &str = "Resume notifications";
pub const RESUME_HOVER: &str = "Deliver this lane's held notices to its lead again";
pub const CONTINUE_LEAD: &str = "Continue lead";
pub const CONTINUE_LEAD_HOVER: &str =
    "Resume the lead's conversation; held notices are then delivered";
pub const LEAD_STOPPED: &str =
    "The lead was stopped: its notices are held, not lost, and it is not woken.";
pub const READY_HOVER: &str = "The lane says its work is done. Nothing has verified it yet.";
pub const POLICY_HOVER: &str =
    "A lane is an independent Orchestrator thread. Its policy decides who manages it.";

/// [`LEAD_STOPPED`] with the number of held notices, when the core counted them.
#[must_use]
pub fn held_line(count: u32) -> String {
    if count == 0 {
        LEAD_STOPPED.to_owned()
    } else {
        format!("The lead was stopped: {count} notice(s) are held, not lost, and it is not woken.")
    }
}

/// Badge colors of a close. A person's completion stays a warning color: nothing verified it.
#[must_use]
pub fn close_colors(kind: LaneCloseRecordKind) -> (egui::Color32, egui::Color32) {
    let p = palette();
    match kind {
        LaneCloseRecordKind::LeadClosed => (p.TEXT_MUTED, p.SURFACE_RAISED),
        LaneCloseRecordKind::HumanClosed => (p.WARNING_FG, p.WARNING_SURFACE),
        LaneCloseRecordKind::Discarded => (p.TEXT_MUTED, p.SURFACE_RAISED),
    }
}

/// Badge colors of a Report status; `ready` stays a warning color until verified.
#[must_use]
pub fn status_colors(status: LaneReportStatus) -> (egui::Color32, egui::Color32) {
    let p = palette();
    match status {
        LaneReportStatus::Working => (p.RUNNING, p.SURFACE_RAISED),
        LaneReportStatus::Waiting => (p.WAITING, p.SURFACE_RAISED),
        LaneReportStatus::Blocked | LaneReportStatus::Ready => (p.WARNING_FG, p.WARNING_SURFACE),
        LaneReportStatus::Failed => (p.ERROR_FG, p.ERROR_SURFACE),
    }
}

/// `lane · attached`: the lane's policy, with what a lane is on hover.
pub fn policy_badge(ui: &mut Ui, policy: &str) -> egui::Response {
    badge(
        ui,
        format!("lane · {policy}"),
        palette().TEXT_MUTED,
        palette().SURFACE_RAISED,
    )
    .on_hover_text(POLICY_HOVER)
}

/// The latest Report's status; a `ready` one says on hover that nothing verified it.
pub fn status_badge(ui: &mut Ui, status: LaneReportStatus) -> egui::Response {
    let (fg, bg) = status_colors(status);
    let response = badge(ui, status_label(status), fg, bg);
    if status == LaneReportStatus::Ready {
        response.on_hover_text(READY_HOVER)
    } else {
        response
    }
}

pub fn close_record_badge(ui: &mut Ui, kind: LaneCloseRecordKind) -> egui::Response {
    let (fg, bg) = close_colors(kind);
    badge(ui, close_kind_label(kind), fg, bg)
}

/// A pill saying the lane's close stands. It is the ledger's record, drawn next to the run's
/// own state and never in place of it.
pub fn close_badge(ui: &mut Ui, text: &str, hover: &str) {
    let label = format!("Lane close: {text}");
    let response = ui.add(
        egui::Label::new(
            RichText::new(text)
                .size(FONT_SMALL)
                .color(palette().TEXT_MUTED),
        )
        .sense(Sense::hover()),
    );
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, &label));
    response.on_hover_text(hover);
}

/// A lane's brief: the objective, then each non-empty section as a bulleted list, then the
/// branch and worktree it declared when the source has them (`None` skips those lines).
pub fn brief_body(
    ui: &mut Ui,
    objective: &str,
    sections: &[(&str, &[String])],
    workspace: Option<&[String]>,
) {
    ui.label(format!("Objective: {objective}"));
    for (label, items) in sections {
        if items.is_empty() {
            continue;
        }
        ui.label(muted(*label));
        for item in *items {
            ui.label(format!("• {item}"));
        }
    }
    match workspace {
        None => {}
        Some([]) => {
            ui.label(muted("No branch or worktree declared"));
        }
        Some(lines) => {
            ui.label(muted(
                "Declared workspace (a declaration, not a reservation)",
            ));
            for line in lines {
                ui.label(line);
            }
        }
    }
}

/// What was chosen in the lane items of a managed lane's thread menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneMenuChoice {
    Policy(ManagedLanePolicy),
    Detach,
    Close(CloseChoice),
}

/// The lane items of a managed lane's thread menu, ending with a separator. Close and
/// Discard are offered only where the window can record them (`can_close`).
pub fn lane_menu(ui: &mut Ui, can_close: bool) -> Option<LaneMenuChoice> {
    let mut choice = None;
    for (label, pick) in [
        (
            "Report milestones (attached)",
            LaneMenuChoice::Policy(ManagedLanePolicy::Attached),
        ),
        (
            "Report completion only (summary)",
            LaneMenuChoice::Policy(ManagedLanePolicy::Summary),
        ),
        ("Detach lane", LaneMenuChoice::Detach),
    ] {
        if ui.button(label).clicked() {
            choice = Some(pick);
            ui.close();
        }
    }
    ui.separator();
    if can_close {
        for close in [CloseChoice::Complete, CloseChoice::Discard] {
            if ui
                .button(close.menu_label())
                .on_hover_text(MEANING)
                .clicked()
            {
                choice = Some(LaneMenuChoice::Close(close));
                ui.close();
            }
        }
        ui.separator();
    }
    choice
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDialogAction {
    Confirm,
    Reload,
    Cancel,
}

/// The Ready a confirmation shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyView<'a> {
    pub seq: u64,
    /// The Ready answers an earlier brief: it is shown for reference only.
    pub for_reference: bool,
    pub summary: &'a str,
    pub evidence: String,
    pub artifacts: Vec<String>,
    /// How many artifact references (and the fixed revision) the Ready names in all.
    pub artifact_count: usize,
}

/// What a confirmation read from the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseBodyView<'a> {
    /// The warning about the Ready, shown only when closing as done.
    pub ready_warning: Option<&'static str>,
    pub open_questions: Option<String>,
    pub objective: &'a str,
    pub acceptance_criteria: &'a [String],
    pub ready: Option<ReadyView<'a>>,
    /// The review of the Ready; the line is skipped when the source has none.
    pub review: Option<&'static str>,
    /// What is known of the pull request; the line is skipped when the source has none.
    pub pull_request: Option<String>,
}

/// The confirmation for ending a lane by hand, as one frame draws it.
pub struct CloseDialogView<'a> {
    pub id: &'static str,
    pub choice: CloseChoice,
    /// The lane as the place it was chosen from names it.
    pub title: &'a str,
    pub running: bool,
    pub blocker: Option<String>,
    /// The blocker is only the read under way (muted rather than a warning).
    pub loading: bool,
    pub body: Option<CloseBodyView<'a>>,
    pub note: &'a mut String,
    /// Bounds the reason field to this height with its own scroll area; `None` lets it grow.
    pub note_max_height: Option<f32>,
    pub note_error: Option<&'static str>,
    pub notice: Option<&'a str>,
    pub can_confirm: bool,
}

/// The confirmation for ending a lane by hand (ADR 0044 §9).
///
/// It shows what the ledger says (the brief, the latest Ready, and the review, evidence and
/// pull request as reference only), what the operation means, and requires a reason. It
/// decides nothing: it returns Confirm, Reload or Cancel, and the owner sends the revisions
/// it read.
pub fn close_dialog(ctx: &egui::Context, view: CloseDialogView<'_>) -> Option<CloseDialogAction> {
    let CloseDialogView {
        id,
        choice,
        title,
        running,
        blocker,
        loading,
        body,
        note,
        note_max_height,
        note_error,
        notice,
        can_confirm,
    } = view;
    let mut action = None;
    let modal = egui::Modal::new(egui::Id::new(id))
        .backdrop_color(palette().OVERLAY)
        .frame(surface_frame(palette().SURFACE_RAISED))
        .show(ctx, |ui| {
            ui.set_width(
                (ctx.viewport_rect().width() * 0.6).min(PROVIDER_MODAL_MAX_WIDTH) - SP_4 * 4.0,
            );
            ui.spacing_mut().item_spacing = egui::vec2(SP_2, SP_2);
            ui.label(h3(choice.title()));
            ui.label(medium(title));
            ui.add(egui::Label::new(muted(MEANING)).wrap());
            ui.add(egui::Label::new(muted(REMAINS)).wrap());
            if running {
                warning(ui, RUNNING_WARNING);
            }
            if let Some(blocker) = blocker {
                let color = if loading {
                    palette().TEXT_MUTED
                } else {
                    palette().WARNING_FG
                };
                ui.add(egui::Label::new(RichText::new(blocker).color(color)).wrap());
            }
            if let Some(body) = &body {
                if choice == CloseChoice::Complete
                    && let Some(text) = body.ready_warning
                {
                    warning(ui, text);
                }
                if let Some(line) = &body.open_questions {
                    warning(ui, line);
                }
                egui::ScrollArea::vertical()
                    .id_salt("lane-close-body")
                    .max_height(260.0)
                    .show(ui, |ui| close_body(ui, body));
            }
            ui.separator();
            ui.label(muted(NOTE_LABEL));
            match note_max_height {
                Some(height) => {
                    egui::ScrollArea::vertical()
                        .id_salt("lane-close-note")
                        .max_height(height)
                        .show(ui, |ui| note_field(ui, note));
                }
                None => note_field(ui, note),
            }
            if let Some(error) = note_error.filter(|_| !note.is_empty()) {
                ui.colored_label(palette().ERROR_FG, error);
            }
            if let Some(notice) = notice {
                ui.add(egui::Label::new(RichText::new(notice).color(palette().ERROR_FG)).wrap());
            }
            ui.label(muted(choice.recorded_as()));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let confirm = ui
                    .add_enabled_ui(can_confirm, |ui| primary_button(ui, choice.confirm()))
                    .inner;
                if confirm.clicked() && can_confirm {
                    action = Some(CloseDialogAction::Confirm);
                }
                if ui.add(ghost("Reload")).clicked() {
                    action = Some(CloseDialogAction::Reload);
                }
                if ui.add(ghost("Cancel")).clicked() {
                    action = Some(CloseDialogAction::Cancel);
                }
            });
        });
    if action.is_none() && modal.should_close() {
        action = Some(CloseDialogAction::Cancel);
    }
    action
}

fn note_field(ui: &mut Ui, note: &mut String) {
    let edit = ui.add(
        egui::TextEdit::multiline(note)
            .desired_rows(3)
            .hint_text("Why are you ending this lane?")
            .desired_width(f32::INFINITY),
    );
    edit.widget_info(|| WidgetInfo::labeled(WidgetType::TextEdit, true, NOTE_LABEL));
}

fn close_body(ui: &mut Ui, body: &CloseBodyView<'_>) {
    ui.label(medium("Brief"));
    ui.add(egui::Label::new(body.objective).wrap());
    for criterion in body.acceptance_criteria {
        ui.add(egui::Label::new(muted(format!("• {criterion}"))).wrap());
    }
    ui.separator();
    match &body.ready {
        Some(ready) => {
            ui.label(medium(if ready.for_reference {
                format!("Latest Ready (#{}), for an earlier brief", ready.seq)
            } else {
                format!("Latest Ready (#{})", ready.seq)
            }));
            ui.add(egui::Label::new(ready.summary).wrap());
            ui.label(muted(&ready.evidence));
            if !ready.artifacts.is_empty() {
                egui::CollapsingHeader::new(format!(
                    "Artifact references ({})",
                    ready.artifact_count
                ))
                .id_salt("lane-close-artifacts")
                .default_open(false)
                .show(ui, |ui| {
                    for line in &ready.artifacts {
                        ui.add(egui::Label::new(RichText::new(line).monospace()).wrap());
                    }
                });
            }
        }
        None => {
            ui.label(medium("No Ready report"));
            ui.label(muted(
                "The lane has not reported Ready for its current brief.",
            ));
        }
    }
    if let Some(review) = body.review {
        ui.label(muted(review));
    }
    if let Some(pull_request) = &body.pull_request {
        ui.label(muted(pull_request));
    }
    ui.label(muted(
        "Review, evidence and the pull request are reference only; none of them is required to proceed.",
    ));
}

fn warning(ui: &mut Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).color(palette().WARNING_FG)).wrap());
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::{NodeT, Queryable};

    #[test]
    fn artifact_lines_count_the_references_the_source_did_not_list() {
        let refs: Vec<String> = (0..7).map(|n| format!("ref-{n}")).collect();
        let lines = artifact_lines(Some(("commit", "abc")), &refs, 12);
        assert_eq!(lines[0], "revision (commit): abc");
        assert_eq!(lines.len(), 1 + ARTIFACT_REFS_SHOWN + 1);
        assert_eq!(lines.last().unwrap(), "… and 7 more");
        let few = artifact_lines(None, &refs[..1], 3);
        assert_eq!(few, ["ref-0".to_owned(), "… and 2 more".to_owned()]);
        assert!(artifact_lines(None, &[], 0).is_empty());
        assert_eq!(held_line(0), LEAD_STOPPED);
        assert!(held_line(2).contains("2 notice(s) are held"));
    }

    #[test]
    fn the_close_badge_names_the_ledger_record_for_assistive_technology() {
        let harness = Harness::new_ui(|ui| {
            close_badge(
                ui,
                close_badge_text(LaneCloseRecordKind::HumanClosed),
                "hover",
            );
        });
        harness.get_by_label("Lane close: closed (unverified)");
    }

    #[test]
    fn the_dialog_titles_a_discard_as_a_label_and_confirms_it_with_a_button() {
        let mut note = String::new();
        let harness = Harness::new_ui(move |ui| {
            close_dialog(
                ui.ctx(),
                CloseDialogView {
                    id: "lane-widgets-test",
                    choice: CloseChoice::Discard,
                    title: "the lane",
                    running: false,
                    blocker: None,
                    loading: false,
                    body: None,
                    note: &mut note,
                    note_max_height: None,
                    note_error: Some(REASON_REQUIRED),
                    notice: None,
                    can_confirm: false,
                },
            );
        });
        let title = harness.get_by_role_and_label(egui::accesskit::Role::Label, "Discard lane");
        assert!(!title.accesskit_node().is_disabled());
        let confirm = harness.get_by_role_and_label(egui::accesskit::Role::Button, "Discard lane");
        assert!(confirm.accesskit_node().is_disabled());
        harness.get_by_label(MEANING);
    }
}
