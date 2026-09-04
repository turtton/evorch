#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::{AgentRunPhase, Event, LifecycleEvent};
    use runtime::{AgentSummary, RunId};

    #[derive(Clone)]
    struct Source(Vec<AgentSummary>);
    impl AgentRunSource for Source {
        fn list(&self) -> Vec<AgentSummary> {
            self.0.clone()
        }
    }

    // Fixture with distinct name / role_name / model values so any duplication
    // between them is observable in the mapped row.
    fn summary(id: u64, phase: AgentRunPhase) -> AgentSummary {
        AgentSummary {
            run_id: RunId::new(id),
            name: "custom-name".into(),
            role_name: "Reviewer".into(),
            phase,
            model: "model-y".to_string(),
        }
    }

    #[test]
    fn update_maps_summary_identity_directly() {
        // Given: a source summary with distinct name, role, and model values
        let source = Source(vec![summary(2, AgentRunPhase::Running)]);
        let mut model = TasksModel::new(source);
        // When: the rows are refreshed from the summary
        model.refresh();
        // Then: each row field maps directly to the summary identity
        assert_eq!(
            model.rows()[0],
            TaskRow {
                run_id: RunId::new(2),
                name: "custom-name".into(),
                role: "Reviewer".into(),
                status: AgentRunPhase::Running,
                model: "model-y".into()
            }
        );
    }

    #[test]
    fn state_change_updates_known_row_and_unknown_refreshes() {
        // Given: a refreshed row built from a summary with distinct identity values
        let source = Source(vec![summary(1, AgentRunPhase::Running)]);
        let mut model = TasksModel::new(source);
        model.refresh();
        // When: a state-change event marks the run as Done
        model.apply_event(&Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        }));
        // Then: the status updates in place while name and model are preserved
        assert_eq!(model.rows()[0].status, AgentRunPhase::Done);
        assert_eq!(model.rows()[0].name, "custom-name");
        assert_eq!(model.rows()[0].model, "model-y");
    }
}
use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent};
use runtime::{AgentInspection, AgentRuntime, AgentSummary, RunId};

pub trait AgentRunSource: Send {
    fn list(&self) -> Vec<AgentSummary>;

    fn inspect(&self, _run_id: RunId) -> Option<AgentInspection> {
        None
    }
}

impl AgentRunSource for AgentRuntime {
    fn list(&self) -> Vec<AgentSummary> {
        self.list_agents()
    }

    fn inspect(&self, run_id: RunId) -> Option<AgentInspection> {
        self.inspect_agent(run_id).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    pub run_id: RunId,
    pub name: String,
    pub role: String,
    pub status: AgentRunPhase,
    pub model: String,
}

pub struct TasksModel<S> {
    source: S,
    rows: Vec<TaskRow>,
}

impl<S: AgentRunSource> TasksModel<S> {
    pub fn new(source: S) -> Self {
        Self {
            source,
            rows: Vec::new(),
        }
    }

    pub fn rows(&self) -> &[TaskRow] {
        &self.rows
    }

    pub fn inspect(&self, run_id: RunId) -> Option<AgentInspection> {
        self.source.inspect(run_id)
    }

    pub fn refresh(&mut self) {
        let summaries = self.source.list();
        self.update(&summaries);
    }

    pub fn update(&mut self, summaries: &[AgentSummary]) {
        self.rows = summaries
            .iter()
            .map(|summary| TaskRow {
                run_id: summary.run_id,
                name: summary.name.clone(),
                role: summary.role_name.clone(),
                status: summary.phase,
                model: summary.model.clone(),
            })
            .collect();
        self.rows.sort_by_key(|row| row.run_id.get());
    }

    pub fn apply_event(&mut self, event: &Event) {
        let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
            &event.kind
        else {
            return;
        };
        if let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| row.run_id.to_string() == *run_id)
        {
            row.status = *to;
        } else {
            self.refresh();
        }
    }
}
