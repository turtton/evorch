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
fn old_generation_cannot_replace_retry_state_or_last_valid_artifact() {
    // Given: a retried task with a valid artifact from the preceding attempt.
    let mut model = DurableTasksModel::default();
    model.apply_event(&progress("run-1", "running", Some("good.txt")));
    model.apply_event(&Event::new(OrchestratorEvent::TaskRetryScheduled {
        task_id: "task-1".into(),
        attempt: 2,
        reason: "retry".into(),
        new_run_id: "run-2".into(),
    }));
    // When: delayed progress, stale, completion, and a duplicate retry arrive.
    for event in [
        progress("run-1", "completed", Some("old.txt")),
        Event::new(OrchestratorEvent::TaskStaleMarked {
            task_id: "task-1".into(),
            run_id: "run-1".into(),
            last_heartbeat_ns: 1,
        }),
        Event::new(LifecycleEvent::BackgroundTaskCompleted {
            task_id: "run-1".into(),
        }),
        Event::new(OrchestratorEvent::TaskRetryScheduled {
            task_id: "task-1".into(),
            attempt: 1,
            reason: "old".into(),
            new_run_id: "run-1".into(),
        }),
    ] {
        model.apply_event(&event);
    }
    // Then: the new generation and last known artifact remain authoritative.
    let row = model.rows().next().expect("task");
    assert_eq!((row.status, row.attempt), (TaskStatus::Retrying, 2));
    assert_eq!(row.run_id.as_deref(), Some("run-2"));
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
fn goal_evidence_updates_linked_task_only_when_successful_and_same_revision() {
    use event_bus::orchestrator::CriterionEvidence;
    use event_bus::{CriterionCheck, CriterionStatus, GateEvidence, RunPurpose};
    // Given: a task linked to a goal and a checkpoint.
    let mut model = DurableTasksModel::default();
    model.apply_event(&Event::new(OrchestratorEvent::GoalCreated {
        goal_id: "goal-1".into(),
        session_id: "session".into(),
        project_id: "project".into(),
        thread_id: "thread".into(),
        goal: "Ship task".into(),
        references: Vec::new(),
        constraints: Vec::new(),
        repo: "owner/repo".into(),
        base_ref: "main".into(),
        root_run_id: "root".into(),
    }));
    model.apply_event(&Event::new(OrchestratorEvent::RunAttached {
        goal_id: "goal-1".into(),
        run_id: "run-1".into(),
        parent_run_id: Some("root".into()),
        role: "worker".into(),
        purpose: RunPurpose::Implement,
    }));
    model.apply_event(&progress("run-1", "running", None));
    model.apply_event(&Event::new(OrchestratorEvent::TaskCheckpoint {
        task_id: "task-1".into(),
        run_id: "run-1".into(),
        tool_call_count: 50,
        cumulative_input_tokens: 100,
        cumulative_output_tokens: 20,
        elapsed_ms: 900,
    }));
    // When: valid evidence is followed by failed and mismatched-revision evidence.
    for (exit_status, target_sha, artifact) in [
        (0, "head", "good.log"),
        (1, "head", "failed.log"),
        (0, "old", "stale.log"),
    ] {
        model.apply_event(&Event::new(OrchestratorEvent::EvidenceRecorded {
            goal_id: "goal-1".into(),
            evidence: GateEvidence::Criteria {
                head_sha: "head".into(),
                reviewer_run_id: "review".into(),
                round: 1,
                checklist: vec![CriterionCheck {
                    id: "ac1".into(),
                    status: CriterionStatus::Met,
                    note: String::new(),
                    evidence: Some(CriterionEvidence {
                        command: "cargo test".into(),
                        exit_status,
                        target_sha: target_sha.into(),
                        diff_ref: None,
                        artifact_path: Some(artifact.into()),
                        red_evidence: None,
                    }),
                }],
            },
        }));
    }
    // Then: linkage and checkpoint survive and only valid evidence is displayed.
    let row = model.rows().find(|row| row.id == "task-1").expect("task");
    assert_eq!(row.goal_id.as_deref(), Some("goal-1"));
    assert_eq!(row.last_artifact.as_deref(), Some("good.log"));
    assert_eq!(row.detail, "checkpoint: 50 calls · 100/20 tokens · 900 ms");
}
