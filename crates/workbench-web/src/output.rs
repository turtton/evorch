//! Last output boundary: all JSON responses and each SSE data item.
use crate::WorkbenchBackend;
use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use http::{StatusCode, header};
use serde::Serialize;
use std::sync::Arc;

pub(crate) fn encode<B: WorkbenchBackend, T: Serialize>(
    backend: &B,
    value: &T,
) -> serde_json::Result<String> {
    Ok(backend
        .protect_output(serde_json::to_value(value)?)
        .to_string())
}

/// Outermost layer, so HTTP ingress rejections and locally generated replies
/// are protected as well. JSON bodies are already buffered by the adapter;
/// streaming SSE bodies are never collected here (their data uses encode).
pub(crate) async fn protect_response<B: WorkbenchBackend>(
    State(backend): State<Arc<B>>,
    request: Request,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| {
            h.split(';')
                .next()
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
        });
    if !is_json {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let value = match to_bytes(body, usize::MAX).await {
        // HEAD has no output value to protect; retain its status and headers.
        Ok(bytes) if bytes.is_empty() => return Response::from_parts(parts, Body::empty()),
        Ok(bytes) => serde_json::from_slice(&bytes).ok(),
        Err(_) => None,
    }.unwrap_or_else(|| {
        parts.status = StatusCode::INTERNAL_SERVER_ERROR;
        serde_json::json!({"status":"error","payload":{"code":"internal","message":"output serialization failed","retry":"never"}})
    });
    let body = backend.protect_output(value).to_string();
    // Masking may change the byte length. Hyper derives the new length.
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(body))
}
