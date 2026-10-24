//! A live parent continues its finished child with a message, under the
//! parent's current authority, unless the operator stopped or cancelled it.
mod support;

use event_bus::{AgentMessageKind, AgentRunPhase, EventBus};
use providers::FinishReason;
use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
use runtime::{AgentRuntime, Role, RunConfig, RunId, RunRestoreFailure, RunStore, RuntimeError};
use std::sync::Arc;
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, text_response};
use tokio::sync::Notify;
use tools::ToolExecutor;

struct Fixture {
    runtime: AgentRuntime,
    model: Arc<ScriptedModel>,
    parent: RunId,
    child: RunId,
    permit: OwnerPermit,
    child_gate: Arc<Notify>,
    storage: Storage,
    config: StorageConfig,
    _dir: tempfile::TempDir,
}

/// An owned, still-running parent whose child is gated before its first answer.
async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("continuation.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let model = Arc::new(ScriptedModel::new([]));
    model.gate_key("PARENT", Arc::new(Notify::new())).await;
    let child_gate = Arc::new(Notify::new());
    model.gate_key("CHILD", Arc::clone(&child_gate)).await;
    model
        .add_keyed(
            "CHILD",
            [
                Ok(text_response("first result", FinishReason::Stop)),
                Ok(text_response("follow-up handled", FinishReason::Stop)),
            ],
        )
        .await;
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_sequential_run_ids()
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let registry_path = dir.path().join("owners.db");
    let owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "first".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    Registry::open(&registry_path)
        .unwrap()
        .start(&owner)
        .unwrap();
    let permit = OwnerPermit {
        registry_path,
        thread_id: owner.thread_id,
        lease: owner.lease,
        run_id: None,
    };
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".into(),
        RunConfig {
            ownership: Some(permit.clone()),
            ..Default::default()
        },
    );
    model.wait_for_request(0).await;
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .unwrap();
    model.wait_for_request(1).await;
    Fixture {
        runtime,
        model,
        parent,
        child,
        permit,
        child_gate,
        storage,
        config,
        _dir: dir,
    }
}

impl Fixture {
    async fn finish_child(&self) {
        self.child_gate.notify_one();
        assert_eq!(
            self.runtime.wait(self.child).await.unwrap(),
            AgentRunPhase::Done
        );
    }

    fn send(&self, text: &str) -> Result<String, RuntimeError> {
        self.runtime
            .send_agent_message(self.parent, self.child, AgentMessageKind::Send, text, None)
    }

    async fn last_request(&self) -> String {
        serde_json::to_string(self.model.observed().await.last().unwrap()).unwrap()
    }

    async fn shutdown(self) {
        self.runtime.cancel(self.parent).unwrap();
        self.runtime.wait(self.parent).await.unwrap();
    }
}

#[tokio::test]
async fn parent_message_continues_a_finished_owned_child_with_its_history() {
    // Given: a child that inherited the parent's ownership and finished.
    let fixture = fixture().await;
    fixture.finish_child().await;
    let record = storage::Database::open(&fixture.config)
        .unwrap()
        .run_context(&fixture.child.to_string())
        .unwrap()
        .unwrap();
    assert!(
        !record.restorable,
        "an owned child is not restorable from disk alone"
    );
    // When: the parent sends a follow-up.
    fixture.child_gate.notify_one();
    fixture.send("follow up").unwrap();
    // Then: the same child continues with its earlier result in context.
    assert_eq!(
        fixture.runtime.wait(fixture.child).await.unwrap(),
        AgentRunPhase::Done
    );
    let request = fixture.last_request().await;
    assert!(request.contains("first result"), "{request}");
    assert!(request.contains("follow up"), "{request}");
    fixture.shutdown().await;
}

#[tokio::test]
async fn parent_message_reports_interrupted_effects_instead_of_refusing() {
    // Given: a finished child whose snapshot recorded a side-effecting call
    // that may have run without its result being saved.
    let fixture = fixture().await;
    fixture.finish_child().await;
    let database = storage::Database::open(&fixture.config).unwrap();
    let mut record = database
        .run_context(&fixture.child.to_string())
        .unwrap()
        .unwrap();
    let mut descriptor: serde_json::Value = serde_json::from_str(&record.config_json).unwrap();
    descriptor["interrupted_tool_calls"] = serde_json::json!([{
        "call_id": "call-1",
        "tool_name": "shell",
        "result_observed": false,
        "may_have_side_effects": true
    }]);
    record.config_json = descriptor.to_string();
    fixture
        .storage
        .handle()
        .upsert_run_context(&record)
        .unwrap();
    // When: the parent sends a follow-up.
    fixture.child_gate.notify_one();
    fixture.send("follow up").unwrap();
    // Then: the child continues and is told the outcome is unknown.
    assert_eq!(
        fixture.runtime.wait(fixture.child).await.unwrap(),
        AgentRunPhase::Done
    );
    let request = fixture.last_request().await;
    assert!(
        request.contains("ToolExecutionOutcomeUnknown") && request.contains("call-1"),
        "{request}"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn operator_stop_holds_until_the_operator_resumes_the_root() {
    // Given: the operator stopped the child.
    let fixture = fixture().await;
    fixture
        .runtime
        .stop(fixture.child, runtime::StopScope::SelfOnly)
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(fixture.child).await.unwrap(),
        AgentRunPhase::Stopped
    );
    // When/Then: the parent cannot undo the stop by messaging the child.
    assert!(matches!(
        fixture.send("keep going"),
        Err(RuntimeError::RunRestoreFailed {
            reason: RunRestoreFailure::StoppedByOperator,
            ..
        })
    ));
    // When: the operator stops and resumes the root conversation.
    fixture
        .runtime
        .stop(fixture.parent, runtime::StopScope::SelfOnly)
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(fixture.parent).await.unwrap(),
        AgentRunPhase::Stopped
    );
    fixture
        .runtime
        .resume_chat(
            fixture.parent,
            RunConfig {
                ownership: Some(fixture.permit.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    // Then: the resumed parent may continue the child again.
    fixture.child_gate.notify_one();
    fixture.send("keep going").unwrap();
    assert_eq!(
        fixture.runtime.wait(fixture.child).await.unwrap(),
        AgentRunPhase::Done
    );
    assert!(fixture.last_request().await.contains("keep going"));
    fixture.shutdown().await;
}

#[tokio::test]
async fn cancelled_child_is_never_continued_by_messages() {
    let fixture = fixture().await;
    fixture.runtime.cancel(fixture.child).unwrap();
    assert_eq!(
        fixture.runtime.wait(fixture.child).await.unwrap(),
        AgentRunPhase::Error
    );
    assert!(matches!(
        fixture.send("keep going"),
        Err(RuntimeError::RunRestoreFailed {
            reason: RunRestoreFailure::Cancelled,
            ..
        })
    ));
    fixture.shutdown().await;
}
