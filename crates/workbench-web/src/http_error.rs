//! Mapping of protocol errors to HTTP responses.
//!
//! Every non-success answer has the shape of a protocol reply that failed:
//! `{"request_id"?: "..", "status": "error", "payload": {WorkbenchError}}`.
//! Clients branch on `payload.code` and `payload.retry`; the HTTP status is a
//! coarse class for proxies and `fetch` users.

use axum::body::Body;
use axum::response::Response;
use http::{HeaderValue, StatusCode, header};
use serde::Serialize;
use workbench_protocol::envelope::{ReplyEnvelope, ReplyResult};
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::ids::RequestId;

/// The one place an error code becomes a status code.
pub fn status_for(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::InvalidRequest
        | ErrorCode::UnsupportedVersion
        | ErrorCode::UnsupportedCapability => StatusCode::BAD_REQUEST,
        ErrorCode::PermissionDenied => StatusCode::FORBIDDEN,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Conflict
        | ErrorCode::AlreadyResolved
        | ErrorCode::StaleRevision
        | ErrorCode::StaleEpoch
        | ErrorCode::ResyncRequired
        | ErrorCode::RestoreBlocked => StatusCode::CONFLICT,
        ErrorCode::LimitExceeded => StatusCode::PAYLOAD_TOO_LARGE,
        ErrorCode::Busy => StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::Draining | ErrorCode::StorageUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Hide what only the core's logs should contain.
///
/// Internal and storage failures keep their code, retry advice and
/// correlation id but lose their text and details; the text of a backend
/// bug or a database error can name paths, SQL or secrets. Other messages are
/// written for people by the backend and are relayed.
pub fn sanitize(mut error: WorkbenchError) -> WorkbenchError {
    let generic = match error.code {
        ErrorCode::Internal => "internal error",
        ErrorCode::StorageUnavailable => "storage is unavailable",
        _ => return error,
    };
    error.message = generic.to_owned();
    error.details = None;
    error
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<&'a RequestId>,
    #[serde(flatten)]
    result: ErrorResult<'a>,
}

#[derive(Serialize)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
enum ErrorResult<'a> {
    Error(&'a WorkbenchError),
}

pub(crate) fn json_response(status: StatusCode, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// An error answer, optionally correlated with a request.
pub fn error_response(error: WorkbenchError, request_id: Option<&RequestId>) -> Response {
    let error = sanitize(error);
    let status = status_for(error.code);
    let body = serde_json::to_vec(&ErrorBody {
        request_id,
        result: ErrorResult::Error(&error),
    })
    .unwrap_or_else(|_| br#"{"status":"error","payload":{"code":"internal","message":"internal error","retry":"never"}}"#.to_vec());
    json_response(status, body)
}

/// A request rejected by the HTTP layer itself. `message` is a fixed string:
/// it never echoes header or body content.
pub(crate) fn reject(status: StatusCode, code: ErrorCode, message: &'static str) -> Response {
    let retry = match code {
        ErrorCode::Busy => RetryAdvice::AfterReconnect,
        _ => RetryAdvice::Never,
    };
    let mut response = error_response(WorkbenchError::new(code, message, retry), None);
    *response.status_mut() = status;
    response
}

/// The answer to a command or query.
pub fn reply_response(reply: ReplyEnvelope) -> Response {
    match reply.result {
        ReplyResult::Error(error) => error_response(error, Some(&reply.request_id)),
        ReplyResult::Success(_) => match serde_json::to_vec(&reply) {
            Ok(body) => json_response(StatusCode::OK, body),
            Err(_) => error_response(
                WorkbenchError::new(ErrorCode::Internal, "internal error", RetryAdvice::Never),
                Some(&reply.request_id),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_a_documented_status() {
        use ErrorCode::*;
        let table = [
            (InvalidRequest, 400),
            (UnsupportedVersion, 400),
            (UnsupportedCapability, 400),
            (PermissionDenied, 403),
            (NotFound, 404),
            (Conflict, 409),
            (AlreadyResolved, 409),
            (StaleRevision, 409),
            (StaleEpoch, 409),
            (ResyncRequired, 409),
            (RestoreBlocked, 409),
            (LimitExceeded, 413),
            (Busy, 429),
            (Draining, 503),
            (StorageUnavailable, 503),
            (Internal, 500),
        ];
        for (code, status) in table {
            assert_eq!(status_for(code).as_u16(), status, "{code:?}");
        }
    }

    #[test]
    fn internal_text_is_not_relayed() {
        let error = WorkbenchError::new(
            ErrorCode::Internal,
            "sqlite error at /home/u/.evorch/db",
            RetryAdvice::Never,
        );
        assert_eq!(sanitize(error).message, "internal error");
    }
}
