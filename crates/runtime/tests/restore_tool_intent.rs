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
        self.runtime_with_model(
            name,
            permissions,
            Arc::new(ScriptedModel::new([Ok(tool_response(
                "write-1",
                name,
                json!({}),
            ))])),
        )
    }
    fn runtime_with_model(
        &self,
        name: &'static str,
        permissions: Permissions,
        model: Arc<ScriptedModel>,
    ) -> AgentRuntime {
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
    assert!(before.history_available_with_current_authority);
    assert_eq!(before.interrupted_tool_calls[0].call_id, "write-1");
    assert!(!before.interrupted_tool_calls[0].result_observed);
    // Preserve the pre-dispatch snapshot as if the process crashed before a
    // terminal save. Stop the actual test task before starting a fresh runtime.
    let pending = storage::Database::open(&fixture.config)
        .unwrap()
        .run_context(&root.to_string())
        .unwrap()
        .unwrap();
    assert!(
        runtime
            .delegate_chat(
                "interrupted",
                Role::Worker,
                "continue".into(),
                RunConfig::default()
            )
            .is_err()
    );
    runtime.cancel(root).unwrap();
    assert_eq!(runtime.wait(root).await.unwrap(), AgentRunPhase::Error);
    let after = runtime.restore_diagnostics(root).unwrap().unwrap();
    assert!(!after.disk_restorable);
    assert!(after.history_available_with_current_authority);
    assert!(after.interrupted_tool_calls[0].result_observed);
    fixture
        .storage
        .handle()
        .upsert_run_context(&pending)
        .unwrap();
    drop(runtime);
    let model = Arc::new(ScriptedModel::new([Ok(support::text_response(
        "continued",
        providers::FinishReason::Stop,
    ))]));
    let restarted = fixture.runtime_with_model("write", Permissions::read_write(), model.clone());
    let continued = restarted
        .delegate_chat(
            "interrupted",
            Role::Worker,
            "continue".into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(
        restarted.wait(continued).await.unwrap(),
        AgentRunPhase::Done
    );
    let observed = model.observed().await;
    let saved: Vec<providers::Message> = serde_json::from_str(&pending.messages_json).unwrap();
    assert!(observed[0].starts_with(&saved));
    let text = serde_json::to_string(&observed[0]).unwrap();
    assert!(text.contains("ToolExecutionOutcomeUnknown"));
    assert!(text.contains("write-1"));
    assert!(text.contains("continue"));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mixed_incomplete_batch_reports_all_call_ids_without_fabricating_tool_results() {
    let fixture = Fixture::new();
    let first = fixture.runtime();
    let root =
        first.delegate_background(Role::Worker, "batch request".into(), RunConfig::default());
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.entered.notified(),
    )
    .await
    .unwrap();
    let mut record = storage::Database::open(&fixture.config)
        .unwrap()
        .run_context(&root.to_string())
        .unwrap()
        .unwrap();
    first.cancel(root).unwrap();
    first.wait(root).await.unwrap();
    drop(first);
    let mut descriptor: runtime::restore::RunRestoreDescriptor =
        serde_json::from_str(&record.config_json).unwrap();
    descriptor
        .interrupted_tool_calls
        .push(runtime::restore::InterruptedToolCall {
            call_id: "read-completed-in-incomplete-batch".into(),
            tool_name: "read".into(),
            result_observed: true,
            may_have_side_effects: false,
        });
    record.config_json = serde_json::to_string(&descriptor).unwrap();
    fixture
        .storage
        .handle()
        .upsert_run_context(&record)
        .unwrap();
    let model = Arc::new(ScriptedModel::new([Err(runtime::RuntimeError::Model {
        reason: "test stop".into(),
    })]));
    let runtime = fixture.runtime_with_model("write", Permissions::read_write(), model.clone());
    runtime
        .continue_goal(root, "continue".into(), RunConfig::default())
        .unwrap();
    runtime.wait(root).await.unwrap();
    let observed = model.observed().await;
    let text = serde_json::to_string(&observed[0]).unwrap();
    assert_eq!(text.matches("ToolExecutionOutcomeUnknown").count(), 2);
    assert!(text.contains("read-completed-in-incomplete-batch"));
    assert!(
        !observed[0]
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(
                b,
                providers::ContentBlock::ToolResult { .. }
                    | providers::ContentBlock::ToolUse { .. }
            ))
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_write_continues_same_run_without_rewriting_or_repeating_the_call() {
    let fixture = Fixture::new();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("write-1", "write", json!({}))),
        Err(runtime::RuntimeError::Model {
            reason: "test stop after observing continuation".into(),
        }),
        Err(runtime::RuntimeError::Model {
            reason: "test second continuation".into(),
        }),
    ]));
    let runtime = fixture.runtime_with_model("write", Permissions::read_write(), model.clone());
    let root = runtime.delegate_background(
        Role::Worker,
        "original request".into(),
        RunConfig::default(),
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.entered.notified(),
    )
    .await
    .unwrap();
    runtime.cancel(root).unwrap();
    runtime.wait(root).await.unwrap();
    let record = storage::Database::open(&fixture.config)
        .unwrap()
        .run_context(&root.to_string())
        .unwrap()
        .unwrap();
    let saved: Vec<providers::Message> = serde_json::from_str(&record.messages_json).unwrap();
    for prompt in ["状況を教えて", "もう一度説明して"] {
        assert_eq!(
            runtime
                .continue_goal(root, prompt.into(), RunConfig::default())
                .unwrap(),
            root
        );
        runtime.wait(root).await.unwrap();
    }
    let observed = model.observed().await;
    for messages in &observed[1..] {
        assert!(messages.starts_with(&saved));
        assert_eq!(
            serde_json::to_string(messages)
                .unwrap()
                .matches("ToolExecutionOutcomeUnknown")
                .count(),
            1
        );
    }
    assert!(observed[2].starts_with(&observed[1]));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    let diagnostics = runtime.restore_diagnostics(root).unwrap().unwrap();
    assert_eq!(diagnostics.interrupted_tool_calls.len(), 1);
    assert!(diagnostics.history_available_with_current_authority);
    assert!(!diagnostics.disk_restorable);
}

