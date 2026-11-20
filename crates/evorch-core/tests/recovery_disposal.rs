//! Registration-only Q2 leaf cache target. startup_recovery remains the input-derived proof.
#[test]
fn registration_keeps_manual_activation_input_opaque() {
    let _: fn(runtime::ManualActivationReservation) = |_| {};
}
