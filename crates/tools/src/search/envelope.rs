//! MCP JSON-RPC / SSE envelope を純粋関数で正規化するパーサ。

use serde_json::Value;

use super::error::SearchError;
use super::mcp::McpToolSuccess;

/// JSON 応答または SSE frame 群から request_id に一致する応答を抽出する。
///
/// content_type が `text/event-stream` を含む場合は SSE frame（`\n\n` 区切り、
/// `data:` 行連結、event/comment 行無視）として解析し、`id` が一致する frame、
/// 無ければ単一 frame を処理する。それ以外は body 全体を単一 JSON 応答として扱う。
///
/// # Errors
/// JSON 解析・envelope 構造・id 照合・content text 抽出のいずれかに失敗した場合は
/// [`SearchError::Protocol`]、JSON-RPC `error` または `result.isError == true` の場合は
/// [`SearchError::ProviderRejected`] を返す。
pub(crate) fn parse_envelope(
    request_id: i64,
    content_type: Option<&str>,
    body: &[u8],
) -> Result<McpToolSuccess, SearchError> {
    let frames = if content_type.is_some_and(|value| value.contains("text/event-stream")) {
        sse_frames(body)?
    } else {
        json_frames(body)?
    };
    let response = pick_response(frames, request_id)?;
    success_of(&response)
}

fn json_frames(body: &[u8]) -> Result<Vec<Value>, SearchError> {
    let parsed = serde_json::from_slice::<Value>(body)
        .map_err(|error| SearchError::Protocol(format!("応答 JSON を解析できません: {error}")))?;
    Ok(vec![parsed])
}

fn sse_frames(body: &[u8]) -> Result<Vec<Value>, SearchError> {
    let text = std::str::from_utf8(body)
        .map_err(|error| SearchError::Protocol(format!("応答が UTF-8 ではありません: {error}")))?;
    let mut frames = Vec::new();
    for frame in text.split("\n\n") {
        let data = data_lines(frame);
        if data.is_empty() {
            continue;
        }
        let parsed = serde_json::from_str::<Value>(&data).map_err(|error| {
            SearchError::Protocol(format!("SSE frame の JSON を解析できません: {error}"))
        })?;
        frames.push(parsed);
    }
    Ok(frames)
}

fn data_lines(frame: &str) -> String {
    frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| data.strip_prefix(' ').unwrap_or(data))
        .collect::<Vec<_>>()
        .join("\n")
}

fn pick_response(mut frames: Vec<Value>, request_id: i64) -> Result<Value, SearchError> {
    if let Some(index) = frames
        .iter()
        .position(|frame| frame.get("id").and_then(Value::as_i64) == Some(request_id))
    {
        return Ok(frames.swap_remove(index));
    }
    if frames.len() == 1 {
        return Ok(frames.swap_remove(0));
    }
    Err(SearchError::Protocol(
        "request id に一致する JSON-RPC 応答がありません".to_owned(),
    ))
}

fn success_of(response: &Value) -> Result<McpToolSuccess, SearchError> {
    if let Some(error) = response.get("error") {
        return Err(SearchError::ProviderRejected(error_message(error)));
    }
    let result = response
        .get("result")
        .ok_or_else(|| SearchError::Protocol("応答に result がありません".to_owned()))?;
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let message = content_texts(result).join("\n");
        return Err(SearchError::ProviderRejected(if message.is_empty() {
            "isError=true の応答に content text がありません".to_owned()
        } else {
            message
        }));
    }
    let Some(content) = result.get("content").and_then(Value::as_array) else {
        return Err(SearchError::Protocol(
            "応答に content がありません".to_owned(),
        ));
    };
    let found = content.iter().find_map(|item| {
        let text = item.get("text").and_then(Value::as_str)?;
        let usage = item.get("_meta").filter(|meta| !meta.is_null()).cloned();
        Some((text, usage))
    });
    let Some((text, usage)) = found else {
        return Err(SearchError::Protocol(
            "content に text がありません".to_owned(),
        ));
    };
    Ok(McpToolSuccess {
        text: text.to_owned(),
        usage,
    })
}

fn error_message(error: &Value) -> String {
    match error.get("message").and_then(Value::as_str) {
        Some(message) => message.to_owned(),
        None => error.to_string(),
    }
}

