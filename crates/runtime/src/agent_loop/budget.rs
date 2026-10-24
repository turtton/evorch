use event_bus::event::diagnostic_codes;

use super::LoopState;
use super::stop_context::stop_reason;
use crate::budget_tracker::{BudgetContext, BudgetDecision};

impl LoopState {
    pub(super) fn publish_budget(&mut self) -> BudgetDecision {
        let run_id = self.task.run_id.to_string();
        let task_id = self
            .task
            .config
            .task_id
            .as_deref()
            .or_else(|| {
                self.task
                    .config
                    .team_task
                    .as_ref()
                    .map(|task| task.id.as_str())
            })
            .unwrap_or(&run_id);
        let decision = self.budget.publish(
            self.escalation_detector.tool_calls(),
            &BudgetContext {
                bus: &self.shared.bus,
                run_id: &run_id,
                task_id,
                settings: &self.task.config.budget,
            },
        );
        match &decision {
            BudgetDecision::Continue => {}
            BudgetDecision::Exhausted(breach) => {
                // A no-progress loop is judged by what it kept calling; other limits are not.
                let last_tool = (breach.code == diagnostic_codes::NO_PROGRESS)
                    .then_some(self.last_tool.as_ref())
                    .flatten();
                let reason = stop_reason(breach.code, last_tool, &breach.detail);
                self.finish_error(reason);
                return decision;
            }
        }
        self.update_durable_artifact();
        decision
    }
}
