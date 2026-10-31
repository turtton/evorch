//! `Retry-After` 助言の解釈。HTTP ヘッダ、SSE 失敗イベント内 headers、メッセージ本文に共通です。
//!
//! Codex CLI (`http-client/src/retry_after.rs`, `codex-api/src/sse/responses_error.rs`)
//! と同じく、非負整数秒と HTTP-date の両方を受理します。

use std::time::{Duration, SystemTime};

/// `Retry-After` の値 (非負整数秒または HTTP-date) を待機時間へ変換します。
///
/// 過去の HTTP-date は待機なし (`Duration::ZERO`) とします。解釈できない値は `None` です。
pub(crate) fn parse_value(value: &str) -> Option<Duration> {
    // 現在時刻を先に採取し、スケジューリング遅延で期限を縮めない。
    let now = SystemTime::now();
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }
    Some(
        httpdate::parse_http_date(value)
            .ok()?
            .duration_since(now)
            .unwrap_or_default(),
    )
}

/// HTTP 応答ヘッダの `Retry-After` を解釈します。
pub(crate) fn from_header_map(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    parse_value(headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?)
}

/// SSE 失敗イベントの `error.headers` (JSON object) から `retry-after` を解釈します。
///
/// ヘッダ名は大文字小文字を区別せず、値は文字列または数値を受理します。
pub(crate) fn from_json_headers(headers: &serde_json::Value) -> Option<Duration> {
    let (_, value) = headers
        .as_object()?
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))?;
    match value {
        serde_json::Value::String(value) => parse_value(value),
        serde_json::Value::Number(value) => parse_value(&value.to_string()),
        _ => None,
    }
}

/// レート制限メッセージの「try again in N s/ms/m」から待機時間を取り出します。
///
/// 単位は `ms`、`s`/`second(s)`、`m`/`minute(s)` を受理します。Codex CLI が回復する
/// `s`/`ms`/`seconds` に分単位を加えたものです。
pub(crate) fn from_message(message: &str) -> Option<Duration> {
    const MARKER: &str = "try again in";
    let lower = message.to_ascii_lowercase();
    let rest = lower[lower.find(MARKER)? + MARKER.len()..].trim_start();
    let number_len = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let (number, rest) = rest.split_at(number_len);
    let value: f64 = number.parse().ok()?;
    let rest = rest.trim_start();
    let unit_len = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    let seconds = match &rest[..unit_len] {
        "ms" => value / 1000.0,
        "s" | "second" | "seconds" => value,
        "m" | "minute" | "minutes" => value * 60.0,
        _ => return None,
    };
    Duration::try_from_secs_f64(seconds).ok()
}

#[cfg(test)]
mod tests;
