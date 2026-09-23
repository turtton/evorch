mod support;

use std::sync::Arc;

use agents::{NetworkAccess, Role};
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::restore::RunRestoreDescriptor;
use runtime::{AgentRuntime, RunConfig, RunStore};
use serde_json::{Value, json};
use storage::{Database, Storage, StorageConfig};
use support::{ScriptedModel, text_response, tool_response};
use tools::ToolExecutor;

async fn delegate(
    parent_access: NetworkAccess,
    requested: Option<Value>,
    background: bool,
) -> (Option<NetworkAccess>, String, bool) {
    delegate_as("worker", parent_access, requested, background).await
}

async fn delegate_as(
    role: &str,
    parent_access: NetworkAccess,
    requested: Option<Value>,
    background: bool,
) -> (Option<NetworkAccess>, String, bool) {
    let mut input = json!({"role":role, "prompt":"CHILD", "background":background});
    if let Some(requested) = requested {
        input["network_access"] = requested;
    }
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "ROOT",
            [
                Ok(tool_response("child", "delegate", input)),
                Ok(text_response("done", FinishReason::Stop)),
            ],
        )
        .await;
    model
        .add_keyed(
            "CHILD",
            [Ok(text_response("child done", FinishReason::Stop))],
        )
        .await;
    let dir = tempfile::tempdir().expect("temporary store");
    let config = StorageConfig {
        db_path: dir.path().join("runs.sqlite3"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).expect("storage");
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(bus.clone(), Arc::new(ToolExecutor::new(bus)), model.clone())
        .with_run_store(RunStore::open(&config, storage.handle()).expect("run store"));
    let root = runtime.delegate_background(
        Role::Orchestrator,
        "ROOT".into(),
        RunConfig {
            network_access: parent_access,
            ..RunConfig::default()
        },
    );
    assert_eq!(runtime.wait(root).await, Ok(AgentRunPhase::Done));
    let children: Vec<_> = runtime
        .list_agents()
        .into_iter()
        .filter(|run| run.run_id != root)
        .collect();
    assert!(children.len() <= 1);
    let child_access = if let Some(child) = children.first() {
        assert_eq!(runtime.wait(child.run_id).await, Ok(AgentRunPhase::Done));
        let record = Database::open(&config)
            .expect("database")
            .run_context(&child.run_id.to_string())
            .expect("child context")
            .expect("persisted child");
        let descriptor: RunRestoreDescriptor =
            serde_json::from_str(&record.config_json).expect("child descriptor");
        Some(descriptor.network_access)
    } else {
        None
    };
    let observed = model.observed().await;
    let (content, is_error) = observed
        .iter()
        .flatten()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == "child" => {
                let ToolResultContent::Text { text } = content.first().expect("result text");
                Some((text.clone(), *is_error))
            }
            _ => None,
        })
        .expect("parent observed delegate result");
    (child_access, content, is_error)
}

#[tokio::test]
async fn delegate_inherits_parent_network_access_for_foreground_and_background_children() {
    for background in [false, true] {
        for access in [
            NetworkAccess::Denied,
            NetworkAccess::OptIn,
            NetworkAccess::Allowed,
        ] {
            // Omitted arguments preserve the actual parent authority in the child snapshot.
            let (child, result, failed) = delegate(access, None, background).await;
            assert!(!failed, "{result}");
            assert_eq!(child, Some(access));
        }
    }
}

#[tokio::test]
async fn delegate_accepts_equal_or_reduced_network_access() {
    for background in [false, true] {
        for (parent, requested, expected) in [
            (NetworkAccess::Denied, "denied", NetworkAccess::Denied),
            (NetworkAccess::OptIn, "denied", NetworkAccess::Denied),
            (NetworkAccess::OptIn, "opt_in", NetworkAccess::OptIn),
            (NetworkAccess::Allowed, "denied", NetworkAccess::Denied),
            (NetworkAccess::Allowed, "opt_in", NetworkAccess::OptIn),
            (NetworkAccess::Allowed, "allowed", NetworkAccess::Allowed),
        ] {
            let (child, result, failed) =
                delegate(parent, Some(json!(requested)), background).await;
            assert!(!failed, "{result}");
            assert_eq!(child, Some(expected));
        }
    }
}

#[tokio::test]
async fn delegate_rejects_network_escalation_before_creating_any_child() {
    for background in [false, true] {
        for (parent, requested) in [
            (NetworkAccess::Denied, "opt_in"),
            (NetworkAccess::Denied, "allowed"),
            (NetworkAccess::OptIn, "allowed"),
        ] {
            let (child, result, failed) =
                delegate(parent, Some(json!(requested)), background).await;
            assert!(failed, "{result}");
            assert!(
                result.contains("network_access") && result.contains("exceeds parent"),
                "{result}"
            );
            assert_eq!(child, None, "an escalation must not register a child");
        }
    }
}

#[tokio::test]
async fn delegate_rejects_invalid_network_arguments_before_creating_any_child() {
    for requested in [json!("unrestricted"), json!(true), json!(1)] {
        let (child, result, failed) =
            delegate(NetworkAccess::Allowed, Some(requested), false).await;
        assert!(failed, "{result}");
        assert!(result.starts_with("invalid arguments:"), "{result}");
        assert_eq!(child, None);
    }
}

#[tokio::test]
async fn web_researcher_defaults_to_allowed_even_when_parent_network_is_denied() {
    for background in [false, true] {
        for parent in [
            NetworkAccess::Denied,
            NetworkAccess::OptIn,
            NetworkAccess::Allowed,
        ] {
            let (child, result, failed) =
                delegate_as("web_researcher", parent, None, background).await;
            assert!(!failed, "{result}");
            assert_eq!(child, Some(NetworkAccess::Allowed));
        }
    }
}

#[tokio::test]
async fn web_researcher_accepts_explicit_network_access_restrictions() {
    for background in [false, true] {
        for (requested, expected) in [
            ("denied", NetworkAccess::Denied),
            ("opt_in", NetworkAccess::OptIn),
            ("allowed", NetworkAccess::Allowed),
        ] {
            let (child, result, failed) = delegate_as(
                "web_researcher",
                NetworkAccess::Denied,
                Some(json!(requested)),
                background,
            )
            .await;
            assert!(!failed, "{result}");
            assert_eq!(child, Some(expected));
        }
    }
}
