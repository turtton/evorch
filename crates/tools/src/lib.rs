//! ツールレイヤの実装クレート。
//!
//! LLM エージェントが呼び出す標準ツール（read / write / edit / apply_patch / grep / shell /
//! git_diff）の定義、引数スキーマ、権限モデル、出力サニタイズ、そして実行の窓口である
//! ToolExecutor を提供する。web_search / web_fetch は
//! [`executor::ToolExecutor::with_web_tools`] で追加登録する。
//! 各ツールは [`tool::Tool`] トレイトを実装し、引数の
//! スキーマ検証と結果の正規化（制御マーカのエスケープ）は [`executor::ToolExecutor`]
//! が担う（ADR 0008）。

pub mod error;
pub mod executor;
pub mod image;
pub mod lsp;
pub mod mcp;
pub mod network_guard;
pub mod origin;
pub mod output;
mod path_args;
pub mod post_edit;
pub mod result;
pub mod sanitize;
pub(crate) mod schema;
pub mod search;
mod shell_handles;
pub mod tool;
pub mod tools;

pub use error::ToolError;
pub use executor::{ToolExecutionContext, ToolExecutor};
pub use network_guard::{
    DnsResolver, GuardedResponse, MAX_REDIRECTS, MAX_RESPONSE_BYTES, NetworkGuard,
    NetworkGuardError,
};
pub use origin::{ContentOrigin, derive_content_origin};
pub use result::{CutOffTold, Progress, RuntimeOnly, ToolImage, ToolResult};
pub use sanitize::escape_control_markers;
pub use search::{
    ExaKeylessProvider, McpToolSuccess, McpTransport, NetworkGuardMcpTransport,
    NetworkGuardResponsesTransport, OpenAiResponsesProvider, OpenAiSearchCredential,
    ResponsesTransport, SearchError, SearchOptions, SearchProvider, SearchResults,
    TavilyKeylessProvider, count_search_results,
};
pub use shell_handles::{MAX_RESTORED_HANDLE_FLOOR, set_shell_job_handle_dir};
pub use tool::{BuiltinTool, Permissions, Tool, ToolExecutionMode};
pub use tools::run_js::{ChildCall, ChildOutcome, RunJsHost};
pub use tools::{
    ApplyPatch, BROWSER_TOOL_NAMES, BrowserTool, BrowserToolOptions, Edit, GitDiff, Grep,
    GuardPolicy, Read, RunJs, Shell, WebFetch, WebSearch, Write, browser_tools,
};
