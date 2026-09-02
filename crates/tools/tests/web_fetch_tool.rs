mod common;

#[path = "common/guard_responses.rs"]
mod guard_responses;

use std::{
    io::Write,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use flate2::{Compression, write::GzEncoder};
use serde_json::{Value, json};
use tools::{
    DnsResolver, MAX_RESPONSE_BYTES, NetworkGuard, NetworkGuardError, Tool, ToolResult, WebFetch,
};

use common::{FixtureServer, TestResult};
use guard_responses::{chunked_response, identity_response, redirect, response_with_headers};

const TRUNCATION_HINT: &str = "Output truncated to 50KB (51200 bytes). Pass a `selector` argument to narrow extraction to the relevant section, or refine the URL to a more specific page.";

struct CountingResolver {
    addr: IpAddr,
    calls: AtomicUsize,
}

#[async_trait]
impl DnsResolver for CountingResolver {
    async fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, NetworkGuardError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![self.addr])
    }
}

fn tool(server: &FixtureServer) -> (WebFetch, Arc<CountingResolver>) {
    let resolver = Arc::new(CountingResolver {
        addr: server.resolver_addr(),
        calls: AtomicUsize::new(0),
    });
    let guard =
        NetworkGuard::with_resolver_and_root_certificate(resolver.clone(), server.certificate());
    (WebFetch::with_guard(Arc::new(guard)), resolver)
}

fn detail(result: &ToolResult) -> &Value {
    result.detail.as_ref().expect("result detail")
}

#[tokio::test]
async fn execute_returns_extracted_text_with_full_metadata() -> TestResult {
    let paragraph = "The article body explains a concrete topic in complete sentences, with enough detail for the readability heuristic to identify this section as the primary document content. ".repeat(12);
    let html = format!(
        "<!doctype html><html><body><nav>Navigation Noise</nav><main><article><h1>Fixture Article</h1><p>{paragraph}</p><p>{paragraph}</p></article></main></body></html>"
    );
    let server = FixtureServer::start(move |_path| identity_response(html.as_bytes())).await?;
    let (tool, _resolver) = tool(&server);
    let url = server.url("/article");

    let result = tool.execute(json!({"url": url})).await?;

    assert!(!result.is_error);
    assert!(result.content.contains("Fixture Article"));
    assert!(!result.content.contains("Navigation Noise"));
    let detail = detail(&result);
    assert!(matches!(
        detail["extraction_method"].as_str(),
        Some("readability" | "fallback")
    ));
    assert_eq!(detail["redirect_count"], 0);
    assert_eq!(detail["redirect_blocked"], false);
    assert_eq!(detail["truncated"], false);
    assert_eq!(detail["final_url"], url);
    assert_eq!(detail["status_code"], 200);
    assert!(detail["decompressed_bytes"].as_u64().is_some_and(|n| n > 0));
    assert_eq!(detail["format"], "text");
    Ok(())
}

#[tokio::test]
async fn execute_html_format_returns_raw_document() -> TestResult {
    let html = b"<html><body><nav>Raw Navigation</nav><main>Article</main></body></html>";
    let server = FixtureServer::start(move |_path| identity_response(html)).await?;
    let (tool, _resolver) = tool(&server);

    let result = tool
        .execute(json!({"url": server.url("/raw"), "format": "html"}))
        .await?;

    assert!(!result.is_error);
    assert!(result.content.contains("<html"));
    assert!(result.content.contains("Raw Navigation"));
    assert_eq!(detail(&result)["extraction_method"], "raw_html");
    Ok(())
}

#[tokio::test]
async fn oversized_content_length_blocked_with_metadata() -> TestResult {
    let server = FixtureServer::start(|_path| {
        response_with_headers(&["Content-Length: 5242881".to_owned()], b"x")
    })
    .await?;
    let (tool, _resolver) = tool(&server);

    let result = tool.execute(json!({"url": server.url("/large")})).await?;

    assert!(result.is_error);
    assert_eq!(detail(&result)["error_kind"], "response_too_large");
    assert_eq!(detail(&result)["size_check"], "Content-Length");
    assert_eq!(detail(&result)["limit_bytes"], 5_242_880);
    Ok(())
}

