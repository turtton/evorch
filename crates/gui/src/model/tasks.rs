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
            parent_run_id: (id != 1).then(|| RunId::new(1)),
            name: "custom-name".into(),
            role_name: "Reviewer".into(),
            phase,
            model: "model-y".to_string(),
            category: None,
        }
    }

    #[test]
    fn update_keeps_only_subagent_runs() {
        // Given: a mixed list containing a root run and a child run
        let source = Source(vec![
            summary(1, AgentRunPhase::Running),
            summary(2, AgentRunPhase::Running),
        ]);
        let mut model = TasksModel::new(source);
        // When: the Agents tab rows are refreshed
        model.refresh();
        // Then: only the child run is listed
        assert_eq!(
            model
                .rows()
                .iter()
                .map(|row| row.run_id)
                .collect::<Vec<_>>(),
            [RunId::new(2)]
        );
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
                model: "model-y".into(),
                category: None,
            }
        );
    }

    #[test]
    fn restored_child_survives_refresh_and_live_summary_replaces_it() {
        let mut model = TasksModel::new(Source(Vec::new()));
        let events = [
            Event::new(LifecycleEvent::AgentRunStarted {
                run_id: "run-1".into(),
                parent_run_id: None,
                agent_name: "root".into(),
                role: "orchestrator".into(),
            }),
            Event::new(LifecycleEvent::AgentRunStarted {
                run_id: "run-2".into(),
                parent_run_id: Some("run-1".into()),
                agent_name: "older child".into(),
                role: "reviewer".into(),
            }),
            Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: "run-2".into(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Done,
                reason: None,
            }),
        ];
        model.restore_events(events.iter());
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].name, "older child");
        assert_eq!(model.rows()[0].status, AgentRunPhase::Done);
        model.refresh();
        assert_eq!(model.rows().len(), 1);

        model.update(&[summary(2, AgentRunPhase::Running)]);
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].name, "custom-name");
        assert_eq!(model.rows()[0].status, AgentRunPhase::Running);
    }

    #[test]
    fn stopped_row_remains_visible_and_can_resume() {
        let mut model = TasksModel::new(Source(vec![summary(2, AgentRunPhase::Running)]));
        model.refresh();
        for phase in [AgentRunPhase::Stopped, AgentRunPhase::Running] {
            model.apply_event(&Event::new(LifecycleEvent::AgentRunStateChanged {
                run_id: "run-2".into(),
                from: AgentRunPhase::Running,
                to: phase,
                reason: None,
            }));
            assert_eq!(model.rows()[0].status, phase);
            assert_eq!(model.rows()[0].name, "custom-name");
        }
    }

    #[test]
    fn rows_list_the_most_recently_updated_run_first() {
        // Given: two child runs whose latest activity times differ from their creation order
        let mut model = TasksModel::new(Source(vec![
            summary(2, AgentRunPhase::Running),
            summary(3, AgentRunPhase::Running),
        ]));
        let turn_at = |run_id: &str, seconds| {
            let mut event = Event::new(LifecycleEvent::TurnCompleted {
                run_id: run_id.into(),
                context_len: 1,
            });
            event.meta.wall_clock = UNIX_EPOCH + std::time::Duration::from_secs(seconds);
            event
        };
        let order = |model: &TasksModel<Source>| {
            model
                .rows()
                .iter()
                .map(|row| row.run_id)
                .collect::<Vec<_>>()
        };
        model.apply_event(&turn_at("run-3", 10));
        model.apply_event(&turn_at("run-2", 20));
        model.refresh();
        assert_eq!(order(&model), [RunId::new(2), RunId::new(3)]);

        // When: the older run completes another turn
        model.apply_event(&turn_at("run-3", 30));

        // Then: it moves to the top
        assert_eq!(order(&model), [RunId::new(3), RunId::new(2)]);
    }

    #[test]
    fn state_change_updates_known_row_and_unknown_refreshes() {
        // Given: a refreshed row built from a summary with distinct identity values
        let source = Source(vec![summary(2, AgentRunPhase::Running)]);
        let mut model = TasksModel::new(source);
        model.refresh();
        // When: a state-change event marks the run as Done
        model.apply_event(&Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-2".into(),
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
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

pub trait AgentRunSource: Send {
    fn teams(&self) -> Vec<(RunId, Vec<runtime::team::TeamTask>)> {
        Vec::new()
    }
    fn list(&self) -> Vec<AgentSummary>;

    fn inspect(&self, _run_id: RunId) -> Option<AgentInspection> {
        None
    }
}

impl AgentRunSource for AgentRuntime {
    fn teams(&self) -> Vec<(RunId, Vec<runtime::team::TeamTask>)> {
        self.team_tasks()
    }
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
    pub category: Option<String>,
}

pub struct TasksModel<S> {
    source: S,
    rows: Vec<TaskRow>,
    history_rows: Vec<TaskRow>,
    /// Latest lifecycle event time per run; rows are listed most recently updated first.
    updated_at: HashMap<RunId, SystemTime>,
}

pub fn role_for_run<'a>(rows: &'a [TaskRow], run_id: &str) -> Option<&'a str> {
    rows.iter()
        .find(|row| row.run_id.to_string() == run_id)
        .map(|row| row.role.as_str())
}

