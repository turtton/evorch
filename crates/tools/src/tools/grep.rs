//! grep ツールの実装。
//!
//! 引数スキーマと権限は最終契約。`execute` は正規表現に一致した行を
//! `path:行番号:行` 形式で返す。ディレクトリ指定時は再帰的に走査し、
//! `.git` と読み取り不能・非 UTF-8 のファイルは黙ってスキップする。

use std::io::ErrorKind;
use std::path::Path;

use regex::Regex;

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

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        // スキーマ検証は ToolExecutor（wave 3）が担うため、ここでは生の引数から
        // 必要フィールドを取り出す。欠落時の InvalidArgs は直接呼び出しの防御。
        let pattern = string_arg(&args, "pattern")?;
        let path = string_arg(&args, "path")?;

        let regex = Regex::new(pattern).map_err(|error| ToolError::InvalidPattern {
            detail: error.to_string(),
        })?;

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

        let mut matches = Vec::new();
        if metadata.is_dir() {
            walk_dir(Path::new(path), &regex, &mut matches);
        } else {
            // 明示的に指定された 1 ファイルの失敗はエラーにする（黙ってスキップするのは再帰時のみ）。
            let content = std::fs::read_to_string(path).map_err(|error| ToolError::Io {
                detail: format!("{path} の読み取りに失敗しました: {error}"),
            })?;
            matches.extend(matching_lines(Path::new(path), &content, &regex));
        }

        Ok(ToolResult::success(matches.join("\n")))
    }
}

/// 引数オブジェクトから文字列型のフィールドを取り出す。
///
/// スキーマ適合は ToolExecutor（wave 3）が保証するため通常は失敗しない。直接
/// 呼び出しの場合の防御として `InvalidArgs` を返す。
fn string_arg<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ToolError::InvalidArgs {
            detail: format!("引数 {key} は文字列である必要があります"),
        })
}

/// 1 ファイル分の一致行を `path:行番号:行` 形式で生成する。
fn matching_lines(path: &Path, content: &str, regex: &Regex) -> Vec<String> {
    let path = path.display();
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| regex.is_match(line))
        .map(|(index, line)| {
            let line_no = index + 1;
            format!("{path}:{line_no}:{line}")
        })
        .collect()
}

/// ディレクトリを再帰的に走査し、一致行を `matches` へ収集する。
///
/// 仕様どおり、読み取り不能・非 UTF-8 のファイルとシンボリックリンクは黙って
/// スキップし、`.git` エントリは必ず無視する。ディレクトリエントリはファイル名
/// 順にソートして決定論的な出力順を保証する。
fn walk_dir(dir: &Path, regex: &Regex, matches: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() || entry.file_name() == ".git" {
            continue;
        }
        let entry_path = entry.path();
        if file_type.is_dir() {
            walk_dir(&entry_path, regex, matches);
        } else if file_type.is_file()
            && let Ok(content) = std::fs::read_to_string(&entry_path)
        {
            matches.extend(matching_lines(&entry_path, &content, regex));
        }
    }
}
