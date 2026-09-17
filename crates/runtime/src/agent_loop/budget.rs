use super::LoopState;
use crate::budget_tracker::BudgetContext;

impl LoopState {
    pub(super) fn publish_budget(&mut self) {
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
        self.budget.publish(
            self.escalation_detector.tool_calls(),
            &BudgetContext {
                bus: &self.shared.bus,
                run_id: &run_id,
                task_id,
                settings: &self.task.config.budget,
            },
        );
    }
}
