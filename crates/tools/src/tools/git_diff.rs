//! git_diff ツールのスタブ実装。
//!
//! 引数スキーマと権限は最終契約。実行ボディは wave 2 で実装する。

use crate::error::ToolError;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool};

/// Git の差分を取得するツール。
#[derive(Debug, Clone, Copy)]
pub struct GitDiff;

#[async_trait::async_trait]
impl Tool for GitDiff {
    fn name(&self) -> &'static str {
        "git_diff"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cwd": { "type": "string", "default": "." },
                "path": { "type": "string" }
            },
            "required": [],
            "additionalProperties": false
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions {
            fs_read: true,
            fs_write: false,
            process_spawn: true,
        }
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult, ToolError> {
        todo!("wave 2 implements the body")
    }
}
