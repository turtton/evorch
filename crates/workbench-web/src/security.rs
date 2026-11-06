//! Browser-facing protections (ADR 0045 §5).
//!
//! There is no authentication. What stands between a web page the user opens
//! elsewhere and the core is this layer:
//!
//! - `Host` allowlist (DNS rebinding): a rebound name never matches.
//! - `Origin` allowlist or same origin for every request that carries one, and
//!   `Origin: null` refused; `Sec-Fetch-Site: cross-site` refused.
//! - State changing requests (`POST`) need `Content-Type: application/json`,
//!   which a cross-site form cannot send and a cross-site `fetch` can only send
//!   after a CORS preflight that this server never approves.
//! - No CORS headers at all, so no other origin can read a response.
//! - `X-Forwarded-*`, `Forwarded` and Tailscale identity headers are never
//!   read. Only the `Host` header and the connection decide.
//! - Request, connection and stream counts are bounded.

use crate::config::Policy;
use crate::http_error::reject;
use crate::shutdown::ShutdownSignal;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use std::sync::Arc;
use tokio::sync::Semaphore;
use workbench_protocol::errors::ErrorCode;

pub(crate) const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; \
style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; \
manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; \
object-src 'none'";

const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");
const X_FRAME_OPTIONS: HeaderName = HeaderName::from_static("x-frame-options");
const X_CONTENT_TYPE_OPTIONS: HeaderName = HeaderName::from_static("x-content-type-options");
const REFERRER_POLICY: HeaderName = HeaderName::from_static("referrer-policy");
const CORP: HeaderName = HeaderName::from_static("cross-origin-resource-policy");
const COOP: HeaderName = HeaderName::from_static("cross-origin-opener-policy");

/// State shared by the guard and the handlers.
pub(crate) struct Shared {
    pub(crate) policy: Policy,
    pub(crate) in_flight: Arc<Semaphore>,
    pub(crate) sse_slots: Arc<Semaphore>,
    pub(crate) shutdown: ShutdownSignal,
}

fn deny(message: &'static str) -> Response {
    reject(StatusCode::FORBIDDEN, ErrorCode::PermissionDenied, message)
}

fn single_header<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    first.to_str().map(Some).map_err(|_| ())
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    matches!(
        single_header(headers, &header::CONTENT_TYPE),
        Ok(Some(value))
            if value
                .split(';')
                .next()
                .is_some_and(|essence| essence.trim().eq_ignore_ascii_case("application/json"))
    )
}

/// Everything decided from the request head, before any handler runs.
fn check_head(shared: &Shared, request: &Request) -> Result<(), Box<Response>> {
    let method = request.method();
    if method != Method::GET && method != Method::POST {
        return Err(Box::new(reject(
            StatusCode::METHOD_NOT_ALLOWED,
            ErrorCode::InvalidRequest,
            "method not allowed",
        )));
    }
    let headers = request.headers();

    let host = match single_header(headers, &header::HOST) {
        Ok(Some(host)) if shared.policy.host_allowed(host) => host,
        _ => return Err(Box::new(deny("host not allowed"))),
    };
    // An absolute-form request line must agree with the Host header.
    if let Some(authority) = request.uri().authority()
        && !authority.as_str().eq_ignore_ascii_case(host)
    {
        return Err(Box::new(deny("host not allowed")));
    }

    match single_header(headers, &SEC_FETCH_SITE) {
        Ok(None | Some("same-origin" | "same-site" | "none")) => {}
        _ => return Err(Box::new(deny("cross-site requests are not allowed"))),
    }

    match single_header(headers, &header::ORIGIN) {
        Ok(None) => {}
        Ok(Some(origin)) if shared.policy.origin_allowed(origin, host) => {}
        _ => return Err(Box::new(deny("origin not allowed"))),
    }

    if method == Method::POST && !is_json_content_type(headers) {
        return Err(Box::new(reject(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorCode::InvalidRequest,
            "content type must be application/json",
        )));
    }
    Ok(())
}

fn add_security_headers(response: &mut Response, api: bool) {
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(CORP, HeaderValue::from_static("same-origin"));
    headers.insert(COOP, HeaderValue::from_static("same-origin"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if api { "no-store" } else { "no-cache" }),
    );
}

/// The outermost layer: checks the head, bounds concurrency and stamps the
/// security headers on every answer, rejections included.
pub(crate) async fn guard(
    State(shared): State<Arc<Shared>>,
    request: Request,
    next: Next,
) -> Response {
    let api = request.uri().path().starts_with("/api/");
    let mut response = match check_head(&shared, &request) {
        Err(rejection) => *rejection,
        Ok(()) => match Arc::clone(&shared.in_flight).try_acquire_owned() {
            Ok(_permit) => next.run(request).await,
            Err(_) => reject(
                StatusCode::TOO_MANY_REQUESTS,
                ErrorCode::Busy,
                "too many requests in flight",
            ),
        },
    };
    add_security_headers(&mut response, api);
    response
}
