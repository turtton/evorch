//! PROVISIONAL names. The one place to change when I0
//! (`chore/integration-next-wave-registration`) fixes the environment variable
//! names and the manifest fields that hand the product artifact to a test job.
//!
//! Nothing else in the fixture spells these strings. The fixture reads exactly
//! these names; it never falls back to `CARGO_BIN_EXE_*` or to a path under
//! Cargo's target directory.

/// Path of the manifest that describes the immutable product artifact. The
/// artifact itself is named by the manifest's `artifact` field, relative to the
/// manifest's directory (or absolute).
pub const ENV_MANIFEST: &str = "EVORCH_F4P_PRODUCT_MANIFEST";
/// Optional renderer helper executable the product is told to use through its
/// own `EVORCH_RENDERER_EXEC` setting.
pub const ENV_RENDERER_HELPER: &str = "EVORCH_F4P_RENDERER_HELPER";
/// Directory the measurement tests write their CSV/JSON into.
pub const ENV_MEASURE_OUT: &str = "EVORCH_F4P_MEASURE_OUT";
/// Label of the measurement condition set ("first" samples need a fresh copy).
pub const ENV_MEASURE_LABEL: &str = "EVORCH_F4P_MEASURE_LABEL";

/// Manifest schema revision. Bump together with `ProductManifest`.
pub const MANIFEST_SCHEMA: u32 = 1;

/// The variable a test job's Cargo sets for the harness's own builds. Named
/// only so that the fixture can prove it never selects the product from it:
/// the value is read at run time (never with a compile-time macro) and an
/// artifact that resolves to the same file is rejected.
pub const CARGO_HARNESS_BIN_ENV: &str = "CARGO_BIN_EXE_evorch-core";

// Inputs of `write_manifest_for_built_artifact` (tooling that stands in for the
// artifact-building CI step until I0 provides it).
pub const ENV_BUILD_ELF: &str = "EVORCH_F4P_BUILD_ELF";
pub const ENV_BUILD_PROFILE: &str = "EVORCH_F4P_BUILD_PROFILE";
/// Space-separated Cargo arguments actually used for the build.
pub const ENV_BUILD_ARGS: &str = "EVORCH_F4P_BUILD_ARGS";
pub const ENV_ARTIFACT_DIR: &str = "EVORCH_F4P_ARTIFACT_DIR";
