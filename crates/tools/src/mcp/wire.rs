use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub(crate) enum WireError {
    #[error("応答 JSON を解析できません: {0}")]
    Json(serde_json::Error),
    #[error("応答が UTF-8 ではありません: {0}")]
    Utf8(std::str::Utf8Error),
    #[error("SSE frame の JSON を解析できません: {0}")]
    Sse(serde_json::Error),
    #[error("request id に一致する JSON-RPC 応答がありません")]
    MissingResponse,
}

pub(crate) fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
}

pub(crate) fn accept_headers(mut headers: HeaderMap) -> HeaderMap {
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/event-stream"),
    );
    headers
}

pub(crate) fn response_frames(
    content_type: Option<&str>,
    body: &[u8],
) -> Result<Vec<Value>, WireError> {
    if content_type.is_some_and(|value| value.contains("text/event-stream")) {
        let text = std::str::from_utf8(body).map_err(WireError::Utf8)?;
        let mut frames = Vec::new();
        for frame in text.split("\n\n") {
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|data| data.strip_prefix(' ').unwrap_or(data))
                .collect::<Vec<_>>()
                .join("\n");
            if !data.is_empty() {
                frames.push(serde_json::from_str(&data).map_err(WireError::Sse)?);
            }
        }
        Ok(frames)
    } else {
        Ok(vec![serde_json::from_slice(body).map_err(WireError::Json)?])
    }
}

// Search deliberately accepts a single frame even when its id differs.
pub(crate) fn search_response(mut frames: Vec<Value>, request_id: i64) -> Result<Value, WireError> {
    if let Some(index) = frames
        .iter()
        .position(|frame| frame.get("id").and_then(Value::as_i64) == Some(request_id))
    {
        return Ok(frames.swap_remove(index));
    }
    if frames.len() == 1 {
        return Ok(frames.swap_remove(0));
    }
    Err(WireError::MissingResponse)
}
