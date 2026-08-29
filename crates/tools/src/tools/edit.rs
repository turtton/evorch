//! edit ツールのスタブ実装。
//!
//! 引数スキーマと権限は最終契約。実行ボディは wave 2 で実装する。

use crate::error::ToolError;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool};

/// ファイル内の文字列を置換するツール。
#[derive(Debug, Clone, Copy)]
pub struct Edit;

#[async_trait::async_trait]
impl Tool for Edit {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" }
            },
            "required": ["path", "new_string"],
            "additionalProperties": false
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_write()
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult, ToolError> {
        todo!("wave 2 implements the body")
    }
}
