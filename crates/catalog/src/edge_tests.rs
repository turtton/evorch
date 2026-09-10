use super::*;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIXTURE: &str = include_str!("fixture.json");

fn seed(dir: &Path) {
    std::fs::write(
        dir.join("models-dev.json"),
        format!("{{\"fetched_at\":0,\"api\":{FIXTURE}}}"),
    )
    .unwrap();
}

#[tokio::test]
async fn held_file_lock_returns_stale_without_fetch() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let lock = cache::lock(dir.path()).await.unwrap();
    let server = MockServer::start().await;

    let catalog = ModelCatalog::load_with(dir.path(), &server.uri(), false)
        .await
        .unwrap();

    assert!(catalog.refresh.is_none());
    assert_eq!(catalog.fetched_at, 0);
    assert!(server.received_requests().await.unwrap().is_empty());
    drop(lock);
    assert!(cache::lock(dir.path()).await.is_ok());
}

#[tokio::test]
async fn background_failure_preserves_cache_and_releases_lock() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;

    let mut catalog = ModelCatalog::load_with(dir.path(), &server.uri(), false)
        .await
        .unwrap();
    let result = catalog.take_refresh().unwrap().await.unwrap();

    assert!(matches!(result, Err(CatalogError::Http(_))));
    assert_eq!(cache::read(dir.path()).await.unwrap().fetched_at, 0);
    assert!(cache::lock(dir.path()).await.is_ok());
}

#[tokio::test]
async fn malformed_response_does_not_replace_cache() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let original = std::fs::read(dir.path().join("models-dev.json")).unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{broken"))
        .expect(1)
        .mount(&server)
        .await;

    let catalog = ModelCatalog::load_with(dir.path(), &server.uri(), true)
        .await
        .unwrap();

    assert_eq!(catalog.fetched_at, 0);
    assert_eq!(
        std::fs::read(dir.path().join("models-dev.json")).unwrap(),
        original
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[tokio::test]
async fn corrupt_cache_is_replaced_by_successful_fetch() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("models-dev.json"), "broken").unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE))
        .expect(1)
        .mount(&server)
        .await;

    let catalog = ModelCatalog::load_with(dir.path(), &server.uri(), false)
        .await
        .unwrap();

    assert!(catalog.find("openai", "gpt-4o").is_some());
    assert!(cache::read(dir.path()).await.unwrap().is_fresh());
}

#[test]
fn duplicate_model_ids_use_lexicographic_provider_order() {
    let api = serde_json::from_str(r#"{
        "z-provider":{"models":{"same":{"id":"same","limit":{"context":200}}}},
        "a-provider":{"models":{"same":{"id":"same","limit":{"context":100,"input":90},"future_field":true}}}
    }"#).unwrap();
    let catalog = ModelCatalog {
        api,
        fetched_at: 0,
        refresh: None,
    };

    let model = catalog.find_by_model_id("same").unwrap();

    assert_eq!(model.context_window, Some(100));
    assert_eq!(model.max_input_tokens, Some(90));
    assert_eq!(model.output_price, None);
}

#[test]
fn ttl_expires_at_twenty_four_hours_and_rejects_future_timestamp() {
    let mut catalog = ModelCatalog {
        api: BTreeMap::new(),
        fetched_at: now() - TTL.as_secs(),
        refresh: None,
    };

    assert!(!catalog.is_fresh());
    catalog.fetched_at = now() + TTL.as_secs();
    assert!(!catalog.is_fresh());
}
