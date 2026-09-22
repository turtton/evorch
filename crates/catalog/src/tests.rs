use super::*;
use std::path::Path;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIXTURE: &str = include_str!("fixture.json");

fn seed(dir: &Path, fetched_at: u64) {
    let api: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
    std::fs::write(
        dir.join("models-dev.json"),
        serde_json::to_vec(&serde_json::json!({"fetched_at": fetched_at, "api": api})).unwrap(),
    )
    .unwrap();
}

fn catalog_with_models(json: &str) -> ModelCatalog {
    ModelCatalog {
        api: serde_json::from_str(json).unwrap(),
        fetched_at: 0,
        refresh: None,
    }
}

async fn server(status: u16, count: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(ResponseTemplate::new(status).set_body_raw(FIXTURE, "application/json"))
        .expect(count)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn fresh_cache_is_used_without_fetch() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), now());
    let server = server(200, 0).await;

    let catalog = ModelCatalog::load_with(dir.path(), &format!("{}/api.json", server.uri()), false)
        .await
        .unwrap();

    assert!(catalog.refresh.is_none());
    let model = catalog.find("openai", "gpt-4o").unwrap();
    assert_eq!(model.context_window, Some(128_000));
    assert_eq!(model.input_price, Some(2.5));
}

#[tokio::test]
async fn stale_cache_triggers_refresh_but_returns_stale() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), 0);
    let server = server(200, 1).await;

    let mut catalog =
        ModelCatalog::load_with(dir.path(), &format!("{}/api.json", server.uri()), false)
            .await
            .unwrap();

    assert_eq!(catalog.fetched_at, 0);
    let updated = catalog.refresh.take().unwrap().await.unwrap().unwrap();
    assert!(updated.fetched_at > 0);
    assert!(cache::read(dir.path()).await.unwrap().is_fresh());
}

#[tokio::test]
async fn force_refresh_overwrites_cache_atomically() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), 0);
    let old_file = std::fs::File::open(dir.path().join("models-dev.json")).unwrap();
    let server = server(200, 1).await;

    let catalog = ModelCatalog::load_with(dir.path(), &format!("{}/api.json", server.uri()), true)
        .await
        .unwrap();

    assert!(catalog.fetched_at > 0);
    let old: serde_json::Value = serde_json::from_reader(old_file).unwrap();
    assert_eq!(old["fetched_at"], 0);
    assert!(cache::read(dir.path()).await.unwrap().is_fresh());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[tokio::test]
async fn fetch_failure_falls_back_to_stale_cache() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), 0);
    let server = server(503, 1).await;

    let catalog = ModelCatalog::load_with(dir.path(), &format!("{}/api.json", server.uri()), true)
        .await
        .unwrap();

    assert_eq!(catalog.fetched_at, 0);
    let model = catalog.find("openai", "gpt-4o").unwrap();
    assert_eq!(model.context_window, Some(128_000));
    assert_eq!(model.input_price, Some(2.5));
}

#[tokio::test]
async fn fetch_failure_without_cache_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let server = server(503, 1).await;

    let result =
        ModelCatalog::load_with(dir.path(), &format!("{}/api.json", server.uri()), false).await;

    assert!(matches!(result, Err(CatalogError::Http(_))));
    assert!(!dir.path().join("models-dev.json").exists());
}

#[tokio::test]
async fn find_returns_context_window_and_pricing() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), now());

    let catalog = ModelCatalog::load_or_refresh(dir.path()).await.unwrap();

    let model = catalog.find("openai", "gpt-4o").unwrap();
    assert_eq!(model.context_window, Some(128_000));
    assert_eq!(model.max_input_tokens, None);
    assert_eq!(model.max_output_tokens, Some(16_384));
    assert_eq!(model.input_price, Some(2.5));
    assert_eq!(model.output_price, Some(10.0));
    assert_eq!(model.tool_call, Some(true));
    assert_eq!(model.modalities.input, ["text", "image", "pdf"]);
    assert_eq!(
        catalog.find_by_model_id("gpt-4o-mini").unwrap().input_price,
        Some(0.15)
    );
}

#[tokio::test]
async fn unknown_model_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), now());

    let catalog = ModelCatalog::load_or_refresh(dir.path()).await.unwrap();

    assert!(catalog.find("openai", "missing").is_none());
    assert!(catalog.find("missing", "gpt-4o").is_none());
    assert!(catalog.find_by_model_id("missing").is_none());
}

#[test]
fn agreeing_duplicate_provider_limits_accept() {
    // Given: 二つのプロバイダが同じ制限値の k3 を持つ。
    let catalog = catalog_with_models(
        r#"{
            "kimi-code-plan-global":{"models":{"k3":{"id":"k3","limit":{"context":1048576,"output":65536}}}},
            "kimi-code-plan-cn":{"models":{"k3":{"id":"k3","limit":{"context":1048576,"output":65536}}}}
        }"#,
    );

    // When: k3 を制限値一致ルールで検索する。
    let model = catalog.find_agreeing_model("k3");

    // Then: 一致する重複は最初のプロバイダのエントリを返す。
    assert_eq!(model.map(|model| model.context_window), Some(Some(1048576)));
}

#[test]
fn disagreeing_duplicate_provider_limits_rejected() {
    // Given: 二つのプロバイダの k3 が異なるコンテキスト制限を持つ。
    let catalog = catalog_with_models(
        r#"{
            "kimi-code-plan-global":{"models":{"k3":{"id":"k3","limit":{"context":1048576,"output":65536}}}},
            "kimi-code-plan-cn":{"models":{"k3":{"id":"k3","limit":{"context":131072,"output":65536}}}}
        }"#,
    );

    // When: k3 を制限値一致ルールで検索する。
    let model = catalog.find_agreeing_model("k3");

    // Then: コンテキスト制限が異なる重複は拒否する。
    assert!(model.is_none());
}

#[test]
fn duplicate_agreement_compares_output_limit_too() {
    // Given: 二つのプロバイダの k3 が同じコンテキスト制限と異なる出力制限を持つ。
    let catalog = catalog_with_models(
        r#"{
            "kimi-code-plan-global":{"models":{"k3":{"id":"k3","limit":{"context":1048576,"output":65536}}}},
            "kimi-code-plan-cn":{"models":{"k3":{"id":"k3","limit":{"context":1048576,"output":32768}}}}
        }"#,
    );

    // When: k3 を制限値一致ルールで検索する。
    let model = catalog.find_agreeing_model("k3");

    // Then: 出力制限が異なる重複も拒否する。
    assert!(model.is_none());
}
