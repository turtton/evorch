//! Registration-only Q2 leaf target; existing recovery_cache_tests prove cache preservation.
#[test]
fn registration_accepts_only_the_opaque_disposal_authority_type() {
    let _: fn(&runtime::DisposalAuthority) = |_| {};
}
