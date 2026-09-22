use std::collections::BTreeMap;

use event_bus::{Event, EventKind};
use storage::entity::TaskStatus;

mod orchestrator;

#[derive(Debug, Clone)]
pub struct DurableTaskRow {
    pub id: String,
    pub title: String,
    pub run_id: Option<String>,
    /// Every execution associated with this task, including earlier attempts.
    pub run_ids: Vec<String>,
    pub goal_id: Option<String>,
    pub status: TaskStatus,
    pub attempt: u32,
    pub last_artifact: Option<String>,
    pub detail: String,
}

#[derive(Default)]
pub struct DurableTasksModel {
    rows: BTreeMap<String, DurableTaskRow>,
    run_goals: BTreeMap<String, String>,
}

impl DurableTasksModel {
    pub fn rows(&self) -> impl Iterator<Item = &DurableTaskRow> {
        self.rows.values()
    }

    pub fn tasks_for_run(&self, run_id: &str) -> impl Iterator<Item = &DurableTaskRow> {
        self.rows
            .values()
            .filter(move |row| row.run_ids.iter().any(|run| run == run_id))
    }

    pub fn apply_event(&mut self, event: &Event) {
        match &event.kind {
            EventKind::Orchestrator(event) => self.apply_orchestrator(event),
            // Execution lifecycle does not determine the state of its continuing task.
            EventKind::Lifecycle(_)
            | EventKind::Ledger(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Diagnostic(_)
            | EventKind::Ownership(_)
            | EventKind::Snapshot(_) => {}
        }
    }

    fn row(&mut self, id: &str) -> &mut DurableTaskRow {
        self.rows
            .entry(id.into())
            .or_insert_with(|| DurableTaskRow {
                id: id.into(),
                title: id.into(),
                run_id: None,
                run_ids: Vec::new(),
                goal_id: None,
                status: TaskStatus::Queued,
                attempt: 0,
                last_artifact: None,
                detail: String::new(),
            })
    }

    fn task_for_run(&mut self, id: &str, run: &str) -> Option<&mut DurableTaskRow> {
        if self
            .rows
            .get(id)
            .is_some_and(|row| row.run_id.as_deref().is_some_and(|current| current != run))
        {
            return None;
        }
        let goal = self.run_goals.get(run).cloned();
        let row = self.row(id);
        row.attach_run(run);
        if goal.is_some() {
            row.goal_id = goal;
        }
        Some(row)
    }
}

impl DurableTaskRow {
    fn attach_run(&mut self, run: &str) {
        if !self.run_ids.iter().any(|known| known == run) {
            self.run_ids.push(run.into());
        }
        self.run_id = Some(run.into());
    }
}
