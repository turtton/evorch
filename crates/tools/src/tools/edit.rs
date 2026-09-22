//! edit ツールの実装。
//!
//! 空でない `old_string` の最初の一致箇所を置換する。
//! 書き込みは必ず同一親ディレクトリ上の一時ファイル経由で行い、`persist` に
//! よる原子的リネームで反映する。ディスクへの書き込みはバイト一致とし、制御
//! マーカーのエスケープは行わない（ADR 0008。エスケープは wave 3 の
//! ToolExecutor が結果正規化で担う）。

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::ToolError;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool, ToolExecutionMode};

/// ファイル内の文字列を置換するツール。
#[derive(Debug, Clone, Copy)]
pub struct Edit;

#[async_trait::async_trait]
impl Tool for Edit {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &str {
        "Replace the first exact, non-empty old_string match in an existing file with new_string. Read the file first. To create or replace a whole file, use write(path, content)."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string", "minLength": 1 },
                "new_string": { "type": "string" }
            },
            "required": ["path", "old_string", "new_string"],
            "additionalProperties": false
        })
    }

    fn permissions(&self) -> Permissions {
        Permissions::read_write()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Exclusive
    }

    /// 既存ファイルの `old_string` の最初の一致箇所を置換する。
    ///
    /// # Errors
    /// 引数が不正なら [`ToolError::InvalidArgs`]、`old_string` 指定時にファイルが
    /// 存在しなければ [`ToolError::PathNotFound`]、置換対象が見つからなければ
    /// [`ToolError::EditTargetNotFound`]、入出力に失敗すれば [`ToolError::Io`] を返す。
    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let path_text = required_str(&args, "path")?;
        let new_string = required_str(&args, "new_string")?;
        let old_string = required_str(&args, "old_string")?;
        if old_string.is_empty() {
            return Err(ToolError::InvalidArgs {
                detail: "old_string must be non-empty; use write(path, content) to create or replace a file".into(),
            });
        }
        let path = Path::new(path_text);
        let current = std::fs::read_to_string(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ToolError::PathNotFound {
                    path: path_text.to_string(),
                }
            } else {
                io_error("ファイルの読み込みに失敗しました", error)
            }
        })?;
        let next_content = replace_first(&current, old_string, new_string).ok_or_else(|| {
            ToolError::EditTargetNotFound {
                path: path_text.to_string(),
            }
        })?;
        write_atomically(path, &next_content)?;
        Ok(ToolResult::success(format!("edited {path_text}")))
    }
}

/// `args` から必須の文字列プロパティを取り出す。
pub(super) fn required_str<'a>(
    args: &'a serde_json::Value,
    key: &str,
) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ToolError::InvalidArgs {
            detail: format!("文字列のプロパティ {key} が必要です"),
        })
}

/// `haystack` 中の最初の `needle` を `replacement` へ置換する。見つからなければ `None`。
fn replace_first(haystack: &str, needle: &str, replacement: &str) -> Option<String> {
    let index = haystack.find(needle)?;
    let mut next = String::with_capacity(haystack.len() + replacement.len() - needle.len());
    next.push_str(&haystack[..index]);
    next.push_str(replacement);
    next.push_str(&haystack[index + needle.len()..]);
    Some(next)
}

/// `content` を `path` へ原子的に書き込む。
///
/// 同一親ディレクトリに一時ファイルを作成して書き出した後、`persist` で `path`
/// へリネームする。失敗時は一時ファイルが drop 時に削除される。
pub(super) fn write_atomically(path: &Path, content: &str) -> Result<(), ToolError> {
    let parent_dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let mut temp = tempfile::NamedTempFile::new_in(&parent_dir)
        .map_err(|error| io_error("一時ファイルの作成に失敗しました", error))?;
    // 原子的な置換で既存ファイルの実行ビット等を失わない。
    match std::fs::metadata(path) {
        Ok(metadata) => temp
            .as_file()
            .set_permissions(metadata.permissions())
            .map_err(|error| io_error("ファイル権限の維持に失敗しました", error))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error("ファイル情報の読み込みに失敗しました", error)),
    }
    temp.write_all(content.as_bytes())
        .map_err(|error| io_error("一時ファイルへの書き込みに失敗しました", error))?;
    temp.flush()
        .map_err(|error| io_error("一時ファイルのフラッシュに失敗しました", error))?;
    temp.persist(path)
        .map_err(|error| io_error("一時ファイルの確定に失敗しました", error.error))?;
    Ok(())
}

/// 入出力の失敗を段階名付きの [`ToolError::Io`] へ変換する。
fn io_error(stage: &str, error: std::io::Error) -> ToolError {
    ToolError::Io {
        detail: format!("{stage}: {error}"),
    }
}
