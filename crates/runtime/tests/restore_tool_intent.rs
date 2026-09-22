mod support;

use event_bus::{AgentRunPhase, EventBus};
use runtime::{AgentRuntime, Role, RunConfig, RunStore};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, tool_response};
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};

struct InterruptedWrite {
    name: &'static str,
    permissions: Permissions,
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
}
#[async_trait::async_trait]
impl Tool for InterruptedWrite {
    fn name(&self) -> &str {
        self.name
    }
    fn schema(&self) -> Value {
        json!({"type":"object"})
    }
    fn permissions(&self) -> Permissions {
        self.permissions
    }
    async fn execute(&self, _: Value) -> Result<ToolResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        std::future::pending().await
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Storage,
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("pending.sqlite3"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        Self {
            _dir: dir,
            config,
            storage,
            calls: Arc::new(AtomicUsize::new(0)),
            entered: Arc::new(tokio::sync::Notify::new()),
        }
    }
    fn runtime(&self) -> AgentRuntime {
        self.runtime_with_tool("write", Permissions::read_write())
    }
    fn runtime_with_tool(&self, name: &'static str, permissions: Permissions) -> AgentRuntime {
        let bus = Arc::new(EventBus::new(128));
        let mut executor = ToolExecutor::new(bus.clone());
        executor
            .register(Arc::new(InterruptedWrite {
                name,
                permissions,
                calls: self.calls.clone(),
                entered: self.entered.clone(),
            }))
            .unwrap();
        let model = Arc::new(ScriptedModel::new([Ok(tool_response(
            "write-1",
            name,
            json!({}),
        ))]));
        AgentRuntime::new(bus, Arc::new(executor), model)
            .with_run_store(RunStore::open(&self.config, self.storage.handle()).unwrap())
    }
}

#[tokio::test]
async fn write_intent_is_durable_before_effects_and_restart_never_repeats_it() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime();
    let root = runtime.delegate_background(
        Role::Worker,
        "write a file".into(),
        RunConfig {
            name: Some("chat:Worker:interrupted".into()),
            ..Default::default()
        },
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.entered.notified(),
    )
    .await
    .unwrap();
    let before = runtime.restore_diagnostics(root).unwrap().unwrap();
    assert!(!before.disk_restorable);
    assert!(!before.history_available_with_current_authority);
    assert_eq!(before.interrupted_tool_calls[0].call_id, "write-1");
    assert!(!before.interrupted_tool_calls[0].result_observed);
    // A fresh runtime sees the pre-dispatch record even if no terminal save occurs.
    let restarted = fixture.runtime();
    let error = restarted
        .delegate_chat(
            "interrupted",
            Role::Worker,
            "continue".into(),
            RunConfig::default(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("unresolved_tool_calls"));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    // Even a synthetic cancellation result is not evidence that a write had no effects.
    runtime.cancel(root).unwrap();
    assert_eq!(runtime.wait(root).await.unwrap(), AgentRunPhase::Error);
    let after = runtime.restore_diagnostics(root).unwrap().unwrap();
    assert!(!after.disk_restorable);
    assert_eq!(after.interrupted_tool_calls.len(), 1);
    assert!(after.interrupted_tool_calls[0].result_observed);
    assert!(
        runtime
            .continue_goal(root, "retry".into(), RunConfig::default())
            .is_err()
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_intent_write_prevents_tool_dispatch() {
    let fixture = Fixture::new();
    let connection = rusqlite::Connection::open(&fixture.config.db_path).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_pending_update BEFORE UPDATE ON run_contexts WHEN json_array_length(NEW.config_json, '$.interrupted_tool_calls') > 0 BEGIN SELECT RAISE(ABORT, 'disk full before tool dispatch'); END;").unwrap();
    let runtime = fixture.runtime();
    let run = runtime.delegate_background(Role::Worker, "write".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    // Failed writes preserve the previous, complete checkpoint.
    let diagnostics = runtime.restore_diagnostics(run).unwrap().unwrap();
    assert!(diagnostics.disk_restorable);
    assert!(diagnostics.interrupted_tool_calls.is_empty());
}

#[tokio::test]
async fn cancelled_read_only_tool_is_diagnostic_and_does_not_block_history() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime_with_tool("read", Permissions::read_only());
    let root = runtime.delegate_background(Role::Worker, "read".into(), RunConfig::default());
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.entered.notified(),
    )
    .await
    .unwrap();
    let pending = runtime.restore_diagnostics(root).unwrap().unwrap();
    assert!(pending.disk_restorable);
    assert!(!pending.interrupted_tool_calls[0].may_have_side_effects);
    runtime.cancel(root).unwrap();
    runtime.wait(root).await.unwrap();
    let stopped = runtime.restore_diagnostics(root).unwrap().unwrap();
    assert!(stopped.disk_restorable);
    assert!(stopped.interrupted_tool_calls[0].result_observed);
    runtime
        .continue_goal(root, "new request".into(), RunConfig::default())
        .unwrap();
    runtime.cancel(root).unwrap();
    runtime.wait(root).await.unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

struct UnobservedShell;
#[async_trait::async_trait]
impl Tool for UnobservedShell {
    fn name(&self) -> &str {
        "shell"
    }
    fn schema(&self) -> Value {
        json!({"type":"object"})
    }
    fn permissions(&self) -> Permissions {
        Permissions::process()
    }
    async fn execute(&self, _: Value) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::success("{\"job_id\":\"job-1\"}"))
    }
    fn has_unobserved_shell_jobs(&self, _: &str) -> bool {
        true
    }
}

#[tokio::test]
async fn unobserved_shell_result_remains_blocked_even_without_a_running_process() {
    let fixture = Fixture::new();
    let bus = Arc::new(EventBus::new(128));
    let mut executor = ToolExecutor::new(bus.clone());
    executor.register(Arc::new(UnobservedShell)).unwrap();
    let model = Arc::new(ScriptedModel::new([Ok(support::text_response(
        "done",
        providers::FinishReason::Stop,
    ))]));
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model)
        .with_run_store(RunStore::open(&fixture.config, fixture.storage.handle()).unwrap());
    let run = runtime.delegate_background(
        Role::Worker,
        "check terminal job".into(),
        RunConfig::default(),
    );
    runtime.wait(run).await.unwrap();
    let diagnostics = runtime.restore_diagnostics(run).unwrap().unwrap();
    assert!(!diagnostics.disk_restorable);
    assert!(!diagnostics.history_available_with_current_authority);
    assert!(
        diagnostics
            .interrupted_tool_calls
            .iter()
            .any(|call| call.call_id == "unobserved-shell-jobs" && call.may_have_side_effects)
    );
}