#[tokio::test]
async fn existing_unresolved_snapshot_recovers_but_consumed_or_unsupported_records_do_not() {
    let fixture = Fixture::new();
    let first = fixture.runtime();
    let root = first.delegate_background(Role::Worker, "original".into(), RunConfig::default());
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.entered.notified(),
    )
    .await
    .unwrap();
    first.cancel(root).unwrap();
    first.wait(root).await.unwrap();
    let mut record = storage::Database::open(&fixture.config)
        .unwrap()
        .run_context(&root.to_string())
        .unwrap()
        .unwrap();
    let mut descriptor: runtime::restore::RunRestoreDescriptor =
        serde_json::from_str(&record.config_json).unwrap();
    // This is the exact on-disk shape saved for run-98 before this fix.
    descriptor.interrupted_tool_calls = vec![runtime::restore::InterruptedToolCall {
        call_id: "unobserved-shell-jobs".into(),
        tool_name: "shell".into(),
        result_observed: false,
        may_have_side_effects: true,
    }];
    descriptor.restorable = false;
    record.restorable = false;
    drop(first);
    let model = Arc::new(ScriptedModel::new([Err(runtime::RuntimeError::Model {
        reason: "test stop".into(),
    })]));
    let runtime = fixture.runtime_with_model("write", Permissions::read_write(), model.clone());
    for reason in [
        "snapshot_consumed",
        "復元対象外の実行状態: team_task, ownership",
    ] {
        descriptor.non_restorable_reason = Some(reason.into());
        record.config_json = serde_json::to_string(&descriptor).unwrap();
        fixture
            .storage
            .handle()
            .upsert_run_context(&record)
            .unwrap();
        assert!(
            runtime
                .continue_goal(root, "continue".into(), RunConfig::default())
                .is_err()
        );
    }
    descriptor.non_restorable_reason =
        Some("unresolved_tool_calls: inspect actual effects before starting a new run".into());
    // Existing unknown outcomes must not hide a renewable team's authority
    // checks, even for old records where the uncertainty reason took priority.
    let mut team_descriptor = descriptor.clone();
    team_descriptor.role = "Orchestrator".into();
    team_descriptor.renewable_team = Some(runtime::restore::TeamRestoreIdentity {
        team_id: "project:team".into(),
        coordinator_run_id: root,
    });
    record.role = "Orchestrator".into();
    record.config_json = serde_json::to_string(&team_descriptor).unwrap();
    fixture
        .storage
        .handle()
        .upsert_run_context(&record)
        .unwrap();
    assert!(
        runtime
            .continue_goal(root, "continue".into(), RunConfig::default())
            .unwrap_err()
            .to_string()
            .contains("current_team_authority_required")
    );
    record.role = "Worker".into();
    record.config_json = serde_json::to_string(&descriptor).unwrap();
    fixture
        .storage
        .handle()
        .upsert_run_context(&record)
        .unwrap();
    assert!(
        runtime
            .restore_diagnostics(root)
            .unwrap()
            .unwrap()
            .history_available_with_current_authority
    );
    runtime
        .continue_goal(root, "状況を教えて".into(), RunConfig::default())
        .unwrap();
    runtime.wait(root).await.unwrap();
    let observed = model.observed().await;
    let text = serde_json::to_string(&observed[0]).unwrap();
    assert!(text.contains("ToolExecutionOutcomeUnknown"));
    assert!(text.contains("unobserved-shell-jobs"));
    assert!(text.contains("状況を教えて"));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn recovery_notice_does_not_grant_disk_authoritative_delivery() {
    let fixture = Fixture::new();
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("write-1", "write", json!({}))),
        Ok(support::text_response(
            "child done",
            providers::FinishReason::Stop,
        )),
    ]));
    let runtime = fixture.runtime_with_model("write", Permissions::read_write(), model);
    let root = runtime.delegate_background(Role::Worker, "original".into(), RunConfig::default());
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fixture.entered.notified(),
    )
    .await
    .unwrap();
    runtime.cancel(root).unwrap();
    runtime.wait(root).await.unwrap();
    let child = runtime
        .delegate_background_as_child(root, Role::Explorer, "child", RunConfig::default())
        .unwrap();
    runtime.wait(child).await.unwrap();
    let error = runtime
        .send_agent_message(
            child,
            root,
            event_bus::AgentMessageKind::Send,
            "continue",
            None,
        )
        .unwrap_err();
    assert!(error.to_string().contains("unresolved_tool_calls"));
    assert!(
        runtime
            .restore_diagnostics(root)
            .unwrap()
            .unwrap()
            .history_available_with_current_authority
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

struct UnobservedShell {
    running: bool,
}
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
    fn has_running_shell_jobs(&self, _: &str) -> bool {
        self.running
    }
    fn has_unobserved_shell_jobs(&self, _: &str) -> bool {
        true
    }
}

