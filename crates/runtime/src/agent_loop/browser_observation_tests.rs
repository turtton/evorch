//! Invoke a fake tool through run_agent, not a directly stamped executor.
use crate::escalation_review::support::{ScriptedModel, text_response, tool_response};
use crate::{AgentRuntime, Role, RunConfig, RunId};
use browser::agent::observation::{ExecutionStamp, ExecutionStamps};
use event_bus::{AgentRunPhase, EventBus};
use serde_json::json;
use std::sync::Arc;
use tokio::sync::{Notify, mpsc, watch};
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};
struct Probe {
    run: watch::Receiver<Option<RunId>>,
    captured: mpsc::Sender<Arc<ExecutionStamp>>,
    release: Arc<Notify>,
}
#[async_trait::async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        "read"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type":"object"})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    async fn execute(&self, _: serde_json::Value) -> Result<ToolResult, ToolError> {
        let mut run = self.run.clone();
        let run = run.wait_for(Option::is_some).await.unwrap().unwrap();
        let stamp = ExecutionStamps
            .for_creation(&run.to_string())
            .expect("canonical invocation stamp");
        self.captured.send(stamp).await.unwrap();
        self.release.notified().await;
        Ok(ToolResult::success("observed"))
    }
}
#[tokio::test]
async fn fresh_and_resumed_run_agent_deliver_their_own_creation_stamp() {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("stamp.sqlite3"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(64));
    let (run_tx, run_rx) = watch::channel(None);
    let (captured, mut stamps) = mpsc::channel(2);
    let release = Arc::new(Notify::new());
    let mut executor = ToolExecutor::new(bus.clone());
    executor
        .register(Arc::new(Probe {
            run: run_rx,
            captured,
            release: release.clone(),
        }))
        .unwrap();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("first", "read", json!({}))),
        Ok(text_response("done", providers::FinishReason::Stop)),
        Ok(tool_response("resumed", "read", json!({}))),
        Ok(text_response("done again", providers::FinishReason::Stop)),
    ]));
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model)
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime
        .delegate_chat(
            "stamp-thread",
            Role::Worker,
            "first input".into(),
            RunConfig::default(),
        )
        .unwrap();
    run_tx.send_replace(Some(run));
    let first = tokio::select! { stamp = stamps.recv() => stamp.unwrap(), ended = runtime.wait(run) => panic!("fresh ended without probe: {ended:?}") };
    assert_eq!(first.run, run.to_string());
    assert_eq!(
        first.incarnation,
        runtime
            .browser_execution_reader()
            .observe(run)
            .unwrap()
            .incarnation
    );
    release.notify_one();
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(
        runtime
            .continue_goal(run, "new input".into(), RunConfig::default())
            .unwrap(),
        run
    );
    let resumed = tokio::select! { stamp = stamps.recv() => stamp.unwrap(), ended = runtime.wait(run) => panic!("resumed ended without probe: {ended:?}") };
    assert_eq!(resumed.run, first.run);
    assert_eq!(
        resumed.incarnation,
        runtime
            .browser_execution_reader()
            .observe(run)
            .unwrap()
            .incarnation
    );
    assert_ne!(resumed.incarnation, first.incarnation);
    runtime.cancel(run).unwrap();
    release.notify_one();
    runtime.wait(run).await.unwrap();
    assert!(ExecutionStamps.for_creation(&run.to_string()).is_none());
}
