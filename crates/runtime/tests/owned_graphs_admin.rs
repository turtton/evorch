//! Registration-only Q2 graph observation target, not an enabled management path.
fn registration_keeps_manual_reservation_separate_from_disposal() {
    let _: fn(runtime::ManualActivationReservation) = |_| {};
    let _: fn(runtime::DisposalAuthority) = |_| {};
}

#[path = "../shared-test-support/physical_test_harness.rs"]
mod harness;

fn main() {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if let Some(cases) = harness::select(
        &["registration_keeps_manual_reservation_separate_from_disposal"],
        &args,
    ) {
        for case in cases {
            assert_eq!(
                case,
                "registration_keeps_manual_reservation_separate_from_disposal"
            );
            registration_keeps_manual_reservation_separate_from_disposal();
        }
    }
}
