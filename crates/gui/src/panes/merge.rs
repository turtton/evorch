//! Merge approval pane.

use crate::model::commands::{CiStatus, MergeApprovalModel, MergeDecision, ReviewerStatus};
use crate::theme::text::muted;
use crate::theme::tokens::{
    ACCENT, ACCENT_FG, ERROR_FG, ERROR_SURFACE, R_SM, SUCCESS, SURFACE, SURFACE_RAISED, TEXT,
    TEXT_MUTED, WARNING_FG, WARNING_SURFACE,
};
use crate::theme::widgets::{badge, empty_state, pane_root, surface_frame};

const REASON_ID: &str = "merge-pane-reject-reason";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeAction {
    Decide(MergeDecision),
}

pub fn merge_pane(ui: &mut egui::Ui, merge: &MergeApprovalModel) -> Option<MergeAction> {
    let ctx = ui.ctx().clone();
    let view = &merge.view;
    let mut action = None;

    pane_root(ui, "Merge", |ui| {
        if let Some(pr) = &view.pr {
            surface_frame(SURFACE).show(ui, |ui| {
                ui.label(egui::RichText::new(format!("PR #{}", pr.number)).color(TEXT));
                ui.label(egui::RichText::new(&pr.title).color(TEXT));
                ui.label(muted(&pr.url));
            });
        } else {
            let _ = empty_state(
                ui,
                "no pull request",
                "Merge approval appears when a PR is bound.",
                None,
            );
        }
        ui.horizontal(|ui| {
            let (ci_text, ci_fg, ci_bg) = ci_badge_style(view.ci);
            badge(ui, ci_text, ci_fg, ci_bg);
            let (reviewer_text, reviewer_fg, reviewer_bg) = reviewer_badge_style(view.reviewer);
            badge(ui, reviewer_text, reviewer_fg, reviewer_bg);
        });
        if let Some(summary) = &view.diff_summary {
            ui.label(egui::RichText::new(summary).color(TEXT));
        }
        if let Some(binding) = &view.binding {
            ui.label(
                egui::RichText::new(format!("head: {}", short_id(&binding.head_sha))).color(TEXT),
            );
            ui.label(
                egui::RichText::new(format!("token: {}", short_id(&binding.token_id))).color(TEXT),
            );
        }
        for item in &view.gate {
            let color = if item.ok { SUCCESS } else { WARNING_FG };
            ui.label(
                egui::RichText::new(format!(
                    "gate: {} {}",
                    item.label,
                    if item.ok { "ok" } else { "missing" }
                ))
                .color(color),
            );
        }
        if let Some(reason) = &view.blocked {
            ui.label(egui::RichText::new(format!("blocked: {reason}")).color(ERROR_FG));
        }

        let resolved = view.resolution.is_some();
        let approve_enabled = view.binding.is_some() && view.blocked.is_none() && !resolved;
        if ui
            .add_enabled(
                approve_enabled,
                egui::Button::new(egui::RichText::new("Approve").color(ACCENT_FG))
                    .fill(ACCENT)
                    .corner_radius(egui::CornerRadius::same(R_SM)),
            )
            .clicked()
        {
            action = Some(MergeAction::Decide(MergeDecision::Approve));
        }

        let mut reason = ctx
            .data_mut(|data| data.get_temp::<String>(egui::Id::new(REASON_ID)))
            .unwrap_or_default();
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut reason)
                    .hint_text("reject reason")
                    .desired_width(200.0),
            );
            let reason_ready = !reason.trim().is_empty();
            if ui
                .add_enabled(!resolved && reason_ready, egui::Button::new("Reject"))
                .clicked()
            {
                action = Some(MergeAction::Decide(MergeDecision::Reject {
                    reason: reason.trim().to_owned(),
                }));
                reason.clear();
            }
        });
        ctx.data_mut(|data| data.insert_temp(egui::Id::new(REASON_ID), reason));

        match &view.resolution {
            Some(MergeDecision::Approve) => {
                ui.label(egui::RichText::new("resolved: approved").color(SUCCESS));
            }
            Some(MergeDecision::Reject { .. }) => {
                ui.label(egui::RichText::new("resolved: rejected").color(ERROR_FG));
            }
            None => {}
        }
        action
    })
}

fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

fn ci_badge_style(status: CiStatus) -> (&'static str, egui::Color32, egui::Color32) {
    match status {
        CiStatus::Unknown => ("ci: unknown", TEXT_MUTED, SURFACE_RAISED),
        CiStatus::Pending => ("ci: pending", WARNING_FG, WARNING_SURFACE),
        CiStatus::Passing => ("ci: passing", SUCCESS, SURFACE_RAISED),
        CiStatus::Failing => ("ci: failing", ERROR_FG, ERROR_SURFACE),
    }
}

fn reviewer_badge_style(status: ReviewerStatus) -> (&'static str, egui::Color32, egui::Color32) {
    match status {
        ReviewerStatus::Unknown => ("reviewer: unknown", TEXT_MUTED, SURFACE_RAISED),
        ReviewerStatus::Pending => ("reviewer: pending", WARNING_FG, WARNING_SURFACE),
        ReviewerStatus::Approved => ("reviewer: approved", SUCCESS, SURFACE_RAISED),
        ReviewerStatus::ChangesRequested => {
            ("reviewer: changes-requested", ERROR_FG, ERROR_SURFACE)
        }
    }
}
