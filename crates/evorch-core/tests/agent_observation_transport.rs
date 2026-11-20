//! REGISTRATION-ONLY PLACEHOLDER (integration PR I0). Not a feature test.
//!
//! This file exists so that the scope gate, the CI selection and, where it
//! applies, the cache gate name a target that exists. It uses no product
//! artifact and no browser, so the offline test shards run it. Its success
//! proves only that the target is registered; it is NOT evidence that the
//! feature works. O-V1 replaces it with the real test (the observation
//! transport tests); a real test must fail, never skip, when its product or
//! browser is missing.

#[test]
fn registration_only_placeholder_agent_observation_transport_registers_the_target() {}
