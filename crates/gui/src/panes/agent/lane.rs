//! The lane card of a lane's thread header (ADR 0044): policy, latest Report status,
//! the brief and the branch/worktree declaration, read from the lane ledger.
//!
//! `ready` is worded as unverified: the lane's claim, not a checked result. Delivery
//! notices (capacity, paused wake, stopped lead) come with the operation that clears them.

use event_bus::lane::LaneBrief;

use super::{AgentPaneAction, LaneAction, LaneContext, LeadContext};
use crate::model::lane_close::{close_detail, record_kind};
use crate::model::lanes::{LaneDeliveryView, report_status, routing_notice};
use crate::panes::lane_widgets::{
    CONTINUE_LEAD, CONTINUE_LEAD_HOVER, LEAD_STOPPED, RESUME_HOVER, RESUME_NOTIFICATIONS,
    close_record_badge, policy_badge, status_badge,
};
use crate::theme::{text::muted, tokens::*};

pub(super) fn lane_card(
    ui: &mut egui::Ui,
    lane: &LaneContext<'_>,
    action: &mut Option<AgentPaneAction>,
) {
    let detail = lane.detail;
    let record = &detail.record;
    ui.horizontal_wrapped(|ui| {
        policy_badge(ui, record.policy.as_str());
        match (&detail.close, detail.status()) {
            (Some(close), _) => {
                close_record_badge(ui, record_kind(close.kind)).on_hover_text(close_detail(close));
            }
            (None, Some(status)) => {
                status_badge(ui, report_status(status));
            }
            (None, None) => {
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
    if let Some(close) = &detail.close {
        ui.add(egui::Label::new(muted(close_detail(close))).wrap());
    }
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
                .small_button(RESUME_NOTIFICATIONS)
                .on_hover_text(RESUME_HOVER)
                .clicked()
        {
            *action = Some(AgentPaneAction::Lane(LaneAction::ResumeDelivery {
                lead: lead.to_owned(),
            }));
        }
    }
    if delivery.lead_stopped {
        ui.colored_label(palette().WARNING_FG, LEAD_STOPPED);
        if let Some(lead) = lead
            && ui
                .small_button(CONTINUE_LEAD)
                .on_hover_text(CONTINUE_LEAD_HOVER)
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
    crate::panes::lane_widgets::brief_body(
        ui,
        &brief.objective,
        &[
            ("Deliverables", &brief.deliverables),
            ("Acceptance criteria", &brief.acceptance_criteria),
            ("Constraints", &brief.constraints),
        ],
        Some(workspace),
    );
}
