//! Registration-only product-artifact target; it does not test restart behavior.
#[test]
#[ignore = "registration-only placeholder for public restart product tests"]
fn public_restart_product_placeholder_uses_sealed_artifact() {
    let artifact = test_support::product_artifact::require_from_env();
    assert!(artifact.manifest.features.is_empty());
}