#[tokio::test]
async fn retained_live_shell_still_prevents_recovery_after_cleanup_failure() {
    let fixture = Fixture::new();
    let bus = Arc::new(EventBus::new(128));
    let mut executor = ToolExecutor::new(bus.clone());
    executor
        .register(Arc::new(UnobservedShell { running: true }))
        .unwrap();
    let model = Arc::new(ScriptedModel::new([]));
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model)
        .with_run_store(RunStore::open(&fixture.config, fixture.storage.handle()).unwrap());
    let run = runtime.delegate_background(Role::Worker, "old request".into(), RunConfig::default());
    runtime.wait(run).await.unwrap();
    let error = runtime
        .continue_goal(run, "continue".into(), RunConfig::default())
        .unwrap_err();
    assert!(error.to_string().contains("shell_cleanup_required"));
}

#[tokio::test]
async fn unobserved_shell_result_is_available_as_conversation_error_without_a_running_process() {
    let fixture = Fixture::new();
    let bus = Arc::new(EventBus::new(128));
    let mut executor = ToolExecutor::new(bus.clone());
    executor
        .register(Arc::new(UnobservedShell { running: false }))
        .unwrap();
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
    assert!(diagnostics.history_available_with_current_authority);
    assert!(
        diagnostics
            .interrupted_tool_calls
            .iter()
            .any(|call| call.call_id == "unobserved-shell-jobs" && call.may_have_side_effects)
    );
}