fn content_texts(result: &Value) -> Vec<String> {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const JSON_CONTENT_TYPE: Option<&str> = Some("application/json");
    const SSE_CONTENT_TYPE: Option<&str> = Some("text/event-stream");

    // Given: text を持つ JSON 応答 / When: parse_envelope を呼ぶ / Then: text が抽出され usage は None になる
    #[test]
    fn parses_plain_json_success() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"result text"}]}}"#;

        let success = parse_envelope(1, JSON_CONTENT_TYPE, body).expect("正常な JSON 応答");

        assert_eq!(success.text, "result text");
        assert_eq!(success.usage, None);
    }

    // Given: content-type 未指定の JSON 応答 / When: parse_envelope を呼ぶ / Then: JSON として解析される
    #[test]
    fn treats_missing_content_type_as_json() {
        let body =
            br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"no ct"}]}}"#;

        let success = parse_envelope(1, None, body).expect("content-type 未指定は JSON 扱い");

        assert_eq!(success.text, "no ct");
    }

    // Given: 単一 SSE frame の応答 / When: parse_envelope を呼ぶ / Then: data frame の text が返る
    #[test]
    fn parses_single_sse_frame() {
        let body = concat!(
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"sse\"}]}}\n",
            "\n",
        )
        .as_bytes();

        let success = parse_envelope(1, SSE_CONTENT_TYPE, body).expect("単一 frame は解析できる");

        assert_eq!(success.text, "sse");
    }

    // Given: comment frame と id が異なる frame を含む複数 SSE frame / When: parse_envelope を呼ぶ / Then: id が一致する frame だけが採用される
    #[test]
    fn picks_matching_id_frame_from_multi_frame_sse() {
        let body = concat!(
            ": keepalive\n",
            "event: ping\n",
            "\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"other\"}]}}\n",
            "\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"matched\"}]}}\n",
            "\n",
        )
        .as_bytes();

        let success =
            parse_envelope(1, SSE_CONTENT_TYPE, body).expect("id 一致 frame が採用される");

        assert_eq!(success.text, "matched");
    }

    // Given: id 一致 frame のない複数 SSE frame / When: parse_envelope を呼ぶ / Then: Protocol error になる
    #[test]
    fn rejects_multi_frame_sse_without_matching_id() {
        let body = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"a\"}]}}\n",
            "\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"b\"}]}}\n",
            "\n",
        )
        .as_bytes();

        let error = parse_envelope(1, SSE_CONTENT_TYPE, body).expect_err("一致 frame がない");

        assert!(matches!(error, SearchError::Protocol(_)));
    }

    // Given: JSON-RPC error envelope / When: parse_envelope を呼ぶ / Then: message が ProviderRejected になる
    #[test]
    fn maps_jsonrpc_error_to_provider_rejected() {
        let body =
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Tool not found"}}"#;

        let error = parse_envelope(1, JSON_CONTENT_TYPE, body).expect_err("error envelope");

        assert!(matches!(
            error,
            SearchError::ProviderRejected(message) if message == "Tool not found"
        ));
    }

    // Given: isError=true の result / When: parse_envelope を呼ぶ / Then: content text が ProviderRejected になる
    #[test]
    fn maps_is_error_result_to_provider_rejected() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"search failed"}]}}"#;

        let error = parse_envelope(1, JSON_CONTENT_TYPE, body).expect_err("isError 応答");

        assert!(matches!(
            error,
            SearchError::ProviderRejected(message) if message == "search failed"
        ));
    }

    // Given: text を持たない content / When: parse_envelope を呼ぶ / Then: Protocol error になる
    #[test]
    fn rejects_content_without_text() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"image"}]}}"#;

        let error = parse_envelope(1, JSON_CONTENT_TYPE, body).expect_err("text がない");

        assert!(matches!(error, SearchError::Protocol(_)));
    }

    // Given: content item に _meta / When: parse_envelope を呼ぶ / Then: _meta が usage へ透過される
    #[test]
    fn passes_meta_through_to_usage() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"with meta","_meta":{"searchTime":123}}]}}"#;

        let success = parse_envelope(1, JSON_CONTENT_TYPE, body).expect("meta 付き応答");

        assert_eq!(success.usage, Some(json!({"searchTime": 123})));
    }

    // Given: JSON として解釈できない body / When: parse_envelope を呼ぶ / Then: Protocol error になる
    #[test]
    fn rejects_malformed_json_body() {
        let error = parse_envelope(1, JSON_CONTENT_TYPE, b"not json").expect_err("不正 JSON");

        assert!(matches!(error, SearchError::Protocol(_)));
    }
}
