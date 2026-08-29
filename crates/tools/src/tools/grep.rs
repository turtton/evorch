//! grep ツールのスタブ実装。
//!
//! 引数スキーマと権限は最終契約。実行ボディは wave 2 で実装する。

use crate::error::ToolError;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool};

/// 正規表現でファイル内容を検索するツール。
#[derive(Debug, Clone, Copy)]
pub struct Grep;

#[async_trait::async_trait]
impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" }
            },
            "required": ["pattern", "path"],
            "additionalProperties": false
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult, ToolError> {
        todo!("wave 2 implements the body")
    }
}
