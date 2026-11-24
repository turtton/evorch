use super::{fixture::*, result};
use sandbox::approval::{ApprovalDecision, ApprovalProvenance, ApprovalRegistry, ApprovalRequest};
use workbench_protocol::errors::{ErrorCode, RetryAdvice};

#[tokio::test]
async fn a_pending_approval_refuses_the_public_restart_until_it_is_resolved() {
    let f = Fixture::new().await;
    let gate = f.host.host.lock().unwrap().service().restart_gate();
    let registry = ApprovalRegistry::for_bus(&f.host.runtime.event_bus());
    let approval = registry.register(
        ApprovalRequest {
            run_id: "run-awaiting-approval",
            thread_id: None,
            tool_name: "shell",
            call_id: "c1",
            input: None,
        },
        std::time::Duration::from_secs(300),
    );
    let request = f.request("with-approval").await;

    let refusal = result(&f, request).await.unwrap_err();

    assert_eq!(refusal.code, ErrorCode::Busy);
    assert_eq!(refusal.retry, RetryAdvice::UserActionRequired);
    assert!(
        refusal.message.contains("approve or deny") && refusal.message.contains("stop the run"),
        "the user is told what to do: {}",
        refusal.message
    );
    f.assert_running_untouched();
    gate.claim_restart_admission()
        .expect("the refused restart left no claim behind");
    gate.release_restart_admission();

    let pending = registry.get(approval.id()).unwrap();
    registry
        .resolve(
            &pending.id,
            pending.revision,
            ApprovalDecision::Approve,
            ApprovalProvenance::CLI,
        )
        .unwrap();
    let request = f.request("after-approval").await;
    result(&f, request).await.unwrap();
    assert!(f.core.snapshot().id.is_some());
    f.shutdown().await;
}
