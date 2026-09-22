//! 行範囲を指定できる UTF-8 ファイル読み取り。本文と読み取りバッファに上限を設ける。

use std::io::{BufRead, BufReader, ErrorKind, Read as IoRead};

use crate::error::ToolError;
use crate::result::ToolResult;
use crate::tool::{Permissions, Tool, ToolExecutionMode};

pub const MAX_READ_LINES: u64 = 300;
pub const MAX_READ_BYTES: usize = 16 * 1024;

/// ファイルを読み取るツール。
#[derive(Debug, Clone, Copy)]
pub struct Read;

#[async_trait::async_trait]
impl Tool for Read {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a bounded range of a UTF-8 file. Prefer this over shell cat. offset is a 1-based line number; limit defaults to 300 lines (maximum 300). Returns at most 16 KiB of file text and continuation arguments when truncated. byte_offset continues within an oversized line. Use the returned offset and byte_offset together to read the next range."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Target file path (preferred). Aliases: file, file_path, filename, target."
                },
                "file": { "type": "string" },
                "file_path": { "type": "string" },
                "filename": { "type": "string" },
                "target": { "type": "string" },
                "offset": { "type": "integer", "minimum": 1, "description": "First line to read (1-based; default 1)." },
                "limit": { "type": "integer", "minimum": 1, "description": "Maximum lines to return (default 300; larger values are capped at 300)." },
                "byte_offset": { "type": "integer", "minimum": 0, "description": "Bytes to skip within the selected line (default 0). Use the continuation value returned for a long line; must be a UTF-8 boundary." }
            },
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
        // キーの存在はここで検証し、スキーマの一般エラーではなく修正可能なヒントを返す。
        let Some(path) = ["path", "file", "file_path", "filename", "target"]
            .iter()
            .find_map(|key| args.get(key))
            .and_then(serde_json::Value::as_str)
        else {
            return Err(ToolError::InvalidArgs {
                detail: "引数 path は文字列である必要があります; expected keys: path (aliases: file, file_path); filename and target are also accepted".to_string(),
            });
        };
        let offset = unsigned_arg(&args, "offset", 1, 1)?;
        let limit = unsigned_arg(&args, "limit", MAX_READ_LINES, 1)?.min(MAX_READ_LINES);
        let byte_offset = unsigned_arg(&args, "byte_offset", 0, 0)?;
        let path = path.to_owned();
        // 大きなファイルの offset 探索は async executor を塞がない。
        tokio::task::spawn_blocking(move || read_range(&path, offset, limit, byte_offset))
            .await
            .map_err(|error| ToolError::Io {
                detail: format!("read task failed: {error}"),
            })?
    }
}

fn unsigned_arg(
    args: &serde_json::Value,
    key: &str,
    default: u64,
    minimum: u64,
) -> Result<u64, ToolError> {
    match args.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|value| *value >= minimum)
            .ok_or_else(|| ToolError::InvalidArgs {
                detail: format!("{key} must be an integer >= {minimum}"),
            }),
    }
}

fn read_range(
    path: &str,
    offset: u64,
    limit: u64,
    byte_offset: u64,
) -> Result<ToolResult, ToolError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            ToolError::PathNotFound {
                path: path.to_owned(),
            }
        } else {
            io_error(path, error)
        }
    })?;
    if !metadata.is_file() {
        return Err(ToolError::NotAFile {
            path: path.to_owned(),
        });
    }
    let file = std::fs::File::open(path).map_err(|error| io_error(path, error))?;
    let mut reader = BufReader::new(file);
    // read_until / lines は巨大な 1 行を丸ごと確保するため使わない。
    for _ in 1..offset {
        if !skip_line(&mut reader).map_err(|error| io_error(path, error))? {
            break;
        }
    }
    skip_line_bytes(&mut reader, byte_offset).map_err(|error| io_error(path, error))?;

    // UTF-8 の最大 4 バイト文字と、続きの存在判定に必要な先読みだけを確保する。
    let mut bytes = Vec::with_capacity(MAX_READ_BYTES + 4);
    reader
        .take((MAX_READ_BYTES + 4) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(path, error))?;
    let line_end = bytes
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .nth((limit - 1) as usize)
        .map(|(index, _)| index + 1)
        .unwrap_or(bytes.len());
    let mut end = line_end.min(MAX_READ_BYTES);
    match std::str::from_utf8(&bytes[..end]) {
        Ok(_) => {}
        Err(error) if error.error_len().is_none() && end < bytes.len() => {
            end = error.valid_up_to();
        }
        Err(error) => {
            return Err(io_error(
                path,
                std::io::Error::new(ErrorKind::InvalidData, error),
            ));
        }
    }
    let truncated = end < bytes.len();
    let selected = &bytes[..end];
    let newline_count = selected.iter().filter(|byte| **byte == b'\n').count() as u64;
    let next_offset = offset.saturating_add(newline_count);
    let next_byte_offset = selected
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| (end - index - 1) as u64)
        .unwrap_or_else(|| byte_offset.saturating_add(end as u64));
    let mut content = String::from_utf8(selected.to_vec())
        .map_err(|error| io_error(path, std::io::Error::new(ErrorKind::InvalidData, error)))?;
    if truncated {
        let next = serde_json::json!({"path": path, "offset": next_offset, "byte_offset": next_byte_offset, "limit": limit});
        content.push_str(&format!("\n\n[Output truncated (range limit: {limit} lines / {MAX_READ_BYTES} bytes). Continue with read({next}).]"));
    }
    Ok(ToolResult::success(content).with_detail(serde_json::json!({
        "path": path,
        "offset": offset,
        "byte_offset": byte_offset,
        "bytes_returned": end,
        "truncated": truncated,
        "next_offset": truncated.then_some(next_offset),
        "next_byte_offset": truncated.then_some(next_byte_offset)
    })))
}

/// 改行までを定数メモリで捨てる。EOF なら false。
fn skip_line(reader: &mut impl BufRead) -> std::io::Result<bool> {
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(false);
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map(|index| index + 1).unwrap_or(buffer.len());
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

fn skip_line_bytes(reader: &mut impl BufRead, mut remaining: u64) -> std::io::Result<()> {
    while remaining > 0 {
        let buffer = reader.fill_buf()?;
        let consumed = remaining.min(buffer.len() as u64) as usize;
        if consumed == 0 || buffer[..consumed].contains(&b'\n') {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                "byte_offset extends beyond the selected line",
            ));
        }
        reader.consume(consumed);
        remaining -= consumed as u64;
    }
    Ok(())
}

fn io_error(path: &str, error: std::io::Error) -> ToolError {
    ToolError::Io {
        detail: format!("{path} の読み取りに失敗しました: {error}"),
    }
}
