//! Offline HTML artifact previews rendered by CEF off-screen into egui (ADR 0034).
//!
//! Frame handling, input translation and the network policy are plain Rust and
//! always compiled; everything that touches CEF is behind the `cef` feature.

pub mod frame;
pub mod input;
pub mod offline;

#[cfg(feature = "cef")]
mod agent;
#[cfg(feature = "cef")]
mod artifact;
#[cfg(feature = "cef")]
mod browser;
#[cfg(feature = "cef")]
pub mod devtools;
#[cfg(feature = "cef")]
mod host;
#[cfg(feature = "cef")]
mod runtime;
#[cfg(feature = "cef")]
mod snapshot;
#[cfg(feature = "cef")]
mod view;

#[cfg(feature = "cef")]
pub use agent::{AgentPage, CefTarget};
#[cfg(feature = "cef")]
pub use artifact::{ArtifactContext, NetLog};
#[cfg(feature = "cef")]
pub use browser::{OsrBrowser, Status};
#[cfg(feature = "cef")]
pub use runtime::{CefRuntime, RuntimeConfig, subprocess_app};
#[cfg(feature = "cef")]
pub use snapshot::{SNAPSHOT_SIZE, render_snapshot};
#[cfg(feature = "cef")]
pub use view::CefView;
