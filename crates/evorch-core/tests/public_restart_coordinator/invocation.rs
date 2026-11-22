//! A running CLI invocation refuses a public restart before anything is closed
//! or captured (ADR 0045 §10, provisional until 5b-R), with a reason the user
//! sees. The refusal is of the invocation, not of any connection.
use super::{fixture::*, result};
use workbench_protocol::errors::{ErrorCode, RetryAdvice};

#[tokio::test]
async fn a_running_cli_invocation_refuses_the_public_restart_with_a_visible_reason() {
    let f = Fixture::new().await;
    let gate = f.host.host.lock().unwrap().service().restart_gate();
    let request = f.request("with-cli").await;
    let lease = gate.enter_invocation().unwrap();

    let refusal = result(&f, request.clone()).await.unwrap_err();

    assert_eq!(refusal.code, ErrorCode::Busy);
    assert_eq!(refusal.retry, RetryAdvice::UserActionRequired);
    assert!(
        refusal.message.contains("CLI") && refusal.message.contains('1'),
        "the user is told why: {}",
        refusal.message
    );
    f.assert_running_untouched();
    assert_eq!(gate.running_invocations(), 1);

    drop(lease);
    gate.claim_restart_admission()
        .expect("the refused restart left no claim behind");
    gate.release_restart_admission();
    f.shutdown().await;
}
