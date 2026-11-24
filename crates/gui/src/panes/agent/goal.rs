//! Thread goal controls belong to the composer, independently of worker Stop.
use event_bus::{ThreadGoalPhase, ThreadGoalSnapshot};

use crate::model::commands::WorkbenchCommand;
pub use crate::panes::goal_widgets::GoalAction;
use crate::panes::goal_widgets::{
    GoalBandView, GoalCheckRow, GoalPhaseView, GoalUsageView, progress_band,
};

impl GoalAction {
    pub fn into_command(self) -> WorkbenchCommand {
        match self {
            Self::Review {
                thread_id,
                goal_id,
                enabled,
            } => WorkbenchCommand::SetGoalReview {
                thread_id,
                goal_id,
                enabled,
            },
            Self::ChecksPaused {
                thread_id,
                goal_id,
                paused,
            } => WorkbenchCommand::SetGoalChecksPaused {
                thread_id,
                goal_id,
                paused,
            },
        }
    }
}

pub(super) type BandOutput =
    crate::panes::goal_widgets::BandOutput<crate::panes::composer::ComposerAction>;

/// Queued follow-ups, then goal controls, share one inset composer band. Expanded details scroll within
/// `details_max` so the composer input below keeps its room.
pub(super) fn progress_strip(
    ui: &mut egui::Ui,
    goal: Option<&ThreadGoalSnapshot>,
    queue: super::queue::QueueBand<'_>,
    details_max: f32,
    plan: Option<crate::panes::conversation_plan::PlanBand<'_>>,
) -> BandOutput {
    let thread = goal.map(|goal| goal.thread_id.as_str()).unwrap_or_default();
    let view = goal.map(band_view);
    let rows =
        (!queue.items.is_empty()).then_some(|ui: &mut egui::Ui| super::queue::rows(ui, queue));
    progress_band(ui, thread, view.as_ref(), rows, details_max, plan)
}

fn band_view(goal: &ThreadGoalSnapshot) -> GoalBandView<'_> {
    GoalBandView {
        thread_id: &goal.thread_id,
        goal_id: &goal.goal_id,
        objective: &goal.objective,
        phase: match goal.phase {
            ThreadGoalPhase::Working => GoalPhaseView::Working,
            ThreadGoalPhase::Checking => GoalPhaseView::Checking,
            ThreadGoalPhase::Reviewing => GoalPhaseView::Reviewing,
            ThreadGoalPhase::Repairing => GoalPhaseView::Repairing,
            ThreadGoalPhase::Complete => GoalPhaseView::Complete,
            ThreadGoalPhase::Blocked => GoalPhaseView::Blocked,
        },
        review_enabled: goal.review_enabled,
        checks_paused: goal.checks_paused,
        work_stopped: goal.work_stopped,
        reason: goal.reason.as_deref(),
        review_round: goal.review_round,
        max_review_rounds: goal.max_review_rounds,
        criteria: &goal.criteria,
        checks: goal
            .checks
            .iter()
            .map(|check| GoalCheckRow {
                criterion: check.criterion,
                met: check.met,
                evidence: &check.evidence,
            })
            .collect(),
        findings: &goal.findings,
        original_request: Some(&goal.original_request),
        usage: Some(GoalUsageView {
            requests: u64::from(goal.usage.model_requests),
            input: goal.usage.input_tokens,
            output: goal.usage.output_tokens,
        }),
    }
}
