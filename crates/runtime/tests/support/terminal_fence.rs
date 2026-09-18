use super::{Fixture, persist};
use event_bus::{Event, OrchestratorEvent};
use providers::FinishReason;
use storage::{StorageConfig, entity::TaskStatus};

async fn terminal_fence(cancel: bool, heartbeat: bool) {
    // Given: a real worker reaches a terminal task state.
    let mut fixture = Fixture::new(if cancel {
        FinishReason::Length
    } else {
        FinishReason::Stop
    })
    .await;
    fixture.finished().await;
    if cancel {
        fixture.handle.cancel_task("task").expect("cancel");
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }
    let before = fixture.handle.snapshot(&fixture.goal).expect("snapshot");
    let terminal = before.task_progress["task"].clone();
    let mut late = terminal.clone();
    late["status"] = serde_json::json!("running");
    late["heartbeat_at_ns"] = serde_json::json!(42);
    late["last_artifact"] = serde_json::json!("late invalid artifact");
    // When: same-generation progress or heartbeat arrives after termination.
    fixture
        .bus
        .emit(Event::new(OrchestratorEvent::TaskProgressed {
            task_id: "task".into(),
            run_id: fixture.worker.to_string(),
            progress: late,
            reason: if heartbeat {
                "task heartbeat"
            } else {
                "task progress"
            }
            .into(),
        }));
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let db = persist(
        &mut fixture,
        &StorageConfig {
            db_path: temp.path().join("terminal.db"),
            ..StorageConfig::default()
        },
    )
    .await;
    // Then: neither live ledger nor reopened SQLite loses the terminal state.
    assert_eq!(
        fixture
            .handle
            .snapshot(&fixture.goal)
            .expect("snapshot")
            .task_progress["task"],
        terminal
    );
    let record = db.task("task").expect("query").expect("task");
    let persisted: serde_json::Value =
        serde_json::from_str(record.progress.as_deref().expect("progress")).expect("JSON");
    assert_eq!(persisted, terminal);
    let events = db.events_all_ordered().expect("ordered events");
    let replay = runtime::orchestration::ledger::GoalLedger::replay_checked(
        events.iter().filter_map(|event| match &event.event.kind {
            event_bus::EventKind::Orchestrator(event) => Some(event),
            _ => None,
        }),
    )
    .expect("replay terminal fence");
    assert_eq!(
        replay[&fixture.goal].snapshot().task_progress["task"],
        terminal
    );
    assert_eq!(
        record.status,
        if cancel {
            TaskStatus::Cancelled
        } else {
            TaskStatus::Completed
        }
    );
    assert_eq!(record.last_artifact.as_deref(), Some("validated patch"));
    if cancel {
        assert_eq!(
            record.failure_reason.as_deref(),
            Some("cancelled by operator")
        );
    }
}

#[tokio::test]
async fn completed_rejects_late_progress() {
    terminal_fence(false, false).await;
}
#[tokio::test]
async fn completed_rejects_late_heartbeat() {
    terminal_fence(false, true).await;
}
#[tokio::test]
async fn cancelled_rejects_late_progress() {
    terminal_fence(true, false).await;
}
#[tokio::test]
async fn cancelled_rejects_late_heartbeat() {
    terminal_fence(true, true).await;
}
