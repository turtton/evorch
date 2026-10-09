//! Terminal ペインの端末エミュレーション。
//!
//! `emulator` が VT シーケンスを解釈し、`session` がプロジェクトごとのシェルを管理します。
//! `input` は egui の入力を xterm 互換のバイト列へ変換し、`colors` はセル色を解決します。

pub mod colors;
pub mod emulator;
pub mod input;
pub mod session;

pub use emulator::{GridSize, TerminalEmulator, ViewportPoint};
pub use session::{TerminalKey, TerminalSession, TerminalSessions, TerminalStatus};
