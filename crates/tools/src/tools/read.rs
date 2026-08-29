//! read ツールのスタブ実装。
//!
//! 引数スキーマと権限は最終契約。実行ボディは wave 2 で実装する。

use crate::error::ToolError;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool};

/// ファイルを読み取るツール。
#[derive(Debug, Clone, Copy)]
pub struct Read;

#[async_trait::async_trait]
impl Tool for Read {
    fn name(&self) -> &'static str {
        "read"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            },
            "required": ["path"],
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
