//! A parent can tell from run_output whether messaging a finished child
//! resumes it, instead of delegating the same work again.
mod support;

use event_bus::{AgentRunPhase, EventBus};
use providers::FinishReason;
use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
use runtime::{AgentRuntime, Role, RunConfig, RunRestoreFailure, RunStore};
use serde_json::json;
use std::sync::Arc;
use storage::{Storage, StorageConfig};
use support::{ScriptedModel, text_response, tool_response};
use tokio::sync::Notify;
use tools::ToolExecutor;

/// The parent's `run_output` result for its child, as the parent model sees it.
async fn parent_view_of_finished_child(stop_child: bool) -> serde_json::Value {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("visibility.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(256));
    let model = Arc::new(ScriptedModel::new([]));
    let parent_gate = Arc::new(Notify::new());
    model.gate_key("PARENT", Arc::clone(&parent_gate)).await;
    let child_gate = Arc::new(Notify::new());
    model.gate_key("CHILD", Arc::clone(&child_gate)).await;
    model
        .add_keyed(
            "PARENT",
            [
                Ok(tool_response(
                    "look",
                    "run_output",
                    json!({"run_id": "run-2"}),
                )),
                Ok(text_response("parent done", FinishReason::Stop)),
            ],
        )
        .await;
    model
        .add_keyed(
            "CHILD",
            [Ok(text_response("child result", FinishReason::Stop))],
        )
        .await;
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_sequential_run_ids()
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
    let registry_path = dir.path().join("owners.db");
    let owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "host".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    Registry::open(&registry_path)
        .unwrap()
        .start(&owner)
        .unwrap();
    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "PARENT".into(),
        RunConfig {
            ownership: Some(OwnerPermit {
                registry_path,
                thread_id: owner.thread_id,
                lease: owner.lease,
                run_id: None,
            }),
            ..Default::default()
        },
    );
    model.wait_for_request(0).await;
    let child = runtime
        .delegate_background_as_child(parent, Role::Worker, "CHILD", RunConfig::default())
        .unwrap();
    assert_eq!(child.to_string(), "run-2");
    model.wait_for_request(1).await;
    if stop_child {
        runtime.stop(child, runtime::StopScope::SelfOnly).unwrap();
    } else {
        child_gate.notify_one();
    }
    runtime.wait(child).await.unwrap();
    parent_gate.notify_one();
    let request = model.wait_for_request(2).await;
    runtime.cancel(parent).unwrap();
    runtime.wait(parent).await.unwrap();
    request
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            providers::ContentBlock::ToolResult {
                tool_call_id,
                content,
                ..
            } if tool_call_id == "look" => match &content[0] {
                providers::ToolResultContent::Text { text } => serde_json::from_str(text).ok(),
                _ => None,
            },
            _ => None,
        })
        .expect("run_output result in the parent's next request")
}

#[tokio::test]
async fn finished_owned_child_is_reported_as_continuable() {
    let view = parent_view_of_finished_child(false).await;
    assert_eq!(view["phase"], json!(AgentRunPhase::Done));
    assert_eq!(view["continuable"], true);
    assert!(view.get("continuation_blocked").is_none());
}

#[tokio::test]
async fn operator_stopped_child_reports_why_it_cannot_be_continued() {
    let view = parent_view_of_finished_child(true).await;
    assert_eq!(view["continuable"], false);
    assert_eq!(
        view["continuation_blocked"],
        RunRestoreFailure::StoppedByOperator.to_string()
    );
}
