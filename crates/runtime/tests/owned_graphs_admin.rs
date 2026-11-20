//! Leaf E2E: real physical bootstrap -> retained authority -> both databases
//! -> real scratch. Child exit, not a timeout, is the completion signal.
#[cfg(target_os = "linux")]
#[path = "../shared-test-support/physical_ownership.rs"]
#[allow(
    dead_code,
    reason = "shared physical fixture also serves other test suites"
)]
mod fixture;
#[cfg(target_os = "linux")]
#[path = "owned_graphs_admin/mod.rs"]
mod owned_graphs_admin;
#[cfg(target_os = "linux")]
fn main() {
    fixture::run_with(
        &[
            "release-unused",
            "release-prepared",
            "release-completed-absent",
            "release-completed-released",
            "release-adopted-released",
            "release-bound-root",
            "release-unknown",
            "release-owner-conflict",
            "release-reset-ack-lost",
            "release-executing",
            "release-waiting",
            "release-other-graph",
            "release-paused",
            "release-shell-teardown",
        ],
        owned_graphs_admin::check,
        owned_graphs_admin::shell::factory,
    );
}
#[cfg(not(target_os = "linux"))]
fn main() {}
