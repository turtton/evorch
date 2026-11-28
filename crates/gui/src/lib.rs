//! ADR 0007 に基づく egui + egui_dock ベースの workbench GUI。
//!
//! 層構造: `model` (egui 非依存 view model) -> `pty` / `terminal` / `events` (adapter) -> panes/app (Wave 3)。

// The CEF GUI is the only supported GUI build; the code paths for building
// without it are gone, so refuse the configuration instead of compiling a
// partial GUI.
#[cfg(not(feature = "cef"))]
compile_error!(
    "gui requires the `cef` feature (on by default): build without --no-default-features; \
     see docs/browser.md for the CEF distribution"
);

pub mod attachments;
mod composer_input;
pub mod core_client;
pub mod diff;
mod dock;
mod format;
mod keymap;
pub mod logging;
mod mcp_settings;
pub mod model;
mod panes;
mod pty;
mod repaint;
mod restore_metrics;
mod terminal;
pub mod theme;
pub mod window;
