use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn catalog(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .respond_with(response)
        .mount(server)
        .await;
}
// A received request, not elapsed time, gates cancellation and completion tests.
async fn received_get(stream: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncReadExt;
    let mut headers = Vec::new();
    let mut buffer = [0; 4096];
    while !headers.ends_with(b"\r\n\r\n") {
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0, "connection closed before request headers");
        headers.extend_from_slice(&buffer[..count]);
    }
    assert!(headers.starts_with(b"GET "));
}
fn success() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"models": [{"slug": "model"}]}))
}

#[tokio::test]
async fn catalog_ttl_is_absolute_and_caches_are_independent() {
    let server = MockServer::start().await;
    catalog(&server, success()).await;
    let ticks = Arc::new(AtomicU64::new(0));
    let clock = ticks.clone();
    let cache = CodexCatalogCache::with_clock(Arc::new(move || {
        Duration::from_secs(clock.load(Ordering::Relaxed))
    }));
    let auth = ProviderAuth::new("secret-token");
    assert!(!cache.fetch(&server.uri(), &auth, "account").await.1);
    ticks.store(299, Ordering::Relaxed);
    assert!(cache.fetch(&server.uri(), &auth, "account").await.1);
    ticks.store(300, Ordering::Relaxed);
    assert!(!cache.fetch(&server.uri(), &auth, "account").await.1);
    assert!(
        !CodexCatalogCache::new()
            .fetch(&server.uri(), &auth, "account")
            .await
            .1
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
    assert!(!format!("{cache:?}").contains("secret-token"));
}

#[tokio::test]
async fn catalog_failures_and_empty_results_only_share_the_current_flight() {
    for response in [
        ResponseTemplate::new(500).set_body_string("down"),
        ResponseTemplate::new(200).set_body_string("not-json"),
        ResponseTemplate::new(200).set_body_json(json!({"models": []})),
    ] {
        let server = MockServer::start().await;
        catalog(&server, response).await;
        let cache = CodexCatalogCache::new();
        let auth = ProviderAuth::new("token");
        let base = server.uri();
        let (a, b) = tokio::join!(
            cache.fetch(&base, &auth, "account"),
            cache.fetch(&base, &auth, "account")
        );
        assert!(!a.1);
        assert!(b.1);
        assert_eq!(format!("{:?}", a.0), format!("{:?}", b.0));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        // Inspect before reset: reset clears wiremock's recorded requests, not our cache.
        server.reset().await;
        catalog(&server, success()).await;
        let (result, hit) = cache.fetch(&base, &auth, "account").await;
        assert_eq!(result.unwrap()[0].slug, "model");
        assert!(!hit);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn catalog_keys_separate_url_token_and_account_and_capacity_never_evicts_flights() {
    let server = MockServer::start().await;
    catalog(&server, success()).await;
    let cache = CodexCatalogCache::new();
    let auth = ProviderAuth::new("token");
    let base = server.uri();
    for (url, token, account) in [
        (&base, "token", "account"),
        (&base, "new-token", "account"),
        (&base, "token", "new-account"),
        (&format!("{base}/other"), "token", "account"),
    ] {
        assert!(!cache.fetch(url, &ProviderAuth::new(token), account).await.1);
    }
    // Keep a flight registered while completion capacity is exceeded.
    let flight_key = Key::new(&base, &auth, "in-flight");
    let flight = Arc::new(Flight::default());
    cache
        .registry
        .lock()
        .unwrap()
        .flights
        .insert(flight_key.clone(), flight.clone());
    for i in 0..CAPACITY {
        assert!(!cache.fetch(&base, &auth, &format!("account-{i}")).await.1);
    }
    {
        let registry = cache.registry.lock().unwrap();
        assert_eq!(registry.completed.len(), CAPACITY);
        assert!(Arc::ptr_eq(&registry.flights[&flight_key], &flight));
    }
    assert!(
        !cache.fetch(&base, &auth, "account").await.1,
        "oldest completion evicted"
    );
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        4 + CAPACITY + 1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_owner_drop_hands_off_to_a_waiter_on_another_runtime() {
    use tokio::io::AsyncWriteExt;
    use tokio::sync::oneshot;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        received_get(&mut first).await;
        started_tx.send(()).unwrap();
        let (mut second, _) = listener.accept().await.unwrap();
        received_get(&mut second).await;
        let body = r#"{"models":[{"slug":"model"}]}"#;
        second
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let cache = Arc::new(CodexCatalogCache::new());
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let c = cache.clone();
    let b = base.clone();
    let owner = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let auth = ProviderAuth::new("token");
                tokio::select! {
                    _ = c.fetch(&b, &auth, "account") => panic!("owner must be cancelled"),
                    _ = cancel_rx => {}
                }
            });
    });
    started_rx.await.unwrap();
    let (registered_tx, registered_rx) = oneshot::channel();
    let c = cache.clone();
    let b = base.clone();
    let waiter = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let auth = ProviderAuth::new("token");
                let fetch = c.fetch(&b, &auth, "account");
                tokio::pin!(fetch);
                assert!(futures_util::poll!(fetch.as_mut()).is_pending());
                registered_tx.send(()).unwrap(); // Explicitly registered with the existing flight.
                fetch.await
            })
    });
    registered_rx.await.unwrap();
    cancel_tx.send(()).unwrap();
    owner.join().unwrap(); // Drops its runtime as well as its HTTP future.
    server.await.unwrap();
    let (models, hit) = waiter.join().unwrap();
    assert_eq!(models.unwrap()[0].slug, "model");
    assert!(!hit);
    assert!(
        cache
            .fetch(&base, &ProviderAuth::new("token"), "account")
            .await
            .1
    );
    assert!(cache.registry.lock().unwrap().flights.is_empty());
}

