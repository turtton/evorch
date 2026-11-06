//! The embedded web workbench (`UI_ASSETS`): what is served, with which
//! headers, and that the files keep to what the Content-Security-Policy
//! allows (no inline code, nothing from another origin, every module import
//! resolvable from the table).

mod common;

use axum::Router;
use axum::body::Body;
use common::*;
use http::{StatusCode, header};
use std::collections::BTreeSet;
use std::sync::Arc;
use workbench_web::{UI_ASSETS, WebApp, WebServerConfig};

fn router() -> Router {
    WebApp::new(&WebServerConfig::default(), Arc::new(FakeBackend::new()))
        .unwrap()
        .router()
}

fn text(path: &str) -> &'static str {
    let asset = UI_ASSETS
        .iter()
        .find(|asset| asset.path == path)
        .unwrap_or_else(|| panic!("{path} is not in UI_ASSETS"));
    std::str::from_utf8(asset.body).unwrap()
}

fn expected_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("webmanifest") => "application/manifest+json",
        other => panic!("no expected content type for {path} ({other:?})"),
    }
}

fn assert_security_headers(headers: &http::HeaderMap, what: &str) {
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    let directives: Vec<&str> = csp.split(';').map(str::trim).collect();
    for required in [
        "default-src 'none'",
        "script-src 'self'",
        "style-src 'self'",
        "connect-src 'self'",
        "base-uri 'none'",
        "form-action 'none'",
        "frame-ancestors 'none'",
        "object-src 'none'",
        "require-trusted-types-for 'script'",
    ] {
        assert!(
            directives.contains(&required),
            "{what}: CSP lacks {required}: {csp}"
        );
    }
    for forbidden in [
        "'unsafe-inline'",
        "'unsafe-eval'",
        "'unsafe-hashes'",
        "*",
        "http:",
        "https:",
    ] {
        assert!(
            !csp.split_whitespace().any(|token| token == forbidden),
            "{what}: CSP allows {forbidden}: {csp}"
        );
    }
    assert_eq!(headers["x-content-type-options"], "nosniff", "{what}");
    assert_eq!(headers["x-frame-options"], "DENY", "{what}");
    assert_eq!(headers["referrer-policy"], "no-referrer", "{what}");
}

#[tokio::test]
async fn every_ui_asset_is_served_with_its_type_and_the_security_headers() {
    let router = router();
    assert_eq!(UI_ASSETS[0].path, "index.html");
    for asset in UI_ASSETS {
        assert_eq!(
            asset.content_type,
            expected_type(asset.path),
            "{}",
            asset.path
        );
        let uri = if asset.path == "index.html" {
            "/".to_owned()
        } else {
            format!("/{}", asset.path)
        };
        let response = send(&router, get(&uri).body(Body::empty()).unwrap()).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let headers = response.headers().clone();
        assert_eq!(headers[header::CONTENT_TYPE], asset.content_type, "{uri}");
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache", "{uri}");
        assert_security_headers(&headers, &uri);
        assert_eq!(body_bytes(response).await, asset.body, "{uri}");
    }
}

#[tokio::test]
async fn unknown_paths_are_404_with_the_security_headers() {
    let router = router();
    for uri in [
        "/js/missing.js",
        "/js/",
        "/js",
        "/assets/js/app.js",
        "/app.css/",
        "/index.htm",
        "/tests/web/unit.mjs",
        "/%2e%2e/Cargo.toml",
    ] {
        let response = send(&router, get(uri).body(Body::empty()).unwrap()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        assert_security_headers(response.headers(), uri);
    }
}

/// Values of `name="..."` attributes in an HTML text.
fn attribute_values<'a>(html: &'a str, name: &str) -> Vec<&'a str> {
    let needle = format!(" {name}=\"");
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find(&needle) {
        rest = &rest[start + needle.len()..];
        let end = rest.find('"').unwrap();
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    out
}

