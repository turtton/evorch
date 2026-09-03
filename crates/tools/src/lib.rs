//! ツールレイヤの実装クレート。
//!
//! LLM エージェントが呼び出す標準ツール（read / edit / grep / shell / git_diff）の
//! 定義、引数スキーマ、権限モデル、出力サニタイズ、そして実行の窓口である
//! ToolExecutor を提供する。各ツールは [`tool::Tool`] トレイトを実装し、引数の
//! スキーマ検証と結果の正規化（制御マーカのエスケープ）は [`executor::ToolExecutor`]
//! が担う（ADR 0008）。

pub mod error;
pub mod executor;
pub mod network_guard;
pub mod origin;
pub mod result;
pub mod sanitize;
pub(crate) mod schema;
pub mod search;
pub mod tool;
pub mod tools;

pub use error::ToolError;
pub use executor::{ToolExecutionContext, ToolExecutor};
pub use network_guard::{
    DnsResolver, GuardedResponse, MAX_REDIRECTS, MAX_RESPONSE_BYTES, NetworkGuard,
    NetworkGuardError,
};
pub use origin::{ContentOrigin, derive_content_origin};
pub use result::ToolResult;
pub use sanitize::escape_control_markers;
pub use search::{
    ExaKeylessProvider, McpToolSuccess, McpTransport, NetworkGuardMcpTransport, SearchError,
    SearchOptions, SearchProvider, SearchResults, TavilyKeylessProvider, count_search_results,
};
pub use tool::{Permissions, Tool};
pub use tools::{Edit, GitDiff, Grep, Read, Shell, WebFetch, WebSearch};
