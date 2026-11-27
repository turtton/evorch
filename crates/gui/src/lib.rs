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
pub mod body;
pub mod browser;
pub mod composer_input;
pub mod core_client;
pub mod diff;
pub mod dock;
pub mod events;
pub mod fixture;
pub mod format;
pub mod keymap;
pub mod logging;
pub mod mcp;
pub mod mcp_settings;
pub mod model;
pub mod panes;
pub mod pty;
pub mod repaint;
pub mod restore_metrics;
pub mod terminal;
pub mod theme;
pub mod window;
