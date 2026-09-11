mod support;

use std::sync::Arc;

use event_bus::EventBus;
use runtime::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
use runtime::{AgentRuntime, RunConfig, RuntimeError};

#[tokio::test]
async fn run_commands_reject_previous_generation() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let owner = ThreadOwner::new(
        "thread".into(),
        Lease {
            owner_id: "first".into(),
            generation: 1,
            expires_at: 100,
        },
    );
    let mut registry = Registry::open(&path).expect("registry");
    registry.start(&owner).expect("start");
    let permit = OwnerPermit {
        registry_path: path,
        thread_id: "thread".into(),
        lease: owner.lease.clone(),
        run_id: None,
    };
    let bus = Arc::new(EventBus::new(32));
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(bus, executor, Arc::new(support::ScriptedModel::new([])));
    let run = runtime.delegate_background(
        agents::Role::Reviewer,
        "test".into(),
        RunConfig {
            ownership: Some(permit),
            ..RunConfig::default()
        },
    );
    registry
        .update("thread", |state| state.claim(&owner.lease, "next", 200, 50))
        .expect("claim");
    assert!(matches!(
        runtime.send_message(run, "late".into()),
        Err(RuntimeError::StaleOwnership { .. })
    ));
    assert!(matches!(
        runtime.compact(run),
        Err(RuntimeError::StaleOwnership { .. })
    ));
    assert!(matches!(
        runtime.set_model_preference(run, None),
        Err(RuntimeError::StaleOwnership { .. })
    ));
    assert!(runtime.restore_snapshot(run, false).await.is_err());
    runtime.cancel(run).expect("cancellation remains available");
}
