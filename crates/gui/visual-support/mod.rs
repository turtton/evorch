//! The pieces of the new GUI's screen-diff evidence (`core_client_capture`).
//!
//! The capture binary (`src/bin/core_client_capture.rs`) and the integration test
//! (`tests/core_client_visual.rs`) both include this directory with `#[path]`, so the
//! library API stays unchanged. The directory sits outside `src/` and `tests/` so
//! that the build cache keys it by content.
//!
//! - [`scenarios`] draws the real `CoreClientScreen` from a synthetic fixture.
//! - [`manifest`] is what a capture writes next to its PNGs.
//! - [`compare`] compares two captures pixel by pixel.
//! - [`environment`] and [`font`] pin the process a capture runs in.

pub mod compare;
pub mod environment;
pub mod font;
pub mod manifest;
pub mod scenarios;

/// The fixture's own source. Its hash goes into the manifest, so a report can tell a
/// changed fixture from a changed screen.
pub const FIXTURE_SOURCE: &[u8] = include_bytes!("scenarios.rs");
