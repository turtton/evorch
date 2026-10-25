use std::io::Cursor;
use std::time::Duration;

use base64::Engine;
use futures_util::StreamExt;
use serde::Deserialize;

use super::{ImageBackground, ImageGenerationError, ImageGenerationResponse, MAX_RESPONSE_BYTES};
use crate::{ProviderError, Usage};

#[cfg(test)]
mod tests;

#[derive(Deserialize)]
struct WireResponse {
    data: Vec<WireImage>,
    background: Option<ImageBackground>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireImage {
    b64_json: String,
}

#[derive(Deserialize)]
struct WireUsage {
    input_tokens: u64,
    output_tokens: u64,
}

fn invalid(detail: &str) -> ProviderError {
    ProviderError::InvalidJson {
        detail: detail.into(),
    }
}

/// Deliberately discard server text, endpoint URLs and token-store diagnostics.
pub(super) fn sanitize(error: ProviderError) -> ProviderError {
    match error {
        ProviderError::Timeout | ProviderError::RateLimited { .. } => error,
        ProviderError::Http { status, .. } => ProviderError::Http {
            status,
            body: "image generation request failed".into(),
        },
        ProviderError::Transport { .. } => ProviderError::Transport {
            message: "image generation transport failed".into(),
        },
        _ => ProviderError::Request("image generation session or request failed".into()),
    }
}

async fn bounded_body(response: reqwest::Response) -> Result<Vec<u8>, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(invalid("image response exceeds 48 MiB"));
    }
    bounded_stream(response.bytes_stream()).await
}

async fn bounded_stream(
    mut stream: impl futures_core::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin,
) -> Result<Vec<u8>, ProviderError> {
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(crate::http::map_request_error)
            .map_err(sanitize)?;
        if chunk.len() > MAX_RESPONSE_BYTES - body.len() {
            return Err(invalid("image response exceeds 48 MiB"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

pub(super) async fn decode(
    response: reqwest::Response,
) -> Result<ImageGenerationResponse, ImageGenerationError> {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let request_id = response
        .headers()
        .get("x-codex-imagegen-request-id")
        .or_else(|| response.headers().get("x-request-id"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let body = bounded_body(response).await?;
    if !(200..300).contains(&status) {
        return Err(http_error(status, &body, retry_after));
    }
    // Parsing and PNG validation are CPU work; never block the async executor.
    tokio::task::spawn_blocking(move || decode_body(&body, request_id))
        .await
        .map_err(|_| invalid("image response decoding failed"))?
}

fn decode_body(
    body: &[u8],
    request_id: Option<String>,
) -> Result<ImageGenerationResponse, ImageGenerationError> {
    let wire: WireResponse =
        serde_json::from_slice(body).map_err(|_| invalid("image response JSON is invalid"))?;
    // n > 1 is not part of this capability; do not silently discard billed output.
    if wire.data.len() != 1 {
        return Err(invalid("image response must contain exactly one image").into());
    }
    let png = base64::engine::general_purpose::STANDARD
        .decode(&wire.data[0].b64_json)
        .map_err(|_| invalid("image response base64 is invalid"))?;
    let mut reader = image::ImageReader::with_format(Cursor::new(&png), image::ImageFormat::Png);
    reader.limits(image::Limits::default());
    let image = reader
        .decode()
        .map_err(|_| invalid("image response is not a valid PNG"))?;
    let usage = wire
        .usage
        .map(|usage| Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            ..Usage::default()
        })
        .unwrap_or_default();
    Ok(ImageGenerationResponse {
        width: image.width(),
        height: image.height(),
        png,
        background: wire.background,
        usage,
        request_id,
    })
}

fn http_error(status: u16, body: &[u8], retry_after: Option<Duration>) -> ImageGenerationError {
    let value: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    let error = value.get("error").unwrap_or(&value);
    let marker = |key: &str| {
        error
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
    };
    // Only structured codes/types can change classification, never echoed prompt text.
    let quota = [marker("code"), marker("type")].iter().any(|code| {
        matches!(
            *code,
            "insufficient_quota"
                | "quota_exceeded"
                | "usage_limit_reached"
                | "billing_hard_limit_reached"
        )
    });
    if status == 402 || ((status == 400 || status == 403 || status == 429) && quota) {
        // Existing routing/observation taxonomy recognizes HTTP 402 as Quota.
        return ProviderError::Http {
            status: 402,
            body: "image generation quota exceeded".into(),
        }
        .into();
    }
    match status {
        429 => ProviderError::RateLimited { retry_after }.into(),
        400 | 422 => ImageGenerationError::Rejected(
            if [marker("code"), marker("type")].iter().any(|code| {
                matches!(
                    *code,
                    "moderation_blocked" | "content_policy_violation" | "safety_violations"
                )
            }) {
                "image request rejected by safety policy"
            } else {
                "invalid image generation request"
            }
            .into(),
        ),
        _ => ProviderError::Http {
            status,
            body: "image generation request failed".into(),
        }
        .into(),
    }
}
