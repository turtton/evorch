//! How the benchmark tool runs tools. `evorch run` no longer runs an agent in
//! this process: it is a client of the core (see [`crate::cli::run`]).

/// tool 実行の sandbox 方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxChoice {
    /// production 構成 (`production_executor`、fail-closed)。
    Production,
    /// 隔離なしの実行 (非 production / テスト専用の明示的 opt-out)。
    DirectUnchecked,
}
