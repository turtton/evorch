//! Q1 registration seam, not a quarantine mutation implementation.
#[test]
fn quarantine_claim_is_not_transport_authority() {
    use workbench_authority::IngressContext;
    use workbench_protocol::{
        envelope::{Provenance, Surface},
        ids::ClientId,
    };
    assert!(
        IngressContext::unverified(Provenance::human(
            Surface::Web,
            ClientId::new("admin").unwrap()
        ))
        .authorize()
        .is_err()
    );
}