#[tokio::test]
async fn catalog_old_flight_completion_does_not_overwrite_a_new_token_key() {
    use tokio::io::AsyncWriteExt;
    use tokio::sync::oneshot;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut old, _) = listener.accept().await.unwrap();
        received_get(&mut old).await;
        started_tx.send(()).unwrap();
        let (mut new, _) = listener.accept().await.unwrap();
        received_get(&mut new).await;
        let body = r#"{"models":[{"slug":"new"}]}"#;
        new.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        release_rx.await.unwrap();
        let body = r#"{"models":[{"slug":"old"}]}"#;
        old.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    });
    let cache = Arc::new(CodexCatalogCache::new());
    let c = cache.clone();
    let b = base.clone();
    let old = tokio::spawn(async move {
        c.fetch(&b, &ProviderAuth::new("old-token"), "account")
            .await
    });
    started_rx.await.unwrap();
    let new = cache
        .fetch(&base, &ProviderAuth::new("new-token"), "account")
        .await;
    assert_eq!(new.0.unwrap()[0].slug, "new");
    assert!(!new.1);
    release_tx.send(()).unwrap();
    assert_eq!(old.await.unwrap().0.unwrap()[0].slug, "old");
    server.await.unwrap();
    let (models, hit) = cache
        .fetch(&base, &ProviderAuth::new("new-token"), "account")
        .await;
    assert_eq!(models.unwrap()[0].slug, "new");
    assert!(hit);
}

#[tokio::test]
async fn catalog_expiry_never_serves_stale_success_on_failure() {
    let server = MockServer::start().await;
    catalog(&server, success()).await;
    let ticks = Arc::new(AtomicU64::new(0));
    let clock = ticks.clone();
    let cache = CodexCatalogCache::with_clock(Arc::new(move || {
        Duration::from_secs(clock.load(Ordering::Relaxed))
    }));
    let auth = ProviderAuth::new("private-access");
    cache
        .fetch(&server.uri(), &auth, "account")
        .await
        .0
        .unwrap();
    ticks.store(300, Ordering::Relaxed);
    server.reset().await;
    catalog(
        &server,
        ResponseTemplate::new(500).set_body_string("echo private-access"),
    )
    .await;
    let (result, hit) = cache.fetch(&server.uri(), &auth, "account").await;
    assert!(!hit);
    assert!(matches!(
        &result,
        Err(ProviderError::Http { status: 500, .. })
    ));
    assert!(!format!("{result:?}").contains("private-access"));
    assert!(cache.registry.lock().unwrap().completed.is_empty());
}
