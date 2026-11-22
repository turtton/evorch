//! R uses the real common service and #710.
#[cfg(target_os = "linux")]
#[path = "public_restart_coordinator/contract.rs"]
mod contract;
/// `Capability::IMPLEMENTED` is the static set every peer may be offered. Safe
/// restart is not in it: `admin_composition::capability` advertises it at run
/// time, to verified local peers only, once the composition is complete.
#[test]
fn safe_restart_is_not_in_the_static_capability_set() {
    assert!(
        !workbench_protocol::handshake::Capability::IMPLEMENTED
            .iter()
            .any(|c| c.name() == "safe_restart.v1")
    );
}
