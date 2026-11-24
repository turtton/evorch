//! What a lane shows of itself: its policy and latest Report, what wants attention, the
//! notices held for its lead, and the brief. The Lanes tab draws it for the opened lane, and
//! the conversation of a lane's thread draws it under its header, as the embedded
//! workbench's lane card.
use egui::Ui;
use workbench_protocol::{
    ids::ThreadId,
    lane_attention::LaneAttentionAction,
    namespaces::lanes::{LaneDetail, LaneSummary},
};

use crate::panes::lane_widgets::{
    CONTINUE_LEAD, CONTINUE_LEAD_HOVER, RESUME_HOVER, RESUME_NOTIFICATIONS, attention_tag_label,
    brief_body, close_record_badge, held_line, policy_badge, policy_word, review_line,
    routing_notice, status_badge,
};
use crate::theme::text::muted;
use crate::theme::tokens::palette;

pub struct LaneCardView<'a> {
    pub lead: &'a ThreadId,
    pub summary: &'a LaneSummary,
    pub detail: Option<&'a LaneDetail>,
    /// The lead's current root run is known, so held notices can be resumed for it.
    pub can_resume: bool,
    pub status: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardChoice {
    ResumeNotifications,
    ContinueLead,
}

/// Where a lane card is drawn, which decides the operations it offers for held notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardPlace {
    /// The Lanes tab's detail of the opened lane: Resume notifications is offered for held
    /// notices too, and the core answers whether the lead can take them.
    LanesTab,
    /// The conversation of a lane's thread. A stopped lead is continued, not resumed, as in
    /// the embedded workbench's card: held notices offer only "Continue lead", and only when
    /// `can_continue` says the lead can be continued from here.
    Conversation { can_continue: bool },
}

/// Draws the lane, with the operations `place` offers. The core always runs lane delivery,
/// so the routing line says where questions and notices go with delivery running.
pub fn lane_body(ui: &mut Ui, view: &LaneCardView<'_>, place: CardPlace) -> Option<CardChoice> {
    let mut choice = None;
    let summary = view.summary;
    ui.horizontal_wrapped(|ui| {
        // A badge keeps its text on one line, however narrow the tab.
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
        policy_badge(ui, policy_word(summary.policy));
        match (&summary.current_close, summary.latest_report_status) {
            (Some(close), _) => {
                close_record_badge(ui, close.kind);
            }
            (None, Some(status)) => {
                status_badge(ui, status);
            }
            (None, None) => {
                ui.label(muted("no report yet"));
            }
        }
    });
    // The routing line follows the badges, as in the embedded workbench's card.
    ui.add(egui::Label::new(muted(routing_notice(summary.policy, true))).wrap());
    if let Some(detail) = view.detail {
        for item in &detail.attention {
            ui.colored_label(
                palette().WARNING_FG,
                format!("{}: {}", attention_tag_label(item.tag), item.summary),
            );
        }
        let held = detail.delivery.held;
        if held {
            ui.colored_label(
                palette().WARNING_FG,
                held_line(detail.delivery.pending_count),
            );
        }
        let attention_resumes = detail.attention.iter().any(|item| {
            item.available_actions
                .contains(&LaneAttentionAction::Resume)
        });
        let (resumable, continuable) = match place {
            CardPlace::LanesTab => (view.can_resume && (held || attention_resumes), false),
            CardPlace::Conversation { can_continue } => (
                view.can_resume && !held && attention_resumes,
                held && can_continue,
            ),
        };
        if resumable || continuable {
            ui.horizontal_wrapped(|ui| {
                if resumable
                    && ui
                        .small_button(RESUME_NOTIFICATIONS)
                        .on_hover_text(RESUME_HOVER)
                        .clicked()
                {
                    choice = Some(CardChoice::ResumeNotifications);
                }
                if continuable
                    && ui
                        .small_button(CONTINUE_LEAD)
                        .on_hover_text(CONTINUE_LEAD_HOVER)
                        .clicked()
                {
                    choice = Some(CardChoice::ContinueLead);
                }
            });
        }
        if let Some(report) = &detail.latest_report {
            let mut line = format!("Report #{}: {}", report.seq.0, report.summary);
            if report.needs_human {
                line.push_str(" (needs a person)");
            }
            ui.label(muted(line));
        }
        if let Some(ready) = &detail.ready {
            ui.add(
                egui::Label::new(muted(format!(
                    "The Ready answers brief revision {}{}. It does not block a close.",
                    ready.brief_revision.0,
                    if ready.brief_changed_since {
                        ", and the brief was revised after it"
                    } else {
                        ""
                    }
                )))
                .wrap(),
            );
            ui.label(muted(review_line(ready.readiness)));
        }
    }
    egui::CollapsingHeader::new("Brief")
        .id_salt(("lane-brief", summary.lane_id.as_str()))
        .default_open(false)
        .show(ui, |ui| {
            brief_body(ui, &summary.objective_preview, &[], None)
        });
    if !view.status.is_empty() {
        ui.label(muted(view.status));
    }
    choice
}
