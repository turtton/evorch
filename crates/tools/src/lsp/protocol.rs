use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::LspError;

pub(super) const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub(super) struct Message {
    pub jsonrpc: String,
    pub id: Option<serde_json::Value>,
    pub method: Option<String>,
    pub params: Option<serde_json::Value>,
    pub error: Option<serde_json::Value>,
}

pub(super) async fn read(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Message, LspError> {
    let mut length = None;
    let mut header_bytes = 0;
    loop {
        let mut line = Vec::new();
        let count = reader.take(8193).read_until(b'\n', &mut line).await?;
        header_bytes += count;
        if count == 0 {
            return Err(LspError::Closed);
        }
        if header_bytes > 8192 || !line.ends_with(b"\r\n") {
            return Err(LspError::Protocol);
        }
        if line == b"\r\n" {
            break;
        }
        let line = std::str::from_utf8(&line).map_err(|_| LspError::Protocol)?;
        let (name, value) = line.split_once(':').ok_or(LspError::Protocol)?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err(LspError::Protocol);
            }
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| LspError::Protocol)?,
            );
        }
    }
    let length = length
        .filter(|n| *n <= MAX_BYTES)
        .ok_or(LspError::Protocol)?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    let message: Message = serde_json::from_slice(&body).map_err(|_| LspError::Protocol)?;
    if message.jsonrpc != "2.0" {
        return Err(LspError::Protocol);
    }
    Ok(message)
}

pub(super) async fn write(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &serde_json::Value,
) -> Result<(), LspError> {
    let body = serde_json::to_vec(value).map_err(|_| LspError::Protocol)?;
    if body.len() > MAX_BYTES {
        return Err(LspError::Protocol);
    }
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub(super) struct Batch {
    pub uri: String,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Diagnostic {
    pub range: Range,
    pub severity: Option<Severity>,
    pub code: Option<Code>,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Deserialize)]
pub(super) struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub(super) enum Code {
    Number(i32),
    Text(String),
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(code) => write!(f, "{code}"),
            Self::Text(code) => f.write_str(code),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(try_from = "u8")]
pub(super) enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl TryFrom<u8> for Severity {
    type Error = &'static str;
    fn try_from(value: u8) -> Result<Self, &'static str> {
        match value {
            1 => Ok(Self::Error),
            2 => Ok(Self::Warning),
            3 => Ok(Self::Information),
            4 => Ok(Self::Hint),
            _ => Err("invalid LSP severity"),
        }
    }
}

impl Severity {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "information",
            Self::Hint => "hint",
        }
    }
}

impl Batch {
    pub fn render(&self, path: &std::path::Path) -> Result<String, LspError> {
        self.diagnostics
            .iter()
            .map(|diagnostic| {
                let start = &diagnostic.range.start;
                let end = &diagnostic.range.end;
                if (end.line, end.character) < (start.line, start.character) {
                    return Err(LspError::Protocol);
                }
                let code = diagnostic
                    .code
                    .as_ref()
                    .map(|code| format!(" [{}]", code.to_string().replace(['\r', '\n'], " ")))
                    .unwrap_or_default();
                Ok(format!(
                    "{}:{}:{} {}{code} {}",
                    path.display(),
                    u64::from(start.line) + 1,
                    u64::from(start.character) + 1,
                    diagnostic.severity.unwrap_or(Severity::Information).label(),
                    diagnostic.message.replace(['\r', '\n'], " ")
                ))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|lines| lines.join("\n"))
    }
}
