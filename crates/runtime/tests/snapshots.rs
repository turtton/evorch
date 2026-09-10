mod support;

use std::sync::Arc;
use event_bus::{EventBus, EventKind, ToolEvent};
use providers::FinishReason;
use runtime::{AgentRuntime, Role, RunConfig};
use runtime::snapshot::SnapshotService;
use support::{ScriptedModel, text_response, tool_response};

#[tokio::test]
async fn mutating_tool_emits_checkpoint_before_execution_and_can_be_undone() {
    // Given: a real edit tool and independent snapshot storage.
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("project");
    std::fs::create_dir(&root).expect("root");
    let file = root.join("file");
    std::fs::write(&file, "before").expect("file");
    let bus = Arc::new(EventBus::new(256));
    let mut events = bus.subscribe();
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(), Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("edit-1", "edit", serde_json::json!({"path": file, "new_string": "after"}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let service = Arc::new(SnapshotService::new(&root, &temp.path().join("snapshots")).expect("service"));
    let runtime = AgentRuntime::new(bus, executor, model).with_snapshots(service);
    // When: complete an edit through the public runtime surface.
    let run = runtime.delegate_background(Role::Worker, "edit".into(), RunConfig::default());
    runtime.wait(run).await.expect("wait");
    assert_eq!(std::fs::read_to_string(&file).expect("file"), "after");
    let mut checkpoint_seen = false;
    while let Ok(event) = events.recv().await {
        match event.kind {
            EventKind::Snapshot(snapshot) => {
                assert_eq!(snapshot.call_id, "edit-1");
                checkpoint_seen = true;
            }
            EventKind::Tool(ToolEvent::ToolStarted { .. }) => {
                assert!(checkpoint_seen);
                break;
            }
            _ => {}
        }
    }
    // Then: checkpoint predates execution and undo/redo restore actual bytes.
    assert!(checkpoint_seen);
    assert!(runtime.restore_snapshot(run, false).await.expect("undo").is_some());
    assert_eq!(std::fs::read_to_string(&file).expect("file"), "before");
    assert!(runtime.restore_snapshot(run, true).await.expect("redo").is_some());
    assert_eq!(std::fs::read_to_string(&file).expect("file"), "after");
}
