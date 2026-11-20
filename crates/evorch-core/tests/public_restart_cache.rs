//! REGISTRATION-ONLY 5a-V (integration) target, NOT a restart cache feature test.
//! Existing core_cache tests remain the input-derived wire-prefix gate.
#[test]
fn registration_only_public_restart_cache_has_feature_free_product() {
    let artifact = test_support::product_artifact::require_from_env();
    assert!(artifact.manifest.features.is_empty());
}
