//! shell ツールのスタブ実装。
//!
//! 引数スキーマと権限は最終契約。実行ボディは wave 2 で実装する。

use crate::error::ToolError;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool};

/// コマンドを実行するツール。
#[derive(Debug, Clone, Copy)]
pub struct Shell;

#[async_trait::async_trait]
impl Tool for Shell {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "args": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "interactive": { "type": "boolean", "default": false },
                "cwd": { "type": "string" },
                "timeout_ms": { "type": "integer", "minimum": 1 }
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::process()
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult, ToolError> {
        todo!("wave 2 implements the body")
    }
}
