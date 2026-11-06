//! JSON endpoints: `POST /api/v1/request` and `GET /api/v1/health`.

use crate::backend::{WebSession, WorkbenchBackend};
use crate::http_error::{error_response, json_response, reply_response};
use crate::state::AppState;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::response::Response;
use http::StatusCode;
use serde::Serialize;
use std::sync::Arc;
use workbench_protocol::envelope::{
    Control, Provenance, ReplyEnvelope, ReplyPayload, ReplyResult, RequestBody, RequestEnvelope,
    Surface, decode_request,
};
use workbench_protocol::errors::{ErrorCode, RetryAdvice, WorkbenchError};
use workbench_protocol::handshake::CoreStatus;
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

pub(crate) async fn request<B: WorkbenchBackend>(
    State(state): State<Arc<AppState<B>>>,
    request: Request<Body>,
) -> Response {
    let too_large = || {
        error_response(
            WorkbenchError::new(
                ErrorCode::LimitExceeded,
                "request body is too large",
                RetryAdvice::Never,
            ),
            None,
        )
    };
    let limit = state.max_request_bytes;
    let declared = request
        .headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if declared.is_some_and(|length| length > limit) {
        return too_large();
    }
    let Ok(bytes) = to_bytes(request.into_body(), limit).await else {
        return too_large();
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
    // A command must finish even if the browser goes away mid-request; the
    // request id lets the browser ask again and get the recorded outcome.
    let backend = Arc::clone(&state.backend);
    let handle =
        tokio::spawn(async move { backend.handle_request(&session, envelope, provenance).await });
    match handle.await {
        Ok(answer) if answer.request_id == request_id => reply_response(answer),
        Ok(_) | Err(_) => error_response(internal(), Some(&request_id)),
    }
}

#[derive(Serialize)]
struct Health<'a> {
    status: &'static str,
    core: &'a CoreStatus,
}

pub(crate) async fn health<B: WorkbenchBackend>(State(state): State<Arc<AppState<B>>>) -> Response {
    match state.backend.core_status().await {
        Ok(core) => match serde_json::to_vec(&Health {
            status: "ok",
            core: &core,
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
    crate::assets::serve(state.assets, request.method(), request.uri().path())
}
