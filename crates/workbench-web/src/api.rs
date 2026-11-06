//! JSON endpoints: `POST /api/v1/request` and `GET /api/v1/health`.

use crate::backend::{WebSession, WorkbenchBackend};
use crate::http_error::{error_response, json_response, reject, reply_response};
use crate::security::close_connection;
use crate::state::AppState;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::response::Response;
use http::StatusCode;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use workbench_protocol::envelope::{
    Control, Provenance, ReplyEnvelope, ReplyPayload, ReplyResult, RequestBody, RequestEnvelope,
    Surface, decode_request,
};
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::handshake::ServerHello;
use workbench_protocol::ids::{ClientId, RequestId};

fn internal() -> WorkbenchError {
    WorkbenchError::new(ErrorCode::Internal, "internal error", RetryAdvice::Never)
}

/// The identity the adapter assigns, from the session id the browser chose.
///
/// The provenance built from it is what the core records; nothing the browser
/// sends can make it an agent, another surface or another actor.
fn ingress(request: &RequestEnvelope) -> Result<(WebSession, Provenance), WorkbenchError> {
    let client_id = ClientId::new(format!("web-{}", request.session_id)).map_err(|_| {
        WorkbenchError::invalid_request("session_id is too long for the web adapter")
    })?;
    let provenance = Provenance::human(Surface::Web, client_id.clone());
    let session = WebSession {
        session_id: request.session_id.clone(),
        client_id,
    };
    Ok((session, provenance))
}

fn reply(request_id: RequestId, result: ReplyResult) -> Response {
    reply_response(ReplyEnvelope { request_id, result })
}

fn too_large() -> Response {
    let mut response = error_response(
        WorkbenchError::new(
            ErrorCode::LimitExceeded,
            "request body is too large",
            RetryAdvice::Never,
        ),
        None,
    );
    close_connection(&mut response);
    response
}

/// Read the whole body, or say why not. The answer closes the connection:
/// the rest of an unfinished body would otherwise be read as the next request.
async fn read_body(
    body: Body,
    limit: usize,
    deadline: Duration,
) -> Result<axum::body::Bytes, Box<Response>> {
    let outcome = tokio::time::timeout(deadline, to_bytes(body, limit)).await;
    let mut failure = match outcome {
        Ok(Ok(bytes)) => return Ok(bytes),
        Ok(Err(_)) => return Err(Box::new(too_large())),
        Err(_) => reject(
            StatusCode::REQUEST_TIMEOUT,
            ErrorCode::InvalidRequest,
            "request body was not received in time",
        ),
    };
    close_connection(&mut failure);
    Err(Box::new(failure))
}

pub(crate) async fn request<B: WorkbenchBackend>(
    State(state): State<Arc<AppState<B>>>,
    request: Request<Body>,
) -> Response {
    let limit = state.max_request_bytes;
    let declared = request
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if declared.is_some_and(|length| length > limit) {
        return too_large();
    }
    let bytes = match read_body(request.into_body(), limit, state.body_read_timeout).await {
        Ok(bytes) => bytes,
        Err(response) => return *response,
    };

    let envelope = match decode_request(&bytes, &state.wire_limits) {
        Ok(envelope) => envelope,
        Err(rejection) => return error_response(rejection.error, rejection.request_id.as_ref()),
    };
    let request_id = envelope.request_id.clone();

    match &envelope.body {
        RequestBody::Control(Control::Ping { nonce }) => {
            return reply(
                request_id,
                ReplyResult::Success(ReplyPayload::Pong { nonce: *nonce }),
            );
        }
        RequestBody::Control(Control::Pong { .. }) => {
            return reply(request_id, ReplyResult::Success(ReplyPayload::Ack));
        }
        RequestBody::Control(Control::Subscribe(_) | Control::Unsubscribe { .. }) => {
            return error_response(
                WorkbenchError::invalid_request("subscribe with GET /api/v1/events"),
                Some(&request_id),
            );
        }
        RequestBody::Command(_) | RequestBody::Query(_) => {}
    }

    let (session, provenance) = match ingress(&envelope) {
        Ok(pair) => pair,
        Err(error) => return error_response(error, Some(&request_id)),
    };
    let Ok(slot) = Arc::clone(&state.shared.backend_slots).try_acquire_owned() else {
        return error_response(
            WorkbenchError::new(
                ErrorCode::Busy,
                "too many requests in flight",
                RetryAdvice::SameRequest,
            ),
            Some(&request_id),
        );
    };
    // A command must finish even if the browser goes away mid-request; the
    // request id lets the browser ask again and get the recorded outcome. The
    // slot belongs to the task, so a disconnect does not free it early.
    let backend = Arc::clone(&state.backend);
    let handle = tokio::spawn(async move {
        let answer = backend.handle_request(&session, envelope, provenance).await;
        drop(slot);
        answer
    });
    match handle.await {
        Ok(answer) if answer.request_id == request_id => reply_response(answer),
        Ok(_) | Err(_) => error_response(internal(), Some(&request_id)),
    }
}

#[derive(Serialize)]
struct Adapter {
    name: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
struct Health<'a> {
    status: &'static str,
    adapter: Adapter,
    hello: &'a ServerHello,
}

pub(crate) async fn health<B: WorkbenchBackend>(State(state): State<Arc<AppState<B>>>) -> Response {
    match state.backend.server_hello().await {
        Ok(hello) => match serde_json::to_vec(&Health {
            status: "ok",
            adapter: Adapter {
                name: env!("CARGO_PKG_NAME"),
                version: env!("CARGO_PKG_VERSION"),
            },
            hello: &hello,
        }) {
            Ok(body) => json_response(StatusCode::OK, body),
            Err(_) => error_response(internal(), None),
        },
        Err(error) => error_response(error, None),
    }
}

pub(crate) async fn fallback<B: WorkbenchBackend>(
    State(state): State<Arc<AppState<B>>>,
    request: Request<Body>,
) -> Response {
    let path = request.uri().path();
    if path.starts_with("/api/") {
        return reject(StatusCode::NOT_FOUND, ErrorCode::NotFound, "not found");
    }
    crate::assets::serve(state.assets, request.method(), path)
}

/// The answer to a method a known API path does not support. Registered as the
/// route's own fallback, since the router would otherwise answer `405` with
/// the body of its not-found fallback and a `HEAD` entry in `Allow`.
pub(crate) fn method_not_allowed(
    allow: &'static str,
) -> impl Fn() -> std::future::Ready<Response> + Clone + Send + Sync + 'static {
    move || {
        let mut response = reject(
            StatusCode::METHOD_NOT_ALLOWED,
            ErrorCode::InvalidRequest,
            "method not allowed",
        );
        response
            .headers_mut()
            .insert(http::header::ALLOW, http::HeaderValue::from_static(allow));
        // The request body, if any, is not read.
        close_connection(&mut response);
        std::future::ready(response)
    }
}
