//! Names the fixture reads from the environment, in one place.
//!
//! The product artifact itself is handed over by I0's contract
//! (`test_support::product_artifact`: `EVORCH_PRODUCT_CORE_EXE` and
//! `EVORCH_PRODUCT_MANIFEST`, a sealed release copy). Everything below is
//! local to F4-P. The fixture never falls back to `CARGO_BIN_EXE_*` or to a
//! path under Cargo's target directory.

/// Optional renderer helper executable the product is told to use through its
/// own `EVORCH_RENDERER_EXEC` setting.
pub const ENV_RENDERER_HELPER: &str = "EVORCH_F4P_RENDERER_HELPER";
/// Directory the measurement tests write their CSV into.
pub const ENV_MEASURE_OUT: &str = "EVORCH_F4P_MEASURE_OUT";
/// Label of the measurement condition set (`limited` / `loaded`).
pub const ENV_MEASURE_LABEL: &str = "EVORCH_F4P_MEASURE_LABEL";
/// MEASUREMENT ONLY: manifest of an artifact for a profile the sealed contract
/// does not cover (debug). When it is set, the measurement test uses that
/// artifact instead of the sealed release one. No product test reads it.
pub const ENV_MEASURE_MANIFEST: &str = "EVORCH_F4P_MEASURE_MANIFEST";

/// Schema revision of the measurement-only manifest (`ProductManifest`).
pub const MANIFEST_SCHEMA: u32 = 1;

/// The variable a test job's Cargo sets for the harness's own builds. Named
/// only so that the fixture can prove it never selects the product from it:
/// the value is read at run time (never with a compile-time macro) and an
/// artifact that resolves to the same file is rejected.
pub const CARGO_HARNESS_BIN_ENV: &str = "CARGO_BIN_EXE_evorch-core";

// Inputs of `write_measurement_manifest_for_built_artifact`.
pub const ENV_BUILD_ELF: &str = "EVORCH_F4P_BUILD_ELF";
pub const ENV_BUILD_PROFILE: &str = "EVORCH_F4P_BUILD_PROFILE";
/// Space-separated Cargo arguments actually used for the build.
pub const ENV_BUILD_ARGS: &str = "EVORCH_F4P_BUILD_ARGS";
pub const ENV_ARTIFACT_DIR: &str = "EVORCH_F4P_ARTIFACT_DIR";
