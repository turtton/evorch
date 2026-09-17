use event_bus::{AgentRunPhase, LifecycleEvent};
use storage::entity::TaskStatus;

use super::DurableTasksModel;

impl DurableTasksModel {
    pub(super) fn apply_lifecycle(&mut self, event: &LifecycleEvent) {
        let (id, status) = match event {
            LifecycleEvent::BackgroundTaskStarted { task_id } => (task_id, TaskStatus::Running),
            LifecycleEvent::BackgroundTaskCompleted { task_id } => (task_id, TaskStatus::Completed),
            LifecycleEvent::BackgroundTaskCancelled { task_id } => (task_id, TaskStatus::Cancelled),
            LifecycleEvent::AgentRunStateChanged { run_id, to, .. } => (
                run_id,
                match to {
                    AgentRunPhase::Pending => TaskStatus::Queued,
                    AgentRunPhase::Running => TaskStatus::Running,
                    AgentRunPhase::Waiting => TaskStatus::Blocked,
                    AgentRunPhase::Done => TaskStatus::Completed,
                    AgentRunPhase::Error => TaskStatus::Failed,
                },
            ),
            LifecycleEvent::AgentRunStarted { .. }
            | LifecycleEvent::Started { .. }
            | LifecycleEvent::Delegated { .. }
            | LifecycleEvent::Completed { .. }
            | LifecycleEvent::Failed { .. }
            | LifecycleEvent::RoutingDecision { .. }
            | LifecycleEvent::EscalationRequested { .. }
            | LifecycleEvent::EscalationProposed { .. }
            | LifecycleEvent::AgentRunRestored { .. } => return,
        };
        let mut found = false;
        for row in self
            .rows
            .values_mut()
            .filter(|row| row.run_id.as_ref() == Some(id) || &row.id == id)
        {
            row.status = status;
            if let LifecycleEvent::AgentRunStateChanged {
                reason: Some(reason),
                ..
            } = event
            {
                row.detail.clone_from(reason);
            }
            found = true;
        }
        if !found && matches!(event, LifecycleEvent::BackgroundTaskStarted { .. }) {
            let row = self.row(id);
            row.run_id = Some(id.clone());
            row.status = status;
        }
    }
}
