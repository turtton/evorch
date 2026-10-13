use super::{Ending, Fixture, persist};
use event_bus::{Event, OrchestratorEvent};
use providers::FinishReason;
use storage::{StorageConfig, entity::TaskStatus};

async fn terminal_fence(cancel: bool, heartbeat: bool) {
    // Given: a real worker reaches a terminal task state that is already persisted.
    let mut fixture = Fixture::new(if cancel {
        Ending::Cancel
    } else {
        Ending::Finish(FinishReason::Stop)
    })
    .await;
    let temp = tempfile::tempdir().expect("tempdir");
    let config = StorageConfig {
        db_path: temp.path().join("terminal.db"),
        ..StorageConfig::default()
    };
    let record = persist(&mut fixture, &config)
        .await
        .task("task")
        .expect("query")
        .expect("task");
    let terminal: serde_json::Value =
        serde_json::from_str(record.progress.as_deref().expect("progress")).expect("JSON");
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
    let db = persist(&mut fixture, &config).await;

    // Then: the reopened SQLite projection does not lose the terminal state.
    let record = db.task("task").expect("query").expect("task");
    let persisted: serde_json::Value =
        serde_json::from_str(record.progress.as_deref().expect("progress")).expect("JSON");
    assert_eq!(persisted, terminal);
    assert_eq!(
        record.status,
        if cancel {
            TaskStatus::Cancelled
        } else {
            TaskStatus::Completed
        }
    );
    if cancel {
        assert_eq!(record.failure_reason.as_deref(), Some("cancelled"));
    } else {
        assert_eq!(record.last_artifact.as_deref(), Some("validated patch"));
    }
    assert!(
        fixture
            .runtime
            .list_agents()
            .iter()
            .all(|agent| agent.run_id == fixture.worker)
    );
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
