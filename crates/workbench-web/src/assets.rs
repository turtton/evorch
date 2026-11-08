//! Embedded front-end assets served by `GET /`.
//!
//! The adapter only knows how to serve a fixed table of files that were
//! compiled in. [`UI_ASSETS`] is the web workbench (ADR 0045 P6): plain HTML,
//! CSS and ES modules from `assets/`, with no build step, no inline script or
//! style and nothing loaded from another origin, so the strict
//! Content-Security-Policy of the `security` module applies unchanged.
//! [`WebServer::start_with_assets`](crate::WebServer::start_with_assets)
//! serves another table.

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

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const JS: &str = "text/javascript; charset=utf-8";

macro_rules! asset {
    ($path:literal, $content_type:expr) => {
        StaticAsset {
            path: $path,
            content_type: $content_type,
            body: include_bytes!(concat!("../assets/", $path)),
        }
    };
}

/// The web workbench served by default. Every module `app.js` imports,
/// directly or not, must be listed: a missing one is a `404` in the browser.
pub const UI_ASSETS: &[StaticAsset] = &[
    asset!("index.html", HTML),
    asset!("app.css", CSS),
    asset!("icon.svg", "image/svg+xml"),
    asset!("manifest.webmanifest", "application/manifest+json"),
    asset!("js/app.js", JS),
    asset!("js/api.js", JS),
    asset!("js/cards.js", JS),
    asset!("js/dom.js", JS),
    asset!("js/format.js", JS),
    asset!("js/history.js", JS),
    asset!("js/markdown.js", JS),
    asset!("js/paging.js", JS),
    asset!("js/reread.js", JS),
    asset!("js/sse.js", JS),
    asset!("js/store.js", JS),
    asset!("js/views.js", JS),
    asset!("js/wire.js", JS),
];

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
