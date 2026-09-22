use std::time::UNIX_EPOCH;

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, OrchestratorEvent};
use gui::{
    model::durable_tasks::DurableTasksModel,
    panes::tasks::{TasksAction, tasks_pane},
};
use runtime::{
    RunId,
    team::{TaskSpec, TeamBoard},
};
use storage::{
    Storage, StorageConfig,
    entity::{TaskRecord, TaskStatus},
};

fn task(id: &str) -> TaskRecord {
    TaskRecord {
        id: id.into(),
        session_id: None,
        status: TaskStatus::Pending,
        parent_run_id: Some("parent-only".into()),
        input: None,
        progress: None,
        last_artifact: None,
        failure_reason: None,
        resume_cursor: None,
        attempts: 0,
        heartbeat_at: None,
        created_at: UNIX_EPOCH,
        updated_at: UNIX_EPOCH,
    }
}

#[test]
fn tasks_combine_progress_dependencies_and_claims_without_execution_rows() {
    // Given: a durable task, a queued prerequisite, and a separately scoped team claim.
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("tasks.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .enqueue_task(&task("prerequisite"))
        .unwrap();
    storage.handle().enqueue_task(&task("task-1")).unwrap();
    storage
        .handle()
        .link_tasks("prerequisite", "task-1")
        .unwrap();
    let mut model = DurableTasksModel::default();
    model.apply_event(&Event::new(OrchestratorEvent::TaskProgressed {
        task_id: "task-1".into(),
        run_id: "run-2".into(),
        progress: serde_json::json!({"status":"blocked", "input":"Finish the layout", "last_artifact":"artifacts/layout.png"}),
        reason: "Waiting for prerequisite".into(),
    }));
    let board = TeamBoard::default();
    board
        .enqueue(TaskSpec {
            id: "team-task".into(),
            paths: vec!["src/ui".into()],
        })
        .unwrap();
    board.claim("team-task", "run-3", 0).unwrap();
    let teams = vec![(RunId::new(1), board.snapshot().unwrap())];
    let mut harness = Harness::builder()
        .with_size(egui::vec2(450.0, 1000.0))
        .build_ui_state(
            move |ui, action: &mut Option<TasksAction>| {
                if let Some(next) = tasks_pane(ui, &model, &teams, Some(&config), None) {
                    *action = Some(next);
                }
            },
            None,
        );
    harness.run();
    for label in [
        "Finish the layout",
        "Waiting for prerequisite",
        "artifacts/layout.png",
        "Blocks",
        "Blocked by",
        "team-task",
        "Claimed",
        "run-3",
        "src/ui",
    ] {
        assert!(harness.query_by_label(label).is_some(), "missing {label}");
    }
    assert_eq!(
        harness.query_all_by_label("task-1").count(),
        2,
        "one task card and one dependency label"
    );
    assert!(
        harness.query_by_label("parent-only").is_none(),
        "a parent run is not an assigned run"
    );
    assert!(harness.query_by_label("Model").is_none());
    let artifact = harness.get_by_label("artifacts/layout.png").rect();
    assert!(
        artifact.max.x <= 450.0,
        "task content should fit a narrow pane"
    );

    // When: the user follows a team claim to its owner.
    harness.get_by_label("run-3").click();
    harness.run();
    assert_eq!(harness.state(), &Some(TasksAction::OpenRun("run-3".into())));
}

#[test]
fn task_card_links_current_and_previous_attempts() {
    let mut model = DurableTasksModel::default();
    model.apply_event(&Event::new(OrchestratorEvent::TaskProgressed {
        task_id: "task-1".into(),
        run_id: "run-1".into(),
        progress: serde_json::json!({"status":"running", "last_artifact":"artifacts/valid.txt"}),
        reason: "working".into(),
    }));
    model.apply_event(&Event::new(OrchestratorEvent::TaskRetryScheduled {
        goal_id: "goal-1".into(),
        task_id: "task-1".into(),
        attempt: 1,
        new_run_id: "run-2".into(),
        reason: "provider unavailable".into(),
    }));
    let mut harness = Harness::builder()
        .with_size(egui::vec2(450.0, 500.0))
        .build_ui_state(
            move |ui, action: &mut Option<TasksAction>| {
                if let Some(next) = tasks_pane(ui, &model, &[], None, Some("task-1")) {
                    *action = Some(next);
                }
            },
            None,
        );
    harness.run();
    assert_eq!(harness.query_all_by_label("task-1").count(), 1);
    harness.get_by_label("retrying");
    harness.get_by_label("retry 1");
    for run in ["run-1", "run-2"] {
        harness.get_by_label(run).click();
        harness.run();
        assert_eq!(harness.state(), &Some(TasksAction::OpenRun(run.into())));
    }
}
