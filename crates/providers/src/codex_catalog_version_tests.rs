use super::*;
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn release(tag: &str) -> Value {
    json!({"tag_name": tag, "name": "cosmetic title", "draft": false, "prerelease": false})
}

async fn mount(server: &MockServer, body: Value) {
    Mock::given(method("GET"))
        .and(path("/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .expect(1)
        .mount(server)
        .await;
}

fn resolver(server: &MockServer) -> CodexCatalogVersionResolver {
    CodexCatalogVersionResolver::new(format!("{}/releases", server.uri()))
}

#[test]
fn only_canonical_stable_rust_tags_are_eligible() {
    // Given / When / Then: titles, SDK tags, previews and malformed versions cannot win.
    for tag in [
        "0.999.0",
        "v0.999.0",
        "python-v0.999.0",
        "rusty-v8-v999.0.0",
        "rust-v0.999.0-alpha.1",
        "rust-v0.999.0+build",
        "rust-v00.999.0",
        "rust-v0.999",
        "rust-v0.999.0.1",
        "rust-v+0.999.0",
        "rust-v0.999.0\r\n",
        "rust-v18446744073709551616.0.0",
    ] {
        let r: Release = serde_json::from_value(release(tag)).unwrap();
        assert_eq!(r.stable_version(), None, "{tag}");
    }
    let r: Release = serde_json::from_value(release("rust-v0.156.1")).unwrap();
    assert_eq!(r.stable_version(), Some([0, 156, 1]));
}

#[tokio::test]
async fn selects_numeric_max_stable_cli_tag_and_caches_without_credentials() {
    // Given
    let server = MockServer::start().await;
    let mut preview = release("rust-v10.0.0");
    preview["prerelease"] = json!(true);
    let mut draft = release("rust-v11.0.0");
    draft["draft"] = json!(true);
    mount(
        &server,
        json!([
            release("rust-v0.99.9"),
            release("python-v99.0.0"),
            preview,
            draft,
            release("rust-v0.156.1"),
            release("rust-v0.156.10"),
            release("rust-v0.156.2"),
            release("rust-v99.0.0-alpha.1"),
            release("99.0.0")
        ]),
    )
    .await;
    let resolver = resolver(&server);
    // When: concurrent refreshes and a later refresh share one request.
    let (a, b) = tokio::join!(resolver.resolve(), resolver.resolve());
    let c = resolver.resolve().await;
    // Then
    assert_eq!(a.version, "0.156.10");
    assert!(a.warning.is_none());
    assert_eq!(a, b);
    assert_eq!(b, c);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].headers.contains_key("authorization"));
    assert!(!requests[0].headers.contains_key("chatgpt-account-id"));
    assert!(!requests[0].headers.contains_key("cookie"));
    assert_eq!(requests[0].headers["user-agent"], "evorch-codex-catalog");
}

#[tokio::test]
async fn skips_preview_only_pages() {
    // Given: a full page of previews before the stable release.
    let server = MockServer::start().await;
    Mock::given(query_param("page", "1"))
        .and(query_param("per_page", PAGE_SIZE.to_string()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(vec![release("rust-v1.0.0-alpha.1"); PAGE_SIZE]),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([release("rust-v0.157.0")])))
        .expect(1)
        .mount(&server)
        .await;
    // When / Then
    let result = resolver(&server).resolve().await;
    assert_eq!(result.version, "0.157.0");
    assert!(result.warning.is_none());
}

#[tokio::test]
async fn fallback_on_http_json_empty_and_transport_failures_with_retry_backoff() {
    // Given
    for response in [
        ResponseTemplate::new(403),
        ResponseTemplate::new(429),
        ResponseTemplate::new(503),
        ResponseTemplate::new(200).set_body_string("not-json"),
        ResponseTemplate::new(200).set_body_json(json!([])),
        ResponseTemplate::new(200).set_body_json(json!([{"tag_name":"rust-v99.0.0"}])),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(response)
            .expect(1)
            .mount(&server)
            .await;
        let resolver = resolver(&server);
        // When
        let a = resolver.resolve().await;
        let b = resolver.resolve().await;
        // Then
        assert_eq!(a.version, CODEX_MODELS_FALLBACK_VERSION);
        assert!(
            a.warning
                .as_ref()
                .unwrap()
                .contains("New models may be missing")
        );
        assert_eq!(a, b);
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    assert!(
        CodexCatalogVersionResolver::new(url)
            .resolve()
            .await
            .warning
            .is_some()
    );
}

#[tokio::test]
async fn refresh_failure_retains_last_success_and_retries_after_backoff() {
    // Given: expired successful cache and a temporary GitHub failure.
    let server = MockServer::start().await;
    let resolver = resolver(&server);
    *resolver.cache.lock().await = Some(CachedVersion {
        value: CodexCatalogVersion {
            version: "0.157.0".into(),
            warning: None,
        },
        checked_at: Instant::now() - SUCCESS_TTL,
    });
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    // When
    let stale = resolver.resolve().await;
    // Then: preserve latest known value, not the older bundled fallback.
    assert_eq!(stale.version, "0.157.0");
    assert!(stale.warning.is_some());
    server.reset().await;
    resolver.cache.lock().await.as_mut().unwrap().checked_at = Instant::now() - RETRY_TTL;
    mount(&server, json!([release("rust-v0.158.0")])).await;
    let refreshed = resolver.resolve().await;
    assert_eq!(refreshed.version, "0.158.0");
    assert!(refreshed.warning.is_none());
}

#[tokio::test]
async fn release_lookup_is_bounded_by_page_count_and_body_size() {
    // Given: no eligible stable release, even after multiple pages.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(vec![release("python-v99.0.0"); PAGE_SIZE]),
        )
        .expect(MAX_PAGES as u64)
        .mount(&server)
        .await;
    // When / Then
    assert!(resolver(&server).resolve().await.warning.is_some());
    server.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(" ".repeat(MAX_PAGE_BYTES + 1)))
        .expect(1)
        .mount(&server)
        .await;
    assert!(resolver(&server).resolve().await.warning.is_some());
}

#[tokio::test]
async fn old_release_cannot_regress_below_bundled_floor() {
    // Given
    let server = MockServer::start().await;
    mount(&server, json!([release("rust-v0.153.0")])).await;
    // When / Then
    assert_eq!(
        resolver(&server).resolve().await.version,
        CODEX_MODELS_FALLBACK_VERSION
    );
}
