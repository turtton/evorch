//! R uses the real common service and #710, without advertising SafeRestart.
#[cfg(target_os = "linux")]
#[path = "public_restart_coordinator/contract.rs"]
mod contract;
#[test]
fn safe_restart_remains_unadvertised() {
    assert!(
        !workbench_protocol::handshake::Capability::IMPLEMENTED
            .iter()
            .any(|c| c.name() == "safe_restart.v1")
    );
}