#[test]
fn the_shell_has_no_inline_code_and_only_local_references() {
    let html = text("index.html");
    let lower = html.to_ascii_lowercase();
    // Every <script> loads a file; there is no inline script or style.
    let scripts = lower.matches("<script").count();
    assert_eq!(
        scripts,
        lower.matches("<script type=\"module\" src=\"/").count(),
        "inline <script>"
    );
    assert!(!lower.contains("<style"), "inline <style>");
    assert!(!lower.contains(" style="), "style attribute");
    assert!(!lower.contains("javascript:"), "javascript: URL");
    for (index, _) in lower.match_indices(" on") {
        let attr: String = lower[index + 3..]
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect();
        assert!(
            !lower[index + 3 + attr.len()..].starts_with('='),
            "inline event handler on{attr}"
        );
    }
    let served: BTreeSet<&str> = UI_ASSETS.iter().map(|asset| asset.path).collect();
    for reference in attribute_values(html, "src")
        .into_iter()
        .chain(attribute_values(html, "href"))
    {
        if reference.starts_with("#/") {
            continue;
        }
        let path = reference
            .strip_prefix('/')
            .unwrap_or_else(|| panic!("{reference} is not a same-origin absolute path"));
        assert!(
            served.contains(path),
            "{reference} is referenced but not served"
        );
    }
    let manifest: serde_json::Value = serde_json::from_str(text("manifest.webmanifest")).unwrap();
    for icon in manifest["icons"].as_array().unwrap() {
        let src = icon["src"].as_str().unwrap();
        assert!(served.contains(src.trim_start_matches('/')), "{src}");
    }
}

#[test]
fn scripts_and_styles_avoid_html_sinks_and_other_origins() {
    for asset in UI_ASSETS {
        let body = std::str::from_utf8(asset.body).unwrap();
        if asset.path.ends_with(".js") {
            for sink in [
                "innerHTML",
                "outerHTML",
                "insertAdjacentHTML",
                "document.write",
                "eval(",
                "new Function",
                "setAttribute(\"style\"",
                "srcdoc",
                "DOMParser",
                "createContextualFragment",
            ] {
                assert!(!body.contains(sink), "{} uses {sink}", asset.path);
            }
        }
        if asset.path.ends_with(".css") {
            assert!(
                !body.contains("@import"),
                "{} imports a stylesheet",
                asset.path
            );
            assert!(!body.contains("url("), "{} loads a resource", asset.path);
        }
        if asset.path.ends_with(".js") || asset.path.ends_with(".css") {
            for line in body.lines() {
                let code = line.trim_start();
                if code.starts_with("//") || code.starts_with('*') || code.starts_with("/*") {
                    continue;
                }
                assert!(
                    !code.contains("fetch(\"http") && !code.contains("EventSource(\"http"),
                    "{}: absolute URL request: {line}",
                    asset.path
                );
            }
        }
    }
}

/// Module specifiers in `import ... from "..."` statements.
fn imports(source: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for statement in source.split("from \"").skip(1) {
        out.push(&statement[..statement.find('"').unwrap()]);
    }
    out
}

fn resolve(from: &str, specifier: &str) -> String {
    assert!(
        specifier.starts_with("./") || specifier.starts_with("../"),
        "{from}: bare or absolute import {specifier}"
    );
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for segment in specifier.split('/') {
        match segment {
            "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

#[test]
fn every_module_import_resolves_and_every_module_is_used() {
    let html = text("index.html");
    let entries: Vec<String> = attribute_values(html, "src")
        .into_iter()
        .filter(|src| src.ends_with(".js"))
        .map(|src| src.trim_start_matches('/').to_owned())
        .collect();
    assert_eq!(entries, ["js/app.js"]);
    let mut reached = BTreeSet::new();
    let mut queue = entries;
    while let Some(path) = queue.pop() {
        if !reached.insert(path.clone()) {
            continue;
        }
        for specifier in imports(text(&path)) {
            queue.push(resolve(&path, specifier));
        }
    }
    let modules: BTreeSet<String> = UI_ASSETS
        .iter()
        .filter(|asset| asset.path.ends_with(".js"))
        .map(|asset| asset.path.to_owned())
        .collect();
    assert_eq!(
        reached, modules,
        "served modules and the import graph differ"
    );
}
