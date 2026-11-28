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
async fn open(root: &std::path::Path) -> (test_socket_dir::SocketDir, BootstrappedHost) {
    let owners = test_socket_dir::SocketDir::new();
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
async fn failed_drain_still_flushes_bridge_and_keeps_leases() {
    let dir = tempfile::tempdir().unwrap();
    let (_owners, mut boot) = open(dir.path()).await;
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
    assert!(boot.writer_leases.has_root_writer());
    let db = storage::Database::open(&boot.composition.storage).unwrap();
    assert_eq!(db.events_all_ordered().unwrap().iter().filter(|event|
        matches!(&event.event.kind, event_bus::EventKind::Diagnostic(d) if d.code == "06a.failed-drain")
    ).count(), 1);
}

#[tokio::test]
async fn original_restart_safe_point_still_allows_explicit_operator_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let (_owners, mut boot) = open(dir.path()).await;
    let run = start(&boot);
    boot.quiesce_for_restart().await.unwrap();
    assert_eq!(boot.runtime.quiesce_state(), QuiesceState::Checkpointed);
    boot.shutdown().await.unwrap();
    assert_eq!(
        boot.runtime.wait(run).await.unwrap(),
        event_bus::AgentRunPhase::Stopped
    );
    assert!(!boot.scratch.parent().join("owners.db").exists());
}
#[tokio::test]
async fn capture_without_explicit_restart_fence_cannot_commit_or_close_dispatch() {
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
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

struct GitToolModel;
#[async_trait::async_trait]
impl AgentModel for GitToolModel {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "owned-git-test".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        let returned = messages.iter().flat_map(|m| &m.content).any(|c| matches!(c, providers::ContentBlock::ToolResult { tool_call_id, .. } if tool_call_id == "git"));
        Ok(providers::ChatResponse {
            usage_reported: true,
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![if returned {
                    providers::ContentBlock::Text {
                        text: "done".into(),
                    }
                } else {
                    providers::ContentBlock::ToolUse {
                        id: "git".into(),
                        name: "git_diff".into(),
                        input: serde_json::json!({}),
                    }
                }],
            },
            usage: providers::Usage::default(),
            finish_reason: if returned {
                providers::FinishReason::Stop
            } else {
                providers::FinishReason::ToolUse
            },
        })
    }
}

#[tokio::test]
async fn core_ledger_owns_slug_reads_and_per_run_tools_and_fences_both() {
    for fenced in [false, true] {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let mut boot = BootstrappedHost::open_with_test_seams(
            BootstrapOptions {
                config_dir: root.clone(),
                project_root: root.clone(),
                ownership_root: dir.sockets().into(),
            },
            TestSeams {
                model: Arc::new(GitToolModel),
                sandbox: Arc::new(Direct),
            },
        )
        .await
        .unwrap();
        assert_eq!(boot.spawn_ledger.child_stats().registered, 0);
        if fenced {
            boot.spawn_ledger.close_fence();
        }
        assert!(boot.runtime.project_slug(&root).is_some());
        let after_slug = boot.spawn_ledger.child_stats().registered;
        assert_eq!(after_slug, u64::from(!fenced));
        let run = boot.runtime.delegate_background(
            Role::Worker,
            "read diff".into(),
            RunConfig::default(),
        );
        boot.runtime.wait(run).await.unwrap();
        let stats = boot.spawn_ledger.child_stats();
        assert_eq!(
            stats.registered,
            after_slug + u64::from(!fenced),
            "per-run executor must share the host fence"
        );
        assert_eq!(stats.registered, stats.reaped);
        assert_eq!(stats.unconfirmed, 0);
        boot.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn bootstrap_reports_the_final_bridge_drain_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (_owners, mut boot) = open(dir.path()).await;
    boot.bridge.as_mut().unwrap().flush().unwrap();
    let sql = rusqlite::Connection::open(&boot.composition.storage.db_path).unwrap();
    sql.execute_batch("CREATE TRIGGER reject_tail BEFORE INSERT ON events BEGIN SELECT RAISE(FAIL, 'reject tail'); END;").unwrap();
    boot.runtime
        .event_bus()
        .emit(event_bus::Event::new(event_bus::DiagnosticEvent {
            source: "test".into(),
            severity: event_bus::DiagnosticSeverity::Warning,
            code: "final-drain".into(),
            detail: "accepted tail event".into(),
            run_id: None,
            thread_id: None,
            call_id: None,
        }));
    assert!(boot.shutdown().await.is_err());
    assert!(boot.bridge.is_none());
}
