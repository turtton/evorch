//! Latest runtime goals and procedures from one ordered orchestrator scan.

use std::collections::BTreeMap;

use event_bus::{Event, EventKind, OrchestratorEvent, ThreadGoalSnapshot, ThreadTodoSnapshot};

#[derive(Default)]
pub struct ThreadSnapshots {
    pub goals: BTreeMap<String, ThreadGoalSnapshot>,
    pub todos: BTreeMap<String, ThreadTodoSnapshot>,
}

impl ThreadSnapshots {
    pub fn read(db: &storage::Database) -> Result<Self, storage::StorageError> {
        let mut snapshots = Self::default();
        db.for_each_event_matching(&[("Orchestrator", None)], |stored| {
            snapshots.apply(stored.event);
        })?;
        Ok(snapshots)
    }

    pub fn apply(&mut self, event: Event) {
        match event.kind {
            EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot }) => {
                // A goal can move threads, and a thread can receive a new goal.
                self.goals.retain(|thread, goal| {
                    thread == &snapshot.thread_id || goal.goal_id != snapshot.goal_id
                });
                self.goals.insert(snapshot.thread_id.clone(), snapshot);
            }
            EventKind::Orchestrator(OrchestratorEvent::ThreadTodoUpdated { snapshot }) => {
                crate::model::thread_todos::apply_snapshot(&mut self.todos, &snapshot);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn goal(thread: &str, id: &str) -> Event {
        Event::new(OrchestratorEvent::ThreadGoalUpdated {
            snapshot: ThreadGoalSnapshot {
                goal_id: id.into(),
                thread_id: thread.into(),
                root_run_id: "run".into(),
                related_root_run_ids: vec![],
                objective: "objective".into(),
                original_request: "request".into(),
                criteria: vec![],
                checks: vec![],
                phase: event_bus::ThreadGoalPhase::Working,
                review_enabled: false,
                checks_paused: false,
                work_stopped: false,
                epoch: 1,
                review_round: 0,
                findings: vec![],
                reason: None,
                usage: event_bus::ThreadGoalUsage {
                    model_requests: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                },
                max_review_rounds: 3,
                max_tokens: None,
            },
        })
    }

    fn todo(thread: &str, revision: u64) -> Event {
        Event::new(OrchestratorEvent::ThreadTodoUpdated {
            snapshot: ThreadTodoSnapshot {
                list_id: "list".into(),
                thread_id: thread.into(),
                revision,
                items: vec![],
            },
        })
    }

    #[test]
    fn one_stream_matches_two_reads_including_transfers_replacements_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("events.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        for event in [
            goal("old", "moving"),
            todo("old", 1),
            goal("new", "moving"),
            todo("new", 3),
            todo("old", 2),
            goal("old", "replacement"),
        ] {
            storage.handle().append_event(Some("gui"), &event).unwrap();
        }
        let db = storage::Database::open(&config).unwrap();
        let mut old_goals = BTreeMap::new();
        for stored in db.orchestrator_events_ordered().unwrap() {
            if let EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot }) =
                stored.event.kind
            {
                old_goals.retain(|thread, goal: &mut ThreadGoalSnapshot| {
                    thread == &snapshot.thread_id || goal.goal_id != snapshot.goal_id
                });
                old_goals.insert(snapshot.thread_id.clone(), snapshot);
            }
        }
        let mut old_todos = BTreeMap::new();
        for stored in db.orchestrator_events_ordered().unwrap() {
            if let EventKind::Orchestrator(OrchestratorEvent::ThreadTodoUpdated { snapshot }) =
                stored.event.kind
            {
                crate::model::thread_todos::apply_snapshot(&mut old_todos, &snapshot);
            }
        }
        let new = ThreadSnapshots::read(&db).unwrap();
        assert_eq!(new.goals, old_goals);
        assert_eq!(new.todos, old_todos);
        assert_eq!(new.goals["new"].goal_id, "moving");
        assert_eq!(new.goals["old"].goal_id, "replacement");
        assert_eq!(new.todos.len(), 1);
        assert_eq!(new.todos["new"].revision, 3);
        assert!(new.todos["new"].items.is_empty());
    }
}
