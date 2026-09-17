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
        if self.task.role == crate::Role::Worker
            && let Ok(elapsed) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            && let Ok(heartbeat) = u64::try_from(elapsed.as_nanos())
        {
            self.shared.bus.emit(event_bus::Event::new(
                event_bus::OrchestratorEvent::TaskProgressed {
                    task_id: task_id.into(),
                    run_id,
                    progress: serde_json::json!({
                        "status": "running", "input": self.task.prompt,
                        "resume_cursor": null, "last_artifact": null,
                        "failure_reason": null, "attempts": 0,
                        "heartbeat_at_ns": heartbeat,
                    }),
                    reason: "task heartbeat".into(),
                },
            ));
        }
    }
}
