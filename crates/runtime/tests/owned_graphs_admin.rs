//! Registration-only Q2 graph observation target, not an enabled management path.
#[test]
fn registration_keeps_manual_reservation_separate_from_disposal() {
    let _: fn(runtime::ManualActivationReservation) = |_| {};
    let _: fn(runtime::DisposalAuthority) = |_| {};
}