#[tokio::test]
async fn content_length_spoof_blocked_by_streaming_check() -> TestResult {
    let server = FixtureServer::start(|_path| {
        chunked_response(std::iter::repeat_with(|| vec![b'x'; 1024 * 1024]).take(6))
    })
    .await?;
    let (tool, _resolver) = tool(&server);

    let result = tool.execute(json!({"url": server.url("/stream")})).await?;

    assert!(result.is_error);
    assert_eq!(detail(&result)["error_kind"], "response_too_large");
    assert_eq!(detail(&result)["size_check"], "streaming");
    Ok(())
}

#[tokio::test]
async fn decompression_bomb_blocked_with_metadata() -> TestResult {
    let compressed = gzip(&vec![b'x'; MAX_RESPONSE_BYTES + 1])?;
    let server = FixtureServer::start(move |_path| {
        response_with_headers(
            &[
                "Content-Encoding: gzip".to_owned(),
                format!("Content-Length: {}", compressed.len()),
            ],
            &compressed,
        )
    })
    .await?;
    let (tool, _resolver) = tool(&server);

    let result = tool.execute(json!({"url": server.url("/gzip")})).await?;

    assert!(result.is_error);
    assert_eq!(detail(&result)["error_kind"], "response_too_large");
    assert_eq!(detail(&result)["size_check"], "decompressed");
    Ok(())
}

#[tokio::test]
async fn redirect_blocked_visible_in_metadata() -> TestResult {
    let server =
        FixtureServer::start(|_path| redirect("https://169.254.169.254/latest/meta-data")).await?;
    let (tool, _resolver) = tool(&server);

    let result = tool.execute(json!({"url": server.url("/start")})).await?;

    assert!(result.is_error);
    assert_eq!(detail(&result)["error_kind"], "redirect_blocked");
    assert_eq!(detail(&result)["redirect_blocked"], true);
    Ok(())
}

#[tokio::test]
async fn output_over_50kb_truncated_with_hint() -> TestResult {
    let article_text = "あ".repeat(20_000);
    let html =
        format!("<html><body><main><article><p>{article_text}</p></article></main></body></html>");
    let server = FixtureServer::start(move |_path| identity_response(html.as_bytes())).await?;
    let (tool, _resolver) = tool(&server);

    let result = tool.execute(json!({"url": server.url("/long")})).await?;

    assert!(!result.is_error);
    assert!(result.content.len() <= 51_200);
    assert!(std::str::from_utf8(result.content.as_bytes()).is_ok());
    let detail = detail(&result);
    assert_eq!(detail["truncated"], true);
    assert!(
        detail["original_bytes"]
            .as_u64()
            .is_some_and(|n| n > 51_200)
    );
    assert_eq!(detail["truncation_hint"], TRUNCATION_HINT);
    Ok(())
}

#[tokio::test]
async fn nonexistent_body_content_length_omitted_from_metadata() -> TestResult {
    let html = b"<html><body><main>Body without length</main></body></html>";
    let server = FixtureServer::start(move |_path| response_with_headers(&[], html)).await?;
    let (tool, _resolver) = tool(&server);

    let result = tool
        .execute(json!({"url": server.url("/no-length")}))
        .await?;

    assert!(!result.is_error);
    let detail = detail(&result).as_object().expect("detail object");
    assert!(!detail.contains_key("content_length"));
    assert_eq!(detail.get("status_code"), Some(&json!(200)));
    assert_eq!(detail.get("truncated"), Some(&json!(false)));
    Ok(())
}

#[tokio::test]
async fn args_reject_unknown_selector_early() -> TestResult {
    let server = FixtureServer::start(|_path| identity_response(b"unreachable")).await?;
    let (tool, resolver) = tool(&server);

    let result = tool
        .execute(json!({"url": server.url("/never"), "selector": "[[["}))
        .await?;

    assert!(result.is_error);
    assert!(result.content.contains("invalid selector"));
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    assert!(server.captured_requests().is_empty());
    Ok(())
}

fn gzip(body: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(body)?;
    encoder.finish()
}
