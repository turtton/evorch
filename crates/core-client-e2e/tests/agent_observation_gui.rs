//! REGISTRATION-ONLY PLACEHOLDER (integration PR I0). Not a feature test.
//!
//! This file exists so that the scope gate, the CI selection and, where it
//! applies, the cache gate name a target that exists. It checks the sealed
//! `evorch-core` artifact and manifest. Its success proves only that the target
//! is registered, in the cef-e2e job; it is NOT evidence that the feature
//! works. O-V2 replaces it with the real test (the GUI observation test through
//! real CEF and Chromium); a real test must fail, never skip, when its product
//! or browser is missing.

use test_support::product_artifact;

#[ignore = "registration-only placeholder: selected by the cef-e2e job with --run-ignored only --no-tests fail"]
#[test]
fn registration_only_placeholder_agent_observation_gui_checks_the_sealed_product_artifact() {
    let artifact = product_artifact::require_from_env();
    assert!(artifact.manifest.features.is_empty());
}
