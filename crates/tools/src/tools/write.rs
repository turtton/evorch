//! ファイルの新規作成と全文置換。部分置換には edit を使う。

use std::path::Path;

use super::edit::{required_str, write_atomically};
use super::file_diff;
use crate::{Permissions, Tool, ToolError, ToolExecutionMode, ToolResult};

/// UTF-8 ファイルを原子的に作成・上書きするツール。
#[derive(Debug, Clone, Copy)]
pub struct Write;

#[async_trait::async_trait]
impl Tool for Write {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &str {
        "Create a UTF-8 file or replace its entire contents atomically using path and content. The parent directory must already exist. Use edit for targeted replacements in an existing file."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_write()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Exclusive
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let path = required_str(&args, "path")?;
        let content = required_str(&args, "content")?;
        let before = file_diff::read_previous(Path::new(path));
        write_atomically(Path::new(path), content)?;
        let output = match before {
            Ok(previous) => file_diff::changed_file(path, previous.as_deref(), content),
            Err(reason) => format!("Wrote {path}; diff unavailable: {reason}"),
        };
        Ok(ToolResult::success(output))
    }
}
