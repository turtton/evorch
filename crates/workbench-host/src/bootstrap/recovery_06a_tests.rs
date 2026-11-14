use super::*;
use runtime::{
    AgentInvocationContext, AgentModel, Role, RunConfig, RuntimeError,
    quiesce::{QuiesceState, RecoveryRecipe, ResumePolicy},
};
use std::collections::BTreeMap;

struct NoProviderCalls;
#[async_trait::async_trait]
impl AgentModel for NoProviderCalls {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "local/test".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        panic!("a parked run must not call a provider");
    }
}
struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}
async fn open(root: &std::path::Path) -> (tempfile::TempDir, BootstrappedHost) {
    let owners = test_socket_dir::tempdir();
    let boot = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: root.into(),
            project_root: root.into(),
            // Keep the Unix socket below SUN_LEN even with a long runner TMPDIR.
            ownership_root: owners.path().into(),
        },
        TestSeams {
            model: Arc::new(NoProviderCalls),
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap();
    (owners, boot)
}
fn start(boot: &BootstrappedHost) -> runtime::RunId {
    boot.runtime.delegate_background(
        Role::Worker,
        "park".into(),
        RunConfig {
            keep_alive: true,
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn failed_drain_still_flushes_bridge_without_releasing_owner() {
    let dir = test_socket_dir::SocketDir::new();
    let (_owners, mut boot) = open(dir.path()).await;
    let _permit = boot.owner.start("owned").unwrap();
    let bus = boot.runtime.event_bus().clone();
    boot.services.start("failing-drain", |stop| async move {
        let _ = stop.await;
        // Publish at the failed producer's terminal boundary, not before the
        // drain begins: this event still requires the bridge's durable ack.
        bus.emit(event_bus::Event::new(event_bus::DiagnosticEvent {
            source: "06a-test".into(),
            severity: event_bus::DiagnosticSeverity::Warning,
            code: "06a.failed-drain".into(),
            detail: "producer stopped with an error".into(),
            run_id: None,
            thread_id: None,
            call_id: None,
        }));
        Err("injected producer drain failure".into())
    });
    assert!(
        boot.shutdown()
            .await
            .unwrap_err()
            .to_string()
            .contains("injected producer drain failure")
    );
    assert!(
        boot.bridge.is_none(),
        "failed drain must not skip bridge acknowledgement and shutdown"
    );
    assert_ne!(
        boot.owner.attach("owned").unwrap().state,
        runtime::ownership::OwnerState::Released
    );
    let db = storage::Database::open(&boot.composition.storage).unwrap();
    assert_eq!(db.events_all_ordered().unwrap().iter().filter(|event|
        matches!(&event.event.kind, event_bus::EventKind::Diagnostic(d) if d.code == "06a.failed-drain")
    ).count(), 1);
}

#[tokio::test]
async fn original_restart_safe_point_still_allows_explicit_operator_shutdown() {
    let dir = test_socket_dir::SocketDir::new();
    let (_owners, mut boot) = open(dir.path()).await;
    let _permit = boot.owner.start("owned").unwrap();
    let run = start(&boot);
    boot.quiesce_for_restart().await.unwrap();
    assert_eq!(boot.runtime.quiesce_state(), QuiesceState::Checkpointed);
    boot.shutdown().await.unwrap();
    assert_eq!(
        boot.runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
    assert_eq!(
        boot.owner.attach("owned").unwrap().state,
        runtime::ownership::OwnerState::Released
    );
}
#[tokio::test]
async fn capture_without_explicit_restart_fence_cannot_commit_or_close_dispatch() {
    let dir = test_socket_dir::SocketDir::new();
    let (_owners, mut boot) = open(dir.path()).await;
    let run = start(&boot);
    assert!(
        recovery::capture(&boot.runtime, None, "missing-fence", &BTreeMap::new())
            .await
            .is_err()
    );
    assert_eq!(boot.runtime.quiesce_state(), QuiesceState::Running);
    boot.quiesce_for_restart().await.unwrap();
    assert!(
        recovery::capture(&boot.runtime, None, "still-missing", &BTreeMap::new())
            .await
            .is_err()
    );
    let shutdown = boot
        .runtime
        .fence_dispatch_shutdown(|| async { Ok::<(), RuntimeError>(()) })
        .await
        .unwrap();
    assert!(
        recovery::capture(
            &boot.runtime,
            Some(&shutdown),
            "wrong-kind",
            &BTreeMap::new()
        )
        .await
        .is_err()
    );
    assert!(recovery::plan(&boot.runtime).unwrap().components.is_empty());
    boot.shutdown().await.unwrap();
    assert_eq!(
        boot.runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
}
#[tokio::test]
async fn explicit_current_config_and_core_policy_capture_a_candidate_but_never_activate() {
    let dir = test_socket_dir::SocketDir::new();
    let (_owners, boot) = open(dir.path()).await;
    let run = start(&boot);
    boot.quiesce_for_restart().await.unwrap();
    let fence = boot.runtime.fence_dispatch_restart().await.unwrap();
    let recipes = BTreeMap::from([(
        run,
        RecoveryRecipe {
            settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
            policy: ResumePolicy::CoreOwned,
            goal_id: None,
            team: None,
            lane: None,
            descriptor: None,
        },
    )]);
    let ids = recovery::capture(&boot.runtime, Some(&fence), "host-capture", &recipes)
        .await
        .unwrap();
    assert_eq!(ids.len(), 1);
    let plan = recovery::plan(&boot.runtime).unwrap();
    assert_eq!(plan.components.len(), 1);
    assert_eq!(plan.components[0].runs, vec![run]);
    assert!(plan.report.recovered.is_empty());
    assert!(
        boot.runtime
            .resume_intents()
            .unwrap()
            .iter()
            .find(|r| r.run_id == run)
            .unwrap()
            .active
    );
    // Drop simulates process replacement: capture is not an operator Stop.
}

#[tokio::test]
async fn stale_restart_fence_cannot_capture_after_reopen_and_reseal() {
    let dir = test_socket_dir::SocketDir::new();
    let (_owners, boot) = open(dir.path()).await;
    let attempt = boot.runtime.begin_restart_quiesce().unwrap();
    boot.quiesce_for_restart().await.unwrap();
    let old = boot.runtime.fence_dispatch_restart().await.unwrap();
    boot.runtime.drain_dispatch_tails(&old).await.unwrap();
    boot.runtime.cancel_restart_quiesce(&attempt).unwrap();
    let next_attempt = boot.runtime.begin_restart_quiesce().unwrap();
    let current = boot.runtime.fence_dispatch_restart().await.unwrap();
    boot.runtime.drain_dispatch_tails(&current).await.unwrap();
    assert!(
        recovery::capture(&boot.runtime, Some(&old), "stale", &BTreeMap::new())
            .await
            .is_err()
    );
    assert!(recovery::plan(&boot.runtime).unwrap().components.is_empty());
    assert!(
        boot.runtime
            .capture_recovery_records(&old, &BTreeMap::new())
            .is_err()
    );
    boot.runtime
        .capture_recovery_records(&current, &BTreeMap::new())
        .unwrap();
    assert!(
        boot.runtime.cancel_restart_quiesce(&next_attempt).is_err(),
        "capture must exclude reopen, even before graph writes"
    );
}
