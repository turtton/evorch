use event_bus::{Event, LifecycleEvent, OrchestratorEvent};
use gui::model::durable_tasks::DurableTasksModel;
use storage::entity::TaskStatus;

fn progress(run: &str, status: &str, artifact: Option<&str>) -> Event {
    Event::new(OrchestratorEvent::TaskProgressed {
        task_id: "task-1".into(),
        run_id: run.into(),
        progress: serde_json::json!({"status":status, "last_artifact":artifact}),
        reason: "progress".into(),
    })
}

#[test]
fn other_generation_cannot_replace_current_state_or_last_valid_artifact() {
    // Given: a running task with a valid artifact from its current execution.
    let mut model = DurableTasksModel::default();
    model.apply_event(&progress("run-1", "running", Some("good.txt")));
    // When: progress from another execution and the execution's completion arrive.
    for event in [
        progress("run-2", "completed", Some("old.txt")),
        Event::new(LifecycleEvent::BackgroundTaskCompleted {
            task_id: "run-1".into(),
        }),
    ] {
        model.apply_event(&event);
    }
    // Then: the current execution and last known artifact remain authoritative.
    let row = model.rows().next().expect("task");
    assert_eq!(row.status, TaskStatus::Running);
    assert_eq!(row.run_id.as_deref(), Some("run-1"));
    assert_eq!(row.last_artifact.as_deref(), Some("good.txt"));
}

#[test]
fn typed_statuses_survive_progress_without_losing_artifact() {
    for (wire, expected) in [
        ("queued", TaskStatus::Queued),
        ("running", TaskStatus::Running),
        ("blocked", TaskStatus::Blocked),
        ("retrying", TaskStatus::Retrying),
        ("completed", TaskStatus::Completed),
        ("cancelled", TaskStatus::Cancelled),
        ("stopped", TaskStatus::Stopped),
        ("failed", TaskStatus::Failed),
    ] {
        // Given: a valid artifact exists before a status-only update.
        let mut model = DurableTasksModel::default();
        model.apply_event(&progress("run-1", "queued", Some("valid.txt")));
        // When: the runtime publishes its typed continuation state.
        model.apply_event(&progress("run-1", wire, None));
        // Then: all supported statuses render from typed data, retaining the artifact.
        let row = model.rows().next().expect("task");
        assert_eq!(row.status, expected);
        assert_eq!(row.last_artifact.as_deref(), Some("valid.txt"));
    }
}

#[test]
fn checkpoint_updates_the_task_linked_to_its_execution() {
    // Given: a task running in a known execution.
    let mut model = DurableTasksModel::default();
    model.apply_event(&progress("run-1", "running", None));
    // When: the execution reports a budget checkpoint.
    model.apply_event(&Event::new(OrchestratorEvent::TaskCheckpoint {
        task_id: "task-1".into(),
        run_id: "run-1".into(),
        tool_call_count: 50,
        cumulative_input_tokens: 100,
        cumulative_output_tokens: 20,
        elapsed_ms: 900,
    }));
    // Then: the checkpoint is shown on the task linked to that execution only.
    let row = model.rows().next().expect("task");
    assert_eq!(row.detail, "checkpoint: 50 calls · 100/20 tokens · 900 ms");
    assert_eq!(model.tasks_for_run("run-1").next().unwrap().id, "task-1");
    assert!(model.tasks_for_run("run-2").next().is_none());
}

#[test]
fn background_execution_does_not_invent_a_task() {
    let mut model = DurableTasksModel::default();
    model.apply_event(&Event::new(LifecycleEvent::BackgroundTaskStarted {
        task_id: "run-1".into(),
    }));
    assert_eq!(model.rows().count(), 0);
}

#[test]
fn execution_completion_does_not_complete_its_task() {
    let mut model = DurableTasksModel::default();
    model.apply_event(&Event::new(OrchestratorEvent::TaskProgressed {
        task_id: "run-1".into(),
        run_id: "run-1".into(),
        progress: serde_json::json!({"status":"running"}),
        reason: "start".into(),
    }));
    model.apply_event(&Event::new(LifecycleEvent::BackgroundTaskCompleted {
        task_id: "run-1".into(),
    }));
    assert_eq!(model.rows().next().unwrap().status, TaskStatus::Running);
}

#[test]
fn waiting_execution_does_not_block_its_running_task() {
    let mut model = DurableTasksModel::default();
    model.apply_event(&progress("run-1", "running", None));
    model.apply_event(&Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: "run-1".into(),
        from: event_bus::AgentRunPhase::Running,
        to: event_bus::AgentRunPhase::Waiting,
        reason: Some("waiting for tool".into()),
    }));
    assert_eq!(model.rows().next().unwrap().status, TaskStatus::Running);
    assert_eq!(model.rows().next().unwrap().detail, "progress");
}
