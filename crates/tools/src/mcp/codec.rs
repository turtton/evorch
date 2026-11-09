//! Strict external MCP codec. The historical search wire helpers are independent.
use super::{
    McpErrorKind,
    transport::api::{RpcError, RpcFrame, RpcId, RpcNotification, RpcRequest},
    types::{MAX_FRAME_BYTES, MAX_FRAMES_PER_EXCHANGE, MAX_REQUEST_BYTES},
};
use serde_json::Value;

pub fn parse_frame(bytes: &[u8]) -> Result<RpcFrame, McpErrorKind> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(McpErrorKind::LimitExceeded);
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|_| McpErrorKind::Protocol)?;
    let object = value.as_object().ok_or(McpErrorKind::Protocol)?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(McpErrorKind::Protocol);
    }
    let has_result = object.contains_key("result");
    let has_error = object.contains_key("error");
    let has_method = object.contains_key("method");
    if usize::from(has_result) + usize::from(has_error) + usize::from(has_method) != 1 {
        return Err(McpErrorKind::Protocol);
    }
    let id = match object.get("id") {
        None | Some(Value::Null) if has_error => None,
        None => None,
        Some(value) => Some(
            serde_json::from_value::<RpcId>(value.clone()).map_err(|_| McpErrorKind::Protocol)?,
        ),
    };
    if has_result {
        let result = object["result"].clone();
        if !result.is_object() {
            return Err(McpErrorKind::Protocol);
        }
        return Ok(RpcFrame::Result {
            jsonrpc: "2.0".into(),
            id: id.ok_or(McpErrorKind::Protocol)?,
            result,
        });
    }
    if has_error {
        let error: RpcError =
            serde_json::from_value(object["error"].clone()).map_err(|_| McpErrorKind::Protocol)?;
        return Ok(RpcFrame::Error {
            jsonrpc: "2.0".into(),
            id,
            error,
        });
    }
    let method = object["method"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(McpErrorKind::Protocol)?
        .to_owned();
    let params = object.get("params").cloned();
    if params.as_ref().is_some_and(|p| !p.is_object()) {
        return Err(McpErrorKind::Protocol);
    }
    Ok(match id {
        Some(id) => RpcFrame::Request(RpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method,
            params: params.unwrap_or_else(|| serde_json::json!({})),
        }),
        None => RpcFrame::Notification(RpcNotification {
            jsonrpc: "2.0".into(),
            method,
            params,
        }),
    })
}

pub fn matches_response(frame: &RpcFrame, id: &RpcId) -> bool {
    match frame {
        RpcFrame::Result { id: actual, .. } => actual == id,
        RpcFrame::Error {
            id: Some(actual), ..
        } => actual == id,
        _ => false,
    }
}

/// Byte-oriented line assembly handles UTF-8 and CRLF split at any chunk boundary.
/// Both aggregate bytes and individual events are bounded, including comments.
#[derive(Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    data: Vec<u8>,
    skip_lf: bool,
    started: bool,
    total: usize,
    frames: usize,
}
impl SseDecoder {
    pub fn received_bytes(&self) -> usize {
        self.total
    }
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<RpcFrame>, McpErrorKind> {
        self.total = self
            .total
            .checked_add(chunk.len())
            .ok_or(McpErrorKind::LimitExceeded)?;
        if self.total > MAX_REQUEST_BYTES {
            return Err(McpErrorKind::LimitExceeded);
        }
        let mut out = Vec::new();
        for &byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                if byte == b'\r' {
                    self.skip_lf = true;
                }
                if !self.started {
                    self.started = true;
                    if self.line.starts_with(&[0xef, 0xbb, 0xbf]) {
                        self.line.drain(..3);
                    }
                }
                if self.line.is_empty() {
                    if !self.data.is_empty() {
                        self.data.pop();
                        out.push(parse_frame(&self.data)?);
                        self.data.clear();
                        self.frames += 1;
                        if self.frames > MAX_FRAMES_PER_EXCHANGE {
                            return Err(McpErrorKind::LimitExceeded);
                        }
                    }
                } else {
                    // All SSE fields must be valid UTF-8, not just data.
                    std::str::from_utf8(&self.line).map_err(|_| McpErrorKind::Protocol)?;
                    if self.line == b"data" || self.line.starts_with(b"data:") {
                        let mut value = if self.line == b"data" {
                            &[][..]
                        } else {
                            &self.line[5..]
                        };
                        if value.first() == Some(&b' ') {
                            value = &value[1..];
                        }
                        self.data.extend_from_slice(value);
                        self.data.push(b'\n');
                        if self.data.len() > MAX_FRAME_BYTES {
                            return Err(McpErrorKind::LimitExceeded);
                        }
                    }
                    self.line.clear();
                }
            } else {
                self.line.push(byte);
                if self.line.len() > MAX_FRAME_BYTES {
                    return Err(McpErrorKind::LimitExceeded);
                }
            }
        }
        Ok(out)
    }
}
