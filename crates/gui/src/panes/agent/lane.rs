//! The lane card of a lane's thread header (ADR 0044): policy, latest Report status,
//! the brief and the branch/worktree declaration, read from the lane ledger.
//!
//! `ready` is worded as unverified: the lane's claim, not a checked result. Delivery
//! notices (capacity, paused wake, stopped lead) come with the operation that clears them.

use event_bus::lane::{LaneBrief, LaneStatus};

use super::{AgentPaneAction, LaneAction, LaneContext, LeadContext};
use crate::model::lanes::{LaneDeliveryView, routing_notice, status_label};
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
    ui.label(muted(routing_notice(record.policy, lane.delivery_bound)));
    delivery_lines(
        ui,
        &detail.delivery,
        record.lead_thread_id.as_deref(),
        !detail.delivery.lead_stopped,
        action,
    );
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

/// The delivery notices of one lane, each with the operation that clears it.
fn delivery_lines(
    ui: &mut egui::Ui,
    delivery: &LaneDeliveryView,
    lead: Option<&str>,
    resumable: bool,
    action: &mut Option<AgentPaneAction>,
) {
    if let Some(line) = delivery.capacity_line() {
        ui.colored_label(palette().WARNING_FG, line);
    }
    if let Some(line) = delivery.suppressed_line() {
        ui.colored_label(palette().WARNING_FG, line);
        if let Some(lead) = lead
            && resumable
            && ui
                .small_button("Resume notifications")
                .on_hover_text("Deliver this lane's held notices to its lead again")
                .clicked()
        {
            *action = Some(AgentPaneAction::Lane(LaneAction::ResumeDelivery {
                lead: lead.to_owned(),
            }));
        }
    }
    if delivery.lead_stopped {
        ui.colored_label(
            palette().WARNING_FG,
            "The lead was stopped: its notices are held, not lost, and it is not woken.",
        );
        if let Some(lead) = lead
            && ui
                .small_button("Continue lead")
                .on_hover_text("Resume the lead's conversation; held notices are then delivered")
                .clicked()
        {
            *action = Some(AgentPaneAction::Lane(LaneAction::ContinueLead {
                lead: lead.to_owned(),
            }));
        }
    }
}

/// What a lead thread shows of the lanes it manages: questions waiting for its answer and
/// the delivery notices of those lanes. Nothing is drawn while there is nothing to say.
pub(super) fn lead_card(
    ui: &mut egui::Ui,
    lead: &LeadContext<'_>,
    action: &mut Option<AgentPaneAction>,
) {
    if lead.waiting_questions > 0 {
        ui.label(muted(format!(
            "{} lane question(s) wait for your answer here. Read them with lane_questions; they are not in your question list.",
            lead.waiting_questions
        )));
    }
    // A stopped lead is continued, not "resumed": its Stop stays until the user continues it.
    let stopped = lead.lanes.iter().any(|lane| lane.delivery.lead_stopped);
    for lane in &lead.lanes {
        if lane.delivery.capacity_blocked.is_none() && !lane.delivery.guard.suppressed {
            continue;
        }
        ui.label(muted(format!("Lane {}", lane.record.lane_id)));
        let own = LaneDeliveryView {
            lead_stopped: false,
            ..lane.delivery.clone()
        };
        delivery_lines(ui, &own, Some(lead.thread_id), !stopped, action);
    }
    if stopped {
        let held = LaneDeliveryView {
            lead_stopped: true,
            ..Default::default()
        };
        delivery_lines(ui, &held, Some(lead.thread_id), false, action);
    }
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
