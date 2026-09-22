use std::sync::{Arc, mpsc};
use std::time::Duration;

use event_bus::{Event, EventBus, OrchestratorEvent};
use gui::{
    app::WorkbenchState, events::EventPump, fixture::DemoSource, headless::HeadlessWorkbench,
};
use workspace_ui::{PanelId, UiSettings};

#[test]
fn durable_tasks_panel_lists_queued_then_retrying_then_completed_with_last_artifact()
-> Result<(), Box<dyn std::error::Error>> {
    // Given: the real workbench subscribed through EventPump, not a direct model fold.
    let runtime = tokio::runtime::Runtime::new()?;
    let bus = EventBus::new(32);
    let (tx, rx) = mpsc::channel();
    let pump = EventPump::spawn(
        runtime.handle(),
        bus.subscribe(),
        Some(Arc::new(move || {
            let _ = tx.send(());
        })),
    );
    let mut state =
        WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())?.with_pump(pump);
    let path = state
        .dock()
        .find_tab(&PanelId::new("tasks-main"))
        .ok_or("Tasks panel missing")?;
    state
        .dock_mut()
        .set_active_tab(path)
        .map_err(|_| "activate Tasks")?;
    let mut workbench = HeadlessWorkbench::new(state, [1600.0, 900.0]);
    // When: a durable task is queued, retried, and completed through the event bus.
    let events = [
        (
            Event::new(OrchestratorEvent::TaskProgressed {
                task_id: "task-1".into(),
                run_id: "run-1".into(),
                progress: serde_json::json!({"status":"queued", "last_artifact":"artifacts/valid.txt"}),
                reason: "waiting for worker".into(),
            }),
            "queued",
        ),
        (
            Event::new(OrchestratorEvent::TaskRetryScheduled {
                goal_id: "goal-1".into(),
                task_id: "task-1".into(),
                attempt: 2,
                reason: "provider unavailable".into(),
                new_run_id: "run-2".into(),
            }),
            "retrying",
        ),
        (
            Event::new(OrchestratorEvent::TaskProgressed {
                task_id: "task-1".into(),
                run_id: "run-2".into(),
                progress: serde_json::json!({"status":"completed", "attempts":2}),
                reason: "task completed".into(),
            }),
            "completed",
        ),
    ];
    for (event, status) in events {
        bus.emit(event);
        rx.recv_timeout(Duration::from_secs(2))?;
        workbench.run();
        // Then: the actual panel exposes each state and preserves the last valid artifact.
        assert!(workbench.has_label(status), "missing {status}");
        assert!(workbench.has_label("artifacts/valid.txt"));
        if status != "queued" {
            assert!(workbench.has_label("retry 2"));
        }
        if let Some(output) = std::env::var_os("EVORCH_DURABLE_TASKS_EVIDENCE") {
            let output = std::path::PathBuf::from(output);
            std::fs::create_dir_all(&output)?;
            workbench
                .capture()?
                .save_png(&output.join(format!("durable-tasks-{status}.png")))?;
        }
    }
    Ok(())
}
