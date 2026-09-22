//! run ごとのツール実行予算設定。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// ツール実行と停滞検出の上限。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BudgetConfig {
    /// 1 run あたりのツール実行回数の上限 (既定: 400)。
    pub max_tool_calls: u32,
    /// Cumulative input (including cached input) plus output tokens per run.
    pub max_tokens: u64,
    /// Maximum elapsed run time in seconds.
    pub max_elapsed_secs: u64,
    /// 成功した非メタツール結果のないラウンドの許容回数 (既定: 100)。
    pub max_no_progress_rounds: u32,
    /// 同じファイルの初回以降の再読込の許容回数 (既定: 20)。
    pub max_file_rereads: u32,
    /// 同一の名前・引数の連続呼び出しがこの回数に達すると停止 (既定: 5)。
    /// ID は比較対象外。0 でも最初の呼び出しで停止する。
    pub max_identical_tool_call_repeats: u32,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: 400,
            max_tokens: 2_000_000,
            max_elapsed_secs: 7_200,
            max_no_progress_rounds: 100,
            max_file_rereads: 20,
            max_identical_tool_call_repeats: 5,
        }
    }
}
