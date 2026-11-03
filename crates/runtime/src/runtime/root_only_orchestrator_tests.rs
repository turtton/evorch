use super::*;
use crate::{Role, RunConfig};
use std::sync::Arc;

struct PendingModel;

#[async_trait::async_trait]
impl crate::AgentModel for PendingModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "pending".into()
    }

    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        std::future::pending().await
    }
}

fn runtime() -> AgentRuntime {
    let bus = Arc::new(event_bus::EventBus::new(64));
    AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(PendingModel),
    )
}

fn is_denied(error: &RuntimeError, parent: RunId) -> bool {
    matches!(error, RuntimeError::OrchestratorChildDenied { parent: named } if *named == parent.to_string())
}

#[tokio::test]
async fn every_child_entry_refuses_an_orchestrator_and_registers_nothing() {
    // Given: a live root Orchestrator, which a root start still allows.
    let runtime = runtime();
    let root = runtime.delegate_background(Role::Orchestrator, "root".into(), RunConfig::default());
    let before = runtime.list_agents().len();

    // When: each parent-bearing entry is asked for an Orchestrator child.
    let background = runtime
        .delegate_background_as_child(root, Role::Orchestrator, "c", RunConfig::default())
        .expect_err("background child");
    let awaited = runtime
        .delegate_awaited_child(root, (Role::Orchestrator, "c".into(), RunConfig::default()))
        .expect_err("awaited child");
    let reserved_id = runtime.reserve_child_run_id(root).unwrap();
    let reserved = runtime
        .spawn_reserved_child(
            root,
            reserved_id,
            Role::Orchestrator,
            "c",
            RunConfig::default(),
        )
        .expect_err("reserved child");

    // Then: every one is refused with the parent named, and no run appears.
    for error in [&background, &awaited, &reserved] {
        assert!(is_denied(error, root), "{error:?}");
    }
    assert_eq!(runtime.list_agents().len(), before);

    // And: the infallible entry cannot start one either; its admission fails instead.
    let bypass = runtime.reserve_run_id();
    runtime.spawn_reserved(
        bypass,
        Some(root),
        Role::Orchestrator,
        "c",
        RunConfig::default(),
    );
    let error = runtime.wait(bypass).await.expect_err("admission fails");
    assert!(is_denied(&error, root), "{error:?}");
    assert!(runtime.list_agents().iter().all(|run| run.run_id != bypass));

    // And: other roles under the same parent, and a root Orchestrator, still start.
    runtime
        .delegate_background_as_child(root, Role::Worker, "w", RunConfig::default())
        .expect("a Worker child is admitted");
    runtime.delegate_background(Role::Orchestrator, "other".into(), RunConfig::default());
}

#[tokio::test]
async fn run_output_is_denied_between_siblings() {
    // Given: two Workers under one parent, one holding a result.
    let runtime = runtime();
    let root = runtime.delegate_background(Role::Orchestrator, "root".into(), RunConfig::default());
    let target = runtime
        .delegate_background_as_child(root, Role::Worker, "target", RunConfig::default())
        .unwrap();
    let sibling = runtime
        .delegate_background_as_child(root, Role::Worker, "sibling", RunConfig::default())
        .unwrap();

    // When / Then: a sibling is not directly related, the parent is.
    assert!(matches!(
        runtime.run_output(sibling, target),
        Err(RuntimeError::MessageDenied { .. })
    ));
    assert!(runtime.run_output(root, target).is_ok());
}
