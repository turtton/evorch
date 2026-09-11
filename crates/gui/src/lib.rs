//! ADR 0007 に基づく egui + egui_dock ベースの workbench GUI。
//!
//! 層構造: `model` (egui 非依存 view model) -> `pty` / `events` (adapter) -> panes/app (Wave 3)。

pub mod app;
#[cfg(feature = "browser")]
pub mod browser;
pub mod diff;
pub mod dock;
pub mod events;
pub mod evidence;
pub mod fixture;
pub mod headless;
pub mod keymap;
pub mod logging;
pub mod model;
pub mod panes;
pub mod pty;
pub mod runtime_sink;
pub mod storage_bridge;
pub mod theme;
pub mod window;
