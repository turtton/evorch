//! REGISTRATION-ONLY PLACEHOLDER (integration PR I0). Not a feature test.
//!
//! This file exists so that the scope gate, the CI selection and, where it
//! applies, the cache gate name a target that exists. It is an independent
//! cache binary (CACHE_CONTRACT_FILTER in scripts/contract-test-filters.sh);
//! never merge it into another cache binary. It checks the sealed artifact and
//! manifest the cache gate job receives. Its success proves only that the
//! target is registered, in the cache gate; it is NOT evidence that the feature
//! works. F4-E replaces it with the real test (the product cache preservation
//! tests); a real test must fail, never skip, when its product or browser is
//! missing.

use test_support::product_artifact;

#[test]
fn registration_only_placeholder_f4_product_cache_checks_the_sealed_product_artifact() {
    let artifact = product_artifact::require_from_env();
    assert!(artifact.manifest.features.is_empty());
}
