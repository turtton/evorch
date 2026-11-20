//! R registration seam; SafeRestart remains unadvertised in 5a-I.
#[test]
fn safe_restart_is_not_implemented() {
    assert!(
        !workbench_protocol::handshake::Capability::IMPLEMENTED
            .iter()
            .any(|c| c.name() == "safe_restart.v1")
    );
}
