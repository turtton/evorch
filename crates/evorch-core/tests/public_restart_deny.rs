#![cfg(target_os = "linux")]
//! Refusals of the published restart on the sealed product: a request bound to
//! a target revision or core epoch that is no longer true, and one that was
//! not confirmed, never start a restart. The core stays the same process on
//! the same epoch and the run it holds goes on.
//!
//! The other representative refusals are exercised where their causes can be
//! made: an unauthenticated Web/Agent request in
//! `admin_composition::restart::tests` and `public_restart_contracts`; an audit
//! that cannot be written (a test-only storage fault) in
//! `public_restart_coordinator`; undisposed graphs in
//! `admin_composition::restart::tests::live_undisposed_in_the_composed_physical_host_refuses_before_gate_or_transaction`.
#[path = "../shared-test-support/product_exec.rs"]
mod product_exec;
#[path = "../shared-test-support/public_restart_product.rs"]
mod public_restart_product;

use product_exec::artifact::ProductArtifact;
use workbench_protocol::namespaces::lifecycle::RestartPhase;

#[test]
fn stale_or_unconfirmed_restart_requests_are_refused_and_change_nothing() {
    let artifact = ProductArtifact::require();
    let evidence = public_restart_product::drive_refusals(&artifact);
    assert_eq!(evidence.answers.len(), 3);
    for answer in &evidence.answers {
        assert!(
            !answer.contains("Succeeded") && !answer.starts_with("accepted Pending"),
            "a refused request is not accepted as a restart: {:?}",
            evidence.answers
        );
    }
    assert!(
        evidence
            .phases_after
            .iter()
            .all(|phase| *phase == RestartPhase::Running),
        "no restart is under way after a refusal: {:?}",
        evidence.phases_after
    );
    assert_eq!(evidence.pid_after, evidence.pid_before);
    assert_eq!(evidence.epoch_after, evidence.epoch_before, "no new epoch");
    assert_eq!(evidence.follow_up_run, evidence.run, "the same run goes on");
    assert_eq!(evidence.extra_artifact_processes, 0);
    assert_eq!(
        evidence.provider_requests, 2,
        "one request per turn, none replayed"
    );
}

#[test]
#[ignore = "role: the reaper of one supervised family (product_exec::reaper)"]
fn f4p_family_reaper() {
    product_exec::reaper::run();
}
