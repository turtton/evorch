//! The lane card of a lane's thread header (ADR 0044): policy, latest Report status,
//! the brief and the branch/worktree declaration, read from the lane ledger.
//!
//! Phase 1b words `ready` as unverified: the lane's claim, not a checked result.

use event_bus::lane::{LaneBrief, LaneStatus};

use super::{AgentPaneAction, LaneContext};
use crate::model::lanes::{PREVIEW_NOTICE, status_label};
use crate::theme::{text::muted, tokens::*, widgets::badge};

/// Badge colors of a Report status; `ready` stays a warning color until verified.
fn status_colors(status: LaneStatus) -> (egui::Color32, egui::Color32) {
    let p = palette();
    match status {
        LaneStatus::Working => (p.RUNNING, p.SURFACE_RAISED),
        LaneStatus::Waiting => (p.WAITING, p.SURFACE_RAISED),
        LaneStatus::Blocked | LaneStatus::Ready => (p.WARNING_FG, p.WARNING_SURFACE),
        LaneStatus::Failed => (p.ERROR_FG, p.ERROR_SURFACE),
    }
}

pub(super) fn lane_card(
    ui: &mut egui::Ui,
    lane: &LaneContext<'_>,
    action: &mut Option<AgentPaneAction>,
) {
    let detail = lane.detail;
    let record = &detail.record;
    ui.horizontal_wrapped(|ui| {
        let policy = badge(
            ui,
            format!("lane · {}", record.policy.as_str()),
            palette().TEXT_MUTED,
            palette().SURFACE_RAISED,
        );
        policy.on_hover_text(
            "A lane is an independent Orchestrator thread. Its policy decides who manages it.",
        );
        match detail.status() {
            Some(status) => {
                let (fg, bg) = status_colors(status);
                let response = badge(ui, status_label(status), fg, bg);
                if status == LaneStatus::Ready {
                    response.on_hover_text(
                        "The lane says its work is done. Nothing has verified it yet.",
                    );
                }
            }
            None => {
                ui.label(muted("no report yet"));
            }
        }
        if let Some(origin) = lane.origin
            && record.lead_thread_id.is_none()
            && ui
                .small_button(format!("Opened from: {}", origin.title))
                .clicked()
        {
            *action = Some(AgentPaneAction::Sidebar(
                crate::panes::sidebar::SidebarAction::SwitchThread(origin.id.clone()),
            ));
        }
    });
    ui.label(muted(PREVIEW_NOTICE));
    if let Some(report) = &detail.latest_report {
        let mut line = format!("Report #{}: {}", report.report_seq, report.summary);
        if report.needs_human {
            line.push_str(" (needs a person)");
        }
        if let Some(code) = &report.reason_code {
            line.push_str(&format!(" [{code}]"));
        }
        ui.label(muted(line));
    }
    egui::CollapsingHeader::new("Brief")
        .id_salt(("lane-brief", &record.lane_id))
        .default_open(false)
        .show(ui, |ui| {
            brief_body(ui, &record.brief, &detail.workspace_lines())
        });
}

fn brief_body(ui: &mut egui::Ui, brief: &LaneBrief, workspace: &[String]) {
    ui.label(format!("Objective: {}", brief.objective));
    for (label, items) in [
        ("Deliverables", &brief.deliverables),
        ("Acceptance criteria", &brief.acceptance_criteria),
        ("Constraints", &brief.constraints),
    ] {
        if items.is_empty() {
            continue;
        }
        ui.label(muted(label));
        for item in items {
            ui.label(format!("• {item}"));
        }
    }
    if workspace.is_empty() {
        ui.label(muted("No branch or worktree declared"));
    } else {
        ui.label(muted(
            "Declared workspace (a declaration, not a reservation)",
        ));
        for line in workspace {
            ui.label(line);
        }
    }
}
