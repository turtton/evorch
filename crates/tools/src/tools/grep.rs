//! grep ツールの実装。
//!
//! rg の結果をファイル順の `path:行番号:行` で返す。
//! 注記を含めて最大 200 行 / 8 KiB。検索は 60 秒で打ち切る。

use std::io::ErrorKind;
use std::process::Stdio;
use std::time::Duration;

use regex::Regex;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::error::ToolError;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool};

/// 正規表現でファイル内容を検索するツール。
#[derive(Debug, Clone, Copy)]
pub struct Grep;

const MAX_LINES: usize = 200;
const MAX_BYTES: usize = 8192;
const SUMMARY_RESERVE: usize = 64;

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

        Regex::new(pattern).map_err(|_| ToolError::InvalidPattern {
            detail: "invalid regular expression".to_string(),
        })?;

        let metadata = tokio::fs::metadata(path).await.map_err(|error| {
            if error.kind() == ErrorKind::NotFound {
                ToolError::PathNotFound {
                    path: path.chars().take(512).collect(),
                }
            } else {
                ToolError::Io {
                    detail: error.to_string(),
                }
            }
        })?;

        let search = async {
            if metadata.is_file() {
                validate_utf8_file(path).await?;
            }
            let mut child =
                grep_command(pattern, path)
                    .spawn()
                    .map_err(|error| ToolError::SpawnFailed {
                        command: "rg (install ripgrep and make it available on PATH)".to_string(),
                        detail: error.to_string(),
                    })?;
            let stdout = child.stdout.take().ok_or_else(|| ToolError::Io {
                detail: "rg stdout is unavailable".to_string(),
            })?;
            let output = collect_output(stdout).await.map_err(io_error)?;
            let status = child.wait().await.map_err(io_error)?;
            match status.code() {
                Some(0 | 1) => Ok(ToolResult::success(output)),
                Some(2) if metadata.is_dir() => Ok(ToolResult::success(output)),
                _ => Ok(ToolResult::error(
                    "rg search failed (check path and permissions)",
                )),
            }
        };
        tokio::time::timeout(Duration::from_secs(60), search)
            .await
            .map_err(|_| ToolError::Timeout { timeout_ms: 60_000 })?
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

fn grep_command(pattern: &str, path: &str) -> Command {
    let mut command = Command::new("rg");
    command
        .args([
            "--no-config",
            "--no-ignore",
            "--hidden",
            "--glob",
            "!.git",
            "--sort",
            "path",
            "--color",
            "never",
            "--no-heading",
            "--with-filename",
            "--line-number",
            "--no-messages",
            "--encoding",
            "none",
            "--regexp",
            pattern,
            "--",
            path,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

async fn collect_output(mut reader: impl AsyncRead + Unpin) -> std::io::Result<String> {
    let mut output = String::with_capacity(MAX_BYTES);
    let mut line = Vec::with_capacity(MAX_BYTES);
    let mut buffer = [0; MAX_BYTES];
    let mut total = 0_usize;
    let mut shown = 0;
    let mut oversized = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        for &byte in &buffer[..count] {
            if byte == b'\n' {
                total = total.saturating_add(1);
                if total == shown + 1
                    && shown < MAX_LINES
                    && !oversized
                    && let Ok(text) = std::str::from_utf8(&line)
                    && output.len() + text.len() < MAX_BYTES - SUMMARY_RESERVE
                {
                    output.push_str(text.trim_end_matches('\r'));
                    output.push('\n');
                    shown += 1;
                }
                line.clear();
                oversized = false;
            } else if line.len() < MAX_BYTES {
                line.push(byte);
            } else {
                oversized = true;
            }
        }
    }
    if !line.is_empty() || oversized {
        total = total.saturating_add(1);
    }
    if total > shown {
        if shown == MAX_LINES {
            output.pop();
            output.truncate(output.rfind('\n').map_or(0, |index| index + 1));
            shown -= 1;
        }
        output.push_str(&format!("[truncated: {} more lines]", total - shown));
    } else {
        output.pop();
    }
    Ok(output)
}

async fn validate_utf8_file(path: &str) -> Result<(), ToolError> {
    let mut file = tokio::fs::File::open(path).await.map_err(io_error)?;
    let mut buffer = [0; MAX_BYTES];
    let mut pending = 0;
    loop {
        let count = file.read(&mut buffer[pending..]).await.map_err(io_error)?;
        let bytes = &buffer[..pending + count];
        match std::str::from_utf8(bytes) {
            Ok(_) if count == 0 => return Ok(()),
            Ok(_) => pending = 0,
            Err(error) if error.error_len().is_none() && count > 0 => {
                let start = error.valid_up_to();
                pending = bytes.len() - start;
                buffer.copy_within(start..start + pending, 0);
            }
            Err(_) => {
                return Err(ToolError::Io {
                    detail: "file is not UTF-8".to_string(),
                });
            }
        }
    }
}

fn io_error(error: std::io::Error) -> ToolError {
    ToolError::Io {
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    // Given: a search / When: constructing its command / Then: rg is the only backend.
    #[test]
    fn grep_uses_rg_backend() {
        let command = super::grep_command("needle", ".");
        assert_eq!(command.as_std().get_program(), "rg");
    }
}
