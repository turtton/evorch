//! Merge approval pane.

use crate::model::commands::{CiStatus, MergeApprovalModel, MergeDecision, ReviewerStatus};

const REASON_ID: &str = "merge-pane-reject-reason";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeAction {
    Decide(MergeDecision),
}

pub fn merge_pane(ui: &mut egui::Ui, merge: &MergeApprovalModel) -> Option<MergeAction> {
    let ctx = ui.ctx().clone();
    let view = &merge.view;
    let mut action = None;

    ui.heading("Merge");
    if let Some(pr) = &view.pr {
        ui.label(format!("PR #{}", pr.number));
        ui.label(&pr.title);
        ui.label(&pr.url);
    } else {
        ui.label("no pull request");
    }
    ui.label(ci_badge(view.ci));
    ui.label(reviewer_badge(view.reviewer));
    if let Some(summary) = &view.diff_summary {
        ui.label(summary);
    }

    let resolved = view.resolution.is_some();
    if ui
        .add_enabled(!resolved, egui::Button::new("Approve"))
        .clicked()
    {
        action = Some(MergeAction::Decide(MergeDecision::Approve));
    }

    let mut reason = ctx
        .data_mut(|data| data.get_temp::<String>(egui::Id::new(REASON_ID)))
        .unwrap_or_default();
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
    ctx.data_mut(|data| data.insert_temp(egui::Id::new(REASON_ID), reason));

    match &view.resolution {
        Some(MergeDecision::Approve) => {
            ui.label("resolved: approved");
        }
        Some(MergeDecision::Reject { .. }) => {
            ui.label("resolved: rejected");
        }
        None => {}
    }
    action
}

fn ci_badge(status: CiStatus) -> &'static str {
    match status {
        CiStatus::Unknown => "ci: unknown",
        CiStatus::Pending => "ci: pending",
        CiStatus::Passing => "ci: passing",
        CiStatus::Failing => "ci: failing",
    }
}

fn reviewer_badge(status: ReviewerStatus) -> &'static str {
    match status {
        ReviewerStatus::Unknown => "reviewer: unknown",
        ReviewerStatus::Pending => "reviewer: pending",
        ReviewerStatus::Approved => "reviewer: approved",
        ReviewerStatus::ChangesRequested => "reviewer: changes-requested",
    }
}
