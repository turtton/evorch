//! REGISTRATION-ONLY CN cache target, NOT a continuity feature test.
#[test]
fn registration_only_cli_continuity_cache_has_feature_free_product() {
    let artifact = test_support::product_artifact::require_from_env();
    assert!(artifact.manifest.features.is_empty());
}
