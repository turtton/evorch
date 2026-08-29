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

    fn summary(id: u64, role: &str, phase: AgentRunPhase) -> AgentSummary {
        AgentSummary {
            run_id: RunId::new(id),
            role_name: role.into(),
            phase,
        }
    }

    #[test]
    fn update_maps_runtime_summary_to_gui_labels() {
        let source = Source(vec![summary(2, "reviewer", AgentRunPhase::Running)]);
        let mut model = TasksModel::new(source, "model-x");
        model.refresh();
        assert_eq!(
            model.rows()[0],
            TaskRow {
                run_id: RunId::new(2),
                name: "reviewer".into(),
                role: "reviewer".into(),
                status: AgentRunPhase::Running,
                model: "model-x".into()
            }
        );
    }

    #[test]
    fn state_change_updates_known_row_and_unknown_refreshes() {
        let source = Source(vec![summary(1, "worker", AgentRunPhase::Running)]);
        let mut model = TasksModel::new(source, "m");
        model.refresh();
        model.apply_event(&Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Done,
            reason: None,
        }));
        assert_eq!(model.rows()[0].status, AgentRunPhase::Done);
    }
}
use event_bus::{AgentRunPhase, Event, EventKind, LifecycleEvent};
use runtime::{AgentRuntime, AgentSummary, RunId};

pub trait AgentRunSource: Send {
    fn list(&self) -> Vec<AgentSummary>;
}

impl AgentRunSource for AgentRuntime {
    fn list(&self) -> Vec<AgentSummary> {
        self.list_agents()
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
    model_label: String,
    rows: Vec<TaskRow>,
}

impl<S: AgentRunSource> TasksModel<S> {
    pub fn new(source: S, model_label: impl Into<String>) -> Self {
        Self {
            source,
            model_label: model_label.into(),
            rows: Vec::new(),
        }
    }

    pub fn rows(&self) -> &[TaskRow] {
        &self.rows
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
                name: summary.role_name.clone(),
                role: summary.role_name.clone(),
                status: summary.phase,
                model: self.model_label.clone(),
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
