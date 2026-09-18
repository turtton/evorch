use super::LoopState;
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
                self.finish_error(format!("{}: {}", breach.code, breach.detail));
                return decision;
            }
        }
        self.publish_durable_task(self.phase(), None);
        decision
    }
}
