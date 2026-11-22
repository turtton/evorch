#![cfg(target_os = "linux")]
//! The published restart on the sealed product: one request over the public
//! wire moves the running core to its successor, and an ordinary run started by
//! a desktop-surface client goes on in the single successor core.
//!
//! Evidence class: product to product. Both sides are the artifact CI hands to
//! every product job (no feature, no test hook), driven only from outside.
#[path = "../shared-test-support/product_exec.rs"]
mod product_exec;
#[path = "../shared-test-support/public_restart_product.rs"]
mod public_restart_product;

use product_exec::artifact::ProductArtifact;
use workbench_protocol::namespaces::lifecycle::AdminOperationState;

fn assert_moved_once(evidence: &public_restart_product::Evidence) {
    assert_eq!(
        evidence.pid_after, evidence.pid_before,
        "exec keeps the pid"
    );
    assert!(
        evidence.exe_is_artifact_after,
        "the successor image is the sealed artifact"
    );
    assert!(
        evidence.epoch_after > evidence.epoch_before,
        "a new core epoch: {} -> {}",
        evidence.epoch_before,
        evidence.epoch_after
    );
    assert!(
        matches!(
            evidence.receipt_state,
            AdminOperationState::Pending | AdminOperationState::Started
        ),
        "{:?}",
        evidence.receipt_state
    );
    assert_eq!(
        evidence.extra_artifact_processes, 0,
        "no second core process under the product"
    );
    assert!(
        !evidence.lifetime_free_while_running,
        "the successor holds the lifetime lock: a second core cannot start"
    );
    assert_eq!(
        evidence.requests.len(),
        3,
        "the first turn is not replayed by the successor: one provider request per turn"
    );
    assert_eq!(evidence.remaining_scripts, 0);
}

#[test]
fn a_public_restart_moves_the_running_core_to_one_successor_and_the_run_goes_on() {
    let artifact = ProductArtifact::require();
    assert_moved_once(&public_restart_product::drive_with(&artifact, false));
}

/// The common case: the run is mid-turn (its provider response is still held)
/// when the restart is asked for. The restart waits for the turn instead of
/// being cancelled when the turn ends under it.
#[test]
fn a_restart_asked_while_a_turn_is_in_flight_goes_through_and_does_not_replay_the_turn() {
    let artifact = ProductArtifact::require();
    assert_moved_once(&public_restart_product::drive_with(&artifact, true));
}

#[test]
#[ignore = "role: the reaper of one supervised family (product_exec::reaper)"]
fn f4p_family_reaper() {
    product_exec::reaper::run();
}
