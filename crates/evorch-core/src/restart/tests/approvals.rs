use super::*;
use sandbox::approval::{
    ApprovalDecision, ApprovalProvenance, ApprovalRegistry, ApprovalRequest, PendingApproval,
};

fn raise(host: &BootstrappedHost, call: &str) -> PendingApproval {
    ApprovalRegistry::for_bus(&host.runtime.event_bus()).register(
        ApprovalRequest {
            run_id: "run-awaiting-approval",
            thread_id: None,
            tool_name: "shell",
            call_id: call,
            input: None,
        },
        std::time::Duration::from_secs(300),
    )
}

#[tokio::test]
async fn a_pending_approval_refuses_a_restart_and_releases_its_admission() {
    let (_dir, mut host) = boot().await;
    let core = RestartCoordinator::new(&host);
    let producer = host.runtime.producer_gate();
    let approval = raise(&host, "c1");
    for _ in 0..2 {
        let error = core.request().unwrap_err();
        assert_eq!(
            error,
            RuntimeError::RestartRefusedByApprovals { pending: 1 }
        );
        let message = error.to_string();
        assert!(message.contains("approve or deny"), "{message}");
        assert!(message.contains("stop the run"), "{message}");
        assert_eq!(core.snapshot().phase, RestartPhase::Running);
        assert_eq!(core.0.gate.phase(), HostRestartPhase::Open);
        assert!(producer.try_enter().is_some());
        drop(core.0.gate.enter_invocation().unwrap());
    }
    let registry = ApprovalRegistry::for_bus(&host.runtime.event_bus());
    let snapshot = registry.get(approval.id()).unwrap();
    registry
        .resolve(
            &snapshot.id,
            snapshot.revision,
            ApprovalDecision::Deny,
            ApprovalProvenance::CLI,
        )
        .unwrap();
    let id = core.request().unwrap();
    core.wait_sealed(id).await.unwrap();
    assert_eq!(core.snapshot().cancel_reason, None);
    core.cancel(id).unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_approval_cancel_that_fails_closed_does_not_ask_for_a_retry() {
    let (_dir, mut host) = boot().await;
    let evaluations = host.restart_preparing_services().evaluations;
    let (release, returned) = tokio::sync::oneshot::channel::<()>();
    evaluations
        .spawn(async { returned.await.unwrap() })
        .unwrap();
    let core = RestartCoordinator::new(&host);
    let id = core.request().unwrap();
    let run = host.runtime.delegate_background(
        runtime::Role::Worker,
        "health failure".into(),
        Default::default(),
    );
    let db = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_terminal_insert BEFORE INSERT ON resume_intents BEGIN SELECT RAISE(ABORT, 'terminal intent failure'); END; CREATE TRIGGER reject_terminal_update BEFORE UPDATE ON resume_intents BEGIN SELECT RAISE(ABORT, 'terminal intent failure'); END;").unwrap();
    host.runtime.wait(run).await.unwrap();
    assert!(matches!(
        host.runtime.quiesce_state(),
        runtime::quiesce::QuiesceState::Failed { .. }
    ));
    assert_eq!(core.snapshot().phase, RestartPhase::PreparingDrains);
    let approval = raise(&host, "late");
    phase(&core, RestartPhase::FailedClosed).await;
    let snapshot = core.snapshot();
    assert_eq!(snapshot.cancel_reason, None);
    assert_eq!(snapshot.to_dto().cancel_reason, None);
    assert!(core.cancel(id).is_err());
    drop(approval);
    let _ = release.send(());
    db.execute_batch("DROP TRIGGER reject_terminal_insert; DROP TRIGGER reject_terminal_update;")
        .unwrap();
    assert!(host.shutdown().await.is_err());
}

#[tokio::test]
async fn an_approval_raised_after_acceptance_cancels_the_restart_and_says_why() {
    for preparing in [true, false] {
        let (_dir, mut host) = boot().await;
        let evaluations = host.restart_preparing_services().evaluations;
        let (release, returned) = tokio::sync::oneshot::channel::<()>();
        if preparing {
            evaluations
                .spawn(async { returned.await.unwrap() })
                .unwrap();
        }
        let core = RestartCoordinator::new(&host);
        let producer = host.runtime.producer_gate();
        let held = producer.try_enter().unwrap();
        let id = core.request().unwrap();
        let held_phase = if preparing {
            RestartPhase::PreparingDrains
        } else {
            RestartPhase::WaitingSafePoint
        };
        phase(&core, held_phase).await;
        let approval = raise(&host, "late");
        phase(&core, RestartPhase::Running).await;
        let snapshot = core.snapshot();
        assert_eq!(snapshot.id, None);
        assert_eq!(
            snapshot.cancel_reason,
            Some(RestartCancelReason::ApprovalPending { pending: 1 })
        );
        assert_eq!(
            snapshot.to_dto().cancel_reason,
            Some(
                workbench_protocol::namespaces::lifecycle::RestartCancelReason::ApprovalPending {
                    pending: 1
                }
            )
        );
        assert_eq!(core.0.gate.phase(), HostRestartPhase::Open);
        assert!(core.cancel(id).is_err());
        drop(approval);
        drop(held);
        let _ = release.send(());
        let next = core.request().unwrap();
        assert_ne!(next, id);
        assert_eq!(core.snapshot().cancel_reason, None);
        core.wait_sealed(next).await.unwrap();
        core.cancel(next).unwrap();
        host.shutdown().await.unwrap();
    }
}
