//! Latest runtime goals from one ordered orchestrator scan.

use std::collections::BTreeMap;

use event_bus::{Event, EventKind, OrchestratorEvent, ThreadGoalSnapshot};

#[derive(Default)]
pub struct ThreadSnapshots {
    pub goals: BTreeMap<String, ThreadGoalSnapshot>,
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
        if let EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot }) =
            event.kind
        {
            // A goal can move threads, and a thread can receive a new goal.
            self.goals.retain(|thread, goal| {
                thread == &snapshot.thread_id || goal.goal_id != snapshot.goal_id
            });
            self.goals.insert(snapshot.thread_id.clone(), snapshot);
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
                criteria: vec!["Verified evidence".into()],
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
            goal("new", "moving"),
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
        let new = ThreadSnapshots::read(&db).unwrap();
        assert_eq!(new.goals, old_goals);
        assert_eq!(new.goals["new"].goal_id, "moving");
        assert_eq!(new.goals["old"].goal_id, "replacement");
    }

    struct NoRequests;
    #[async_trait::async_trait]
    impl runtime::AgentModel for NoRequests {
        fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
            "unused".into()
        }
        async fn complete(
            &self,
            _: &runtime::AgentInvocationContext,
            _: runtime::Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
            unreachable!("startup goal restoration must not resume execution")
        }
    }

    #[test]
    fn startup_notification_and_reconnect_view_include_durable_usage_not_orphan_rows() {
        use std::sync::Arc;
        let directory = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: directory.path().join("startup.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let mut event = goal("thread", "current");
        let EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot }) =
            &mut event.kind
        else {
            unreachable!()
        };
        snapshot.root_run_id = "run-1".into();
        snapshot.usage.input_tokens = 100;
        storage.handle().append_event(Some("gui"), &event).unwrap();
        for id in ["current", "orphan"] {
            storage
                .handle()
                .upsert_thread_goal_usage(
                    id,
                    "thread",
                    "run-1",
                    &event_bus::ThreadGoalUsage {
                        model_requests: 9,
                        input_tokens: 50,
                        output_tokens: 70,
                    },
                )
                .unwrap();
        }
        let bus = Arc::new(event_bus::EventBus::new(32));
        let mut events = bus.subscribe();
        let runtime = runtime::AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            Arc::new(NoRequests),
        )
        .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        let snapshots =
            ThreadSnapshots::read(&storage::Database::open_read_only(&config.db_path).unwrap())
                .unwrap();
        assert_eq!(snapshots.goals.len(), 1);
        for snapshot in snapshots.goals.into_values() {
            runtime.restore_thread_goal(snapshot).unwrap();
        }
        let current = runtime.thread_goal("thread").unwrap();
        assert_eq!(
            current.usage,
            event_bus::ThreadGoalUsage {
                model_requests: 9,
                input_tokens: 100,
                output_tokens: 70
            }
        );
        let notifications = events.drain_pending_snapshot();
        assert_eq!(notifications.len(), 1);
        assert!(matches!(&notifications[0].kind,
            EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated {snapshot}) if snapshot == &current));
        assert!(runtime.list_agents().is_empty());
    }
}
