//! grep ツールの実装。
//!
//! rg の結果をファイル順の `path:行番号:行` で返す。
//! 注記を含めて最大 200 行 / 8 KiB。検索は 60 秒で打ち切る。

use std::io::ErrorKind;
use std::process::Stdio;
use std::time::Duration;

use regex::Regex;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::error::ToolError;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool, ToolExecutionMode};

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

    fn description(&self) -> &str {
        "Search file contents at a path with a regular expression and return matching lines."
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

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Shared
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        // スキーマ検証は ToolExecutor（wave 3）が担うため、ここでは生の引数から
        // 必要フィールドを取り出す。欠落時の InvalidArgs は直接呼び出しの防御。
        let pattern = string_arg(&args, "pattern")?;
        let path = string_arg(&args, "path")?;

        let regex = Regex::new(pattern).map_err(|error| ToolError::InvalidPattern {
            detail: error.to_string(),
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
                return search_file(path, &regex).await.map(ToolResult::success);
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
            let (output, capped) = collect_output(stdout).await.map_err(io_error)?;
            if capped {
                child.kill().await.map_err(io_error)?;
                return Ok(ToolResult::success(output));
            }
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
            "--hidden",
            "--glob",
            "!.git",
            "--null",
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

async fn collect_output(mut reader: impl AsyncRead + Unpin) -> std::io::Result<(String, bool)> {
    let mut output = Matches::default();
    let mut line = Vec::with_capacity(MAX_BYTES);
    let mut buffer = [0; MAX_BYTES];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        for &byte in &buffer[..count] {
            if byte == b'\n' {
                if let Ok(text) = std::str::from_utf8(&line)
                    && let Some((path, hit)) = text.split_once('\0')
                    && !output.push(path, hit.trim_end_matches('\r'))
                {
                    return Ok((output.finish(true), true));
                }
                line.clear();
            } else if line.len() < MAX_BYTES {
                line.push(byte);
            } else {
                output.omitted += 1;
                return Ok((output.finish(true), true));
            }
        }
    }
    Ok((output.finish(false), false))
}

async fn search_file(path: &str, regex: &Regex) -> Result<String, ToolError> {
    let file = tokio::fs::File::open(path).await.map_err(io_error)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut number = 0;
    let mut output = Matches::default();
    loop {
        line.clear();
        if reader.read_line(&mut line).await.map_err(io_error)? == 0 {
            return Ok(output.finish(false));
        }
        number += 1;
        let text = line.strip_suffix('\n').unwrap_or(&line);
        if regex.is_match(text) {
            output.push(path, &format!("{number}:{}", text.trim_end_matches('\r')));
        }
    }
}

#[derive(Default)]
struct Matches {
    lines: Vec<(String, String)>,
    bytes: usize,
    omitted: usize,
}

impl Matches {
    fn push(&mut self, path: &str, hit: &str) -> bool {
        let bytes = path.len() + 1 + hit.len() + 1;
        if self.omitted > 0
            || self.lines.len() == MAX_LINES
            || self.bytes + bytes > MAX_BYTES - SUMMARY_RESERVE
        {
            self.omitted += 1;
            return false;
        }
        self.bytes += bytes;
        self.lines.push((path.to_owned(), hit.to_owned()));
        true
    }

    fn finish(mut self, capped: bool) -> String {
        if self.omitted > 0 && self.lines.len() == MAX_LINES {
            self.lines.pop();
            self.omitted += 1;
        }
        self.lines.sort_by(|a, b| a.0.cmp(&b.0));
        let mut output = self
            .lines
            .iter()
            .map(|(path, hit)| format!("{path}:{hit}"))
            .collect::<Vec<_>>()
            .join("\n");
        if self.omitted > 0 {
            if !output.is_empty() {
                output.push('\n');
            }
            let qualifier = if capped { "at least " } else { "" };
            output.push_str(&format!(
                "[truncated: {qualifier}{} more lines]",
                self.omitted
            ));
        }
        output
    }
}

fn io_error(error: std::io::Error) -> ToolError {
    ToolError::Io {
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    // Given: enough hits and no EOF / When: collecting / Then: no further read is needed.
    #[tokio::test]
    async fn collection_stops_before_reading_to_eof() {
        struct FailAfterHits(std::io::Cursor<Vec<u8>>);
        impl tokio::io::AsyncRead for FailAfterHits {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                let mut bytes = [0; 8192];
                let count =
                    std::io::Read::read(&mut self.0, &mut bytes[..buf.remaining().min(8192)])?;
                assert!(count > 0, "collector read beyond its cap");
                buf.put_slice(&bytes[..count]);
                std::task::Poll::Ready(Ok(()))
            }
        }
        let reader = FailAfterHits(std::io::Cursor::new(b"a\x001:hit\n".repeat(1000)));
        super::collect_output(reader)
            .await
            .expect("bounded collection");
    }
}
