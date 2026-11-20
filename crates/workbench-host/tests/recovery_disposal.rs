//! Registration-only Q2 disposal facade target. No leaf is installed here.
#[test]
fn registration_exposes_no_execution_conversion() {
    let _: fn(&runtime::DisposalAuthority) = |_| {};
}
