use std::sync::Arc;

use storage::thread_retention::TombstoneSet;

use super::super::{ThreadDeleted, TombstoneGuard};

pub(crate) fn tombstones(threads: &[&str], runs: &[&str]) -> TombstoneGuard {
    TombstoneGuard::new(TombstoneSet {
        threads: threads.iter().map(|id| (*id).to_owned()).collect(),
        runs: runs.iter().map(|id| (*id).to_owned()).collect(),
        tasks: Default::default(),
    })
}

// Given: a guard with one deleted thread and run / When: asked about them and others
// Then: only the deleted identities are refused, each with its own error.
#[test]
fn the_guard_refuses_exactly_the_deleted_identities() {
    let guard = tombstones(&["gone"], &["r-gone"]);
    assert_eq!(
        guard.check_thread("gone"),
        Err(ThreadDeleted::Thread("gone".into()))
    );
    assert_eq!(
        guard.check_run("r-gone"),
        Err(ThreadDeleted::Run("r-gone".into()))
    );
    assert!(guard.check_thread("kept").is_ok());
    assert!(guard.check_run("r-kept").is_ok());
    assert!(
        guard.check_thread("r-gone").is_ok(),
        "threads and runs are separate"
    );
}

// The runtime binding entry replaces the removed owner start/claim/attach
// entries: a deleted thread or run must never become a current root again.
#[tokio::test]
async fn runtime_binding_refuses_deleted_threads_and_runs() {
    struct NeverModel;
    #[async_trait::async_trait]
    impl crate::AgentModel for NeverModel {
        fn selected_model(&self, _: crate::Role, _: Option<&str>) -> String {
            "never".into()
        }
        async fn complete(
            &self,
            _: &crate::AgentInvocationContext,
            _: crate::Role,
            _: &[providers::Message],
            _: &[providers::ToolSpec],
        ) -> Result<providers::ChatResponse, crate::RuntimeError> {
            panic!("binding must not invoke the provider")
        }
    }
    let bus = Arc::new(event_bus::EventBus::new(32));
    let runtime = crate::AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(NeverModel),
    );
    let deleted_run = crate::RunId::new(1);
    assert!(
        runtime
            .set_tombstone_guard(tombstones(&["gone"], &[&deleted_run.to_string()]))
            .is_ok()
    );
    assert!(
        runtime.set_tombstone_guard(tombstones(&[], &[])).is_err(),
        "installed only once"
    );
    assert!(
        runtime
            .bind_thread_root("gone", crate::RunId::new(2))
            .is_err()
    );
    assert!(runtime.bind_thread_root("kept", deleted_run).is_err());
    assert!(runtime.goal_thread(deleted_run).is_none());
    let kept = crate::RunId::new(3);
    runtime.bind_thread_root("kept", kept).unwrap();
    assert_eq!(runtime.goal_thread(kept).as_deref(), Some("kept"));
}
