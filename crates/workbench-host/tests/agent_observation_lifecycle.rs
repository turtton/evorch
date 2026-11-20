//! REGISTRATION-ONLY PLACEHOLDER (integration PR I0). Not a feature test.
//!
//! This file exists so that the scope gate, the CI selection and, where it
//! applies, the cache gate name a target that exists. The browser-e2e job
//! selects it by the name held in that job's HOST_LIFECYCLE_TEST variable, the
//! one place to change when O-V1 renames the binary. Its success proves only
//! that the target is registered, in the browser-e2e job; it is NOT evidence
//! that the feature works. O-V1 replaces it with the real test (the host source
//! lifecycle test against a real Chromium); a real test must fail, never skip,
//! when its product or browser is missing.

#[ignore = "registration-only placeholder: selected by the browser-e2e job with --run-ignored only --no-tests fail"]
#[test]
fn registration_only_placeholder_agent_observation_lifecycle_registers_the_target() {}