impl<S: AgentRunSource> TasksModel<S> {
    pub fn teams(&self) -> Vec<(RunId, Vec<runtime::team::TeamTask>)> {
        self.source.teams()
    }
    pub fn new(source: S) -> Self {
        Self {
            source,
            rows: Vec::new(),
            history_rows: Vec::new(),
            updated_at: HashMap::new(),
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
        self.rows = self.history_rows.clone();
        for live in summaries
            .iter()
            .filter(|summary| summary.parent_run_id.is_some())
            .map(|summary| TaskRow {
                run_id: summary.run_id,
                name: summary.name.clone(),
                role: summary.role_name.clone(),
                status: summary.phase,
                model: summary.model.clone(),
                category: summary.category.clone(),
            })
        {
            if let Some(row) = self.rows.iter_mut().find(|row| row.run_id == live.run_id) {
                *row = live;
            } else {
                self.rows.push(live);
            }
        }
        self.sort_rows();
    }

    fn sort_rows(&mut self) {
        let updated_at = &self.updated_at;
        self.rows.sort_by_key(|row| {
            std::cmp::Reverse((updated_at.get(&row.run_id).copied(), row.run_id))
        });
    }

    fn touch(&mut self, event: &Event) {
        let run_id = match &event.kind {
            EventKind::Lifecycle(
                LifecycleEvent::AgentRunStarted { run_id, .. }
                | LifecycleEvent::AgentRunStateChanged { run_id, .. }
                | LifecycleEvent::TurnCompleted { run_id, .. },
            ) => run_id,
            _ => return,
        };
        if let Ok(run_id) = run_id.parse() {
            let updated = self.updated_at.entry(run_id).or_insert(UNIX_EPOCH);
            *updated = (*updated).max(event.meta.wall_clock);
        }
    }

    /// Rebuild the delegated-run index from durable events on startup.
    pub fn restore_events<'a>(&mut self, events: impl IntoIterator<Item = &'a Event>) {
        let mut restored = std::collections::BTreeMap::<RunId, TaskRow>::new();
        for event in events {
            self.touch(event);
            match &event.kind {
                EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
                    run_id,
                    parent_run_id: Some(_),
                    agent_name,
                    role,
                }) => {
                    let Some(id) = parse_run_id(run_id) else {
                        continue;
                    };
                    restored.entry(id).or_insert_with(|| TaskRow {
                        run_id: id,
                        name: agent_name.clone(),
                        role: role.clone(),
                        status: AgentRunPhase::Pending,
                        model: String::new(),
                        category: None,
                    });
                }
                EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                    run_id, to, ..
                }) => {
                    if let Some(row) = parse_run_id(run_id).and_then(|id| restored.get_mut(&id)) {
                        row.status = *to;
                    }
                }
                _ => {}
            }
        }
        self.history_rows = restored.into_values().collect();
        self.refresh();
    }

    pub fn apply_event(&mut self, event: &Event) {
        self.touch(event);
        match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStarted { .. }) => self.refresh(),
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) => {
                if let Some(row) = self
                    .history_rows
                    .iter_mut()
                    .find(|row| row.run_id.to_string() == *run_id)
                {
                    row.status = *to;
                }
                if let Some(row) = self
                    .rows
                    .iter_mut()
                    .find(|row| row.run_id.to_string() == *run_id)
                {
                    row.status = *to;
                    self.sort_rows();
                } else {
                    self.refresh();
                }
            }
            EventKind::Lifecycle(LifecycleEvent::TurnCompleted { .. }) => self.sort_rows(),
            _ => {}
        }
    }
}

fn parse_run_id(value: &str) -> Option<RunId> {
    value.parse().ok()
}
