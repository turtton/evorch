//! ツールレイヤの実装クレート。
//!
//! LLM エージェントが呼び出す標準ツール（read / edit / grep / shell / git_diff）の
//! 定義、引数スキーマ、権限モデル、出力サニタイズを提供する。各ツールは
//! [`tool::Tool`] トレイトを実装し、引数のスキーマ検証と結果の正規化は wave 3 で
//! 追加される ToolExecutor が担う（ADR 0008）。

pub mod error;
pub mod result;
// wave 3 の ToolExecutor が使用する内部ヘルパ。lib ビルドでは現時点で未使用
// （テストからのみ参照）のため dead_code を許可する。wave 3 でこの属性を取り除くこと。
pub mod sanitize;
#[allow(dead_code)]
pub(crate) mod schema;
pub mod tool;
pub mod tools;

pub use error::ToolError;
pub use result::ToolResult;
pub use sanitize::escape_control_markers;
pub use tool::{Permissions, Tool};
pub use tools::{Edit, GitDiff, Grep, Read, Shell};
