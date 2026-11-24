//! One run of the Execution tab, drawn as a card of the Subagents column.
use crate::panes::run_card_widgets::{
    RunCardHeader, open_pane_button, role_label, run_card as card, run_card_header, stop_run_button,
};
use crate::theme::text::muted;
use crate::theme::tokens::{palette, phase_color, phase_label};
use crate::theme::widgets::ghost;
use workbench_protocol::{
    namespaces::execution::{RunDetail, RunLifecycle, RunSource},
    views::{RunPhase, RunRole},
};
use workspace_ui::ThreadRunPhase;

const DRILL_HINT: &str = "Show this run's subagents";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CardAction {
    Drill,
    Open,
    Stop,
}

pub(super) fn run_card(ui: &mut egui::Ui, run: &RunDetail) -> Option<CardAction> {
    let id = run.summary.id.as_str();
    let drillable = run.child_count > 0;
    let status = format!(
        "{} · {}",
        role_label(role_name(run.summary.role), run.category.as_deref()),
        status_text(run)
    );
    let model = model_line(run.model.as_deref());
    let header = RunCardHeader {
        run_id: id,
        name: &run.name,
        drill_hint: drillable.then_some(DRILL_HINT),
        color: status_color(run),
        active: run.source == RunSource::Live && run.summary.phase == RunPhase::Running,
        status: &status,
        status_hover: None,
        model: &model,
        model_hover: None,
    };
    card(ui, id, |ui| {
        let mut action = run_card_header(ui, &header).then_some(CardAction::Drill);
        ui.horizontal_wrapped(|ui| {
            if open_pane_button(ui) {
                action = Some(CardAction::Open);
            }
            if drillable
                && ui
                    .add(ghost(muted(format!("Subagents ({})", run.child_count))))
                    .clicked()
            {
                action = Some(CardAction::Drill);
            }
            if stop_run_button(ui, id, is_stoppable(run)) {
                action = Some(CardAction::Stop);
            }
        });
        action
    })
}

/// Whether Stop is offered: only a run the runtime still holds and that is
/// active. Anything else cannot be stopped from here.
pub(super) fn is_stoppable(run: &RunDetail) -> bool {
    run.source == RunSource::Live && run.lifecycle == RunLifecycle::Active
}

pub(super) fn status_text(run: &RunDetail) -> String {
    let status = if run.lifecycle == RunLifecycle::Recovering {
        "Waiting for recovery"
    } else {
        phase_label(thread_phase(run.summary.phase))
    };
    match run.source {
        RunSource::Live => status.to_owned(),
        RunSource::HistoryOnly => format!("{status} (history only)"),
    }
}

fn status_color(run: &RunDetail) -> egui::Color32 {
    if run.lifecycle == RunLifecycle::Recovering {
        palette().WARNING_FG
    } else {
        phase_color(thread_phase(run.summary.phase))
    }
}

fn role_name(role: RunRole) -> &'static str {
    match role {
        RunRole::Orchestrator => "orchestrator",
        RunRole::Worker => "worker",
        RunRole::Planner => "planner",
        RunRole::Reviewer => "reviewer",
        RunRole::Advisor => "advisor",
        RunRole::Other => "other",
    }
}

/// The core reports a stopped run as cancelled.
fn thread_phase(phase: RunPhase) -> ThreadRunPhase {
    match phase {
        RunPhase::Pending => ThreadRunPhase::Pending,
        RunPhase::Running => ThreadRunPhase::Running,
        RunPhase::WaitingForUser | RunPhase::WaitingForApproval => ThreadRunPhase::Waiting,
        RunPhase::Done => ThreadRunPhase::Done,
        RunPhase::Failed => ThreadRunPhase::Error,
        RunPhase::Cancelled => ThreadRunPhase::Stopped,
    }
}

fn model_line(model: Option<&str>) -> String {
    let model = model.filter(|model| !model.is_empty());
    let provider = model
        .and_then(|model| model.split_once('/'))
        .map_or("unknown", |(provider, _)| provider);
    format!("{} · {provider}", model.unwrap_or("unknown"))
}
