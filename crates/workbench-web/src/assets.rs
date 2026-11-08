//! Embedded front-end assets served by `GET /`.
//!
//! The adapter only knows how to serve a fixed table of files that were
//! compiled in. The real front end (P6) supplies its own table through
//! [`WebServer::start_with_assets`](crate::WebServer::start_with_assets); this
//! crate ships a placeholder page.

use axum::body::Body;
use axum::response::Response;
use http::{HeaderValue, Method, StatusCode, header};

/// One embedded file. `path` has no leading slash; `index.html` answers `/`.
#[derive(Debug, Clone, Copy)]
pub struct StaticAsset {
    pub path: &'static str,
    pub content_type: &'static str,
    pub body: &'static [u8],
}

/// What the server serves when no front end was supplied.
pub const PLACEHOLDER_ASSETS: &[StaticAsset] = &[StaticAsset {
    path: "index.html",
    content_type: "text/html; charset=utf-8",
    body: include_bytes!("../assets/index.html"),
}];

/// Look up an asset by request path. There is no fallback to `index.html`
/// for unknown paths: a typo is a `404`, not the app shell.
pub(crate) fn lookup<'a>(assets: &'a [StaticAsset], path: &str) -> Option<&'a StaticAsset> {
    let wanted = match path.strip_prefix('/') {
        Some("") => "index.html",
        Some(rest) => rest,
        None => return None,
    };
    assets.iter().find(|asset| asset.path == wanted)
}

pub(crate) fn serve(assets: &[StaticAsset], method: &Method, path: &str) -> Response {
    let not_found = || {
        let mut response = Response::new(Body::from("not found"));
        *response.status_mut() = StatusCode::NOT_FOUND;
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        response
    };
    if method != Method::GET || path.starts_with("/api/") {
        return not_found();
    }
    let Some(asset) = lookup(assets, path) else {
        return not_found();
    };
    let mut response = Response::new(Body::from(asset.body));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    response
}
