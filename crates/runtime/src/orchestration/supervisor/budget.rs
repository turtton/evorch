use super::*;
use event_bus::{DiagnosticEvent, DiagnosticSeverity, event::diagnostic_codes};

impl SupervisorActor {
    pub(super) fn on_budget_diagnostic(&mut self, diagnostic: &DiagnosticEvent) {
        if diagnostic.source != "budget_tracker"
            || diagnostic.severity != DiagnosticSeverity::Error
            || !matches!(
                diagnostic.code.as_str(),
                diagnostic_codes::BUDGET_EXHAUSTED | diagnostic_codes::NO_PROGRESS
            )
        {
            return;
        }
        let Some(run_id) = diagnostic.run_id.as_deref() else {
            return;
        };
        for goal_id in self.goals_for_run(run_id) {
            let Some(snapshot) = self.snapshot(&goal_id) else {
                continue;
            };
            match snapshot.state {
                GoalState::Active => {
                    let _ = self.transition(&goal_id, GoalState::Blocked, &diagnostic.detail);
                }
                GoalState::Blocked
                | GoalState::Paused
                | GoalState::Complete
                | GoalState::Cancelled => {}
            }
        }
    }
}
