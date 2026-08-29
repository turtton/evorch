//! read ツールの実装。
//!
//! 引数スキーマと権限は最終契約。`execute` は指定パスのファイル内容を
//! 行番号等の装飾なしで逐語的に返す。

use std::io::ErrorKind;

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

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        // スキーマ検証は ToolExecutor（wave 3）が担うため、ここでは生の引数から
        // 必要フィールドを取り出す。欠落時の InvalidArgs は直接呼び出しの防御。
        let Some(path) = args.get("path").and_then(serde_json::Value::as_str) else {
            return Err(ToolError::InvalidArgs {
                detail: "引数 path は文字列である必要があります".to_string(),
            });
        };

        let metadata = std::fs::metadata(path).map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                ToolError::PathNotFound {
                    path: path.to_string(),
                }
            } else {
                ToolError::Io {
                    detail: format!("{path}: {error}"),
                }
            }
        })?;
        if !metadata.is_file() {
            return Err(ToolError::NotAFile {
                path: path.to_string(),
            });
        }

        // 非 UTF-8 の内容は read_to_string が InvalidData で失敗するため Io に写像される。
        let content = std::fs::read_to_string(path).map_err(|error| ToolError::Io {
            detail: format!("{path} の読み取りに失敗しました: {error}"),
        })?;

        Ok(ToolResult::success(content))
    }
}
