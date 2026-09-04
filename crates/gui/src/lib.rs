//! ADR 0007 に基づく egui + egui_dock ベースの workbench GUI。
//!
//! 層構造: `model` (egui 非依存 view model) -> `pty` / `events` (adapter) -> panes/app (Wave 3)。

pub mod app;
pub mod diff;
pub mod dock;
pub mod events;
pub mod headless;
pub mod keymap;
pub mod model;
pub mod panes;
pub mod pty;
pub mod runtime_sink;
