//! `web_fetch` の E2E 抽出品質テスト: MDN 風 / rustdoc 風 fixture を
//! ローカル TLS サーバで配信し、実ネットワークなしで抽出チェーンを検証する (AC3)。

mod common;

use std::{net::IpAddr, sync::Arc};

use async_trait::async_trait;
use serde_json::{Value, json};
use tools::{DnsResolver, NetworkGuard, NetworkGuardError, Tool, ToolResult, WebFetch};

use common::{FixtureServer, TestResult};

const MDN_LIKE: &str = include_str!("fixtures/mdn_like.html");
const RUST_DOCS_LIKE: &str = include_str!("fixtures/rust_docs_like.html");

const MDN_ARTICLE_SENTENCE: &str = "flatMap first maps each element using a mapping function, then flattens the result into a new array one level deep.";
const MDN_CODE_FRAGMENT: &str = "async function loadExamples(baseUrl)";
const MDN_INTRO_SENTENCE: &str = "returns a new array formed by applying a given callback function to each element of the array, and then flattening the result by one level";
const RUST_DOCBLOCK_SENTENCE: &str = "A rotation matrix in three dimensions preserves handedness and has a determinant of exactly one.";

struct LocalResolver {
    addr: IpAddr,
}

#[async_trait]
impl DnsResolver for LocalResolver {
    async fn resolve(&self, _host: &str) -> Result<Vec<IpAddr>, NetworkGuardError> {
        Ok(vec![self.addr])
    }
}

fn identity_response(body: &[u8]) -> Vec<u8> {
    common::response_with_status("200 OK", &[format!("Content-Length: {}", body.len())], body)
}

async fn fixture_tool(
    body: &'static str,
) -> Result<(FixtureServer, WebFetch), Box<dyn std::error::Error>> {
    let server = FixtureServer::start(move |_| identity_response(body.as_bytes())).await?;
    let guard = NetworkGuard::with_resolver_and_root_certificate(
        Arc::new(LocalResolver {
            addr: server.resolver_addr(),
        }),
        server.certificate(),
    );
    Ok((server, WebFetch::with_guard(Arc::new(guard))))
}

fn detail(result: &ToolResult) -> &Value {
    result.detail.as_ref().expect("result detail")
}

#[tokio::test]
async fn mdn_like_fixture_extracts_main_article_text() -> TestResult {
    let (server, tool) = fixture_tool(MDN_LIKE).await?;

    let result = tool
        .execute(json!({
            "url": server.url("/en-US/docs/Web/JavaScript/Reference/Global_Objects/Array/flatMap")
        }))
        .await?;

    assert!(!result.is_error);
    assert_eq!(detail(&result)["extraction_method"], "readability");
    assert_eq!(detail(&result)["format"], "text");
    assert!(result.content.contains(MDN_ARTICLE_SENTENCE));
    assert!(result.content.contains(MDN_CODE_FRAGMENT));
    assert!(!result.content.contains("MDN Docs Home"));
    assert!(!result.content.contains("Skip to main content"));
    assert!(!result.content.contains("In this article"));
    assert!(!result.content.contains("Report a content issue"));
    assert_eq!(server.captured_requests().len(), 1);
    Ok(())
}

#[tokio::test]
async fn mdn_like_fixture_markdown_output() -> TestResult {
    let (server, tool) = fixture_tool(MDN_LIKE).await?;

    let result = tool
        .execute(json!({ "url": server.url("/mdn"), "format": "markdown" }))
        .await?;

    assert!(!result.is_error);
    assert_eq!(detail(&result)["extraction_method"], "readability");
    assert_eq!(detail(&result)["format"], "markdown");
    assert!(result.content.contains("## Syntax"));
    assert!(result.content.contains("```"));
    assert!(result.content.contains(MDN_CODE_FRAGMENT));
    assert!(!result.content.contains("In this article"));
    Ok(())
}

#[tokio::test]
async fn rust_docs_like_fixture_extracts_docblock() -> TestResult {
    let (server, tool) = fixture_tool(RUST_DOCS_LIKE).await?;

    let result = tool
        .execute(json!({
            "url": server.url("/keisan_core/struct.RotationMatrix.html")
        }))
        .await?;

    assert!(!result.is_error);
    assert_eq!(detail(&result)["extraction_method"], "readability");
    assert!(result.content.contains(RUST_DOCBLOCK_SENTENCE));
    assert!(!result.content.contains("mod quaternion"));
    Ok(())
}

#[tokio::test]
async fn selector_narrows_mdn_fixture() -> TestResult {
    let (server, tool) = fixture_tool(MDN_LIKE).await?;

    let result = tool
        .execute(json!({
            "url": server.url("/mdn"),
            "selector": "article p.intro"
        }))
        .await?;

    assert!(!result.is_error);
    assert_eq!(detail(&result)["extraction_method"], "selector");
    assert!(result.content.contains(MDN_INTRO_SENTENCE));
    assert!(!result.content.contains(MDN_ARTICLE_SENTENCE));
    Ok(())
}

// AC3 smoke: 実ネットワークは既定 run で禁止のため repo 慣例どおり #[ignore]（--ignored で明示実行）。
#[tokio::test]
#[ignore = "実ネットワークアクセスを伴うため --ignored で明示実行する"]
async fn live_docs_fetch_smoke() -> TestResult {
    let tool = WebFetch::new().expect("web_fetch tool");

    let result = tool
        .execute(json!({
            "url": "https://doc.rust-lang.org/std/string/struct.String.html"
        }))
        .await?;

    assert!(!result.is_error);
    assert!(result.content.contains("growable string"));
    assert!(matches!(
        detail(&result)["extraction_method"].as_str(),
        Some("readability" | "fallback")
    ));
    Ok(())
}
