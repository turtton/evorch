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

#[test]
fn canonical_model_id_is_read_and_blank_ids_are_absent() {
    // Given: two carriers name the same model differently, and one entry has a blank id.
    let catalog = catalog_with_models(
        r#"{
            "kimi-code-plan-global": {"models": {
                "k3": {"id": "k3", "canonical_model_id": "moonshotai/kimi-k3"},
                "kimi-for-coding": {"id": "kimi-for-coding", "canonical_model_id": " "}
            }},
            "neuralwatt": {"models": {"kimi-k3": {"id": "kimi-k3", "canonical_model_id": "moonshotai/kimi-k3"}}}
        }"#,
    );
    // When / Then: both carriers share the canonical id; a blank one is treated as unknown.
    let canonical = |provider, model| {
        catalog
            .find(provider, model)
            .and_then(|metadata| metadata.canonical_model_id.as_deref())
    };
    assert_eq!(
        canonical("kimi-code-plan-global", "k3"),
        Some("moonshotai/kimi-k3")
    );
    assert_eq!(
        canonical("neuralwatt", "kimi-k3"),
        Some("moonshotai/kimi-k3")
    );
    assert_eq!(canonical("kimi-code-plan-global", "kimi-for-coding"), None);
}

#[test]
fn family_is_read_and_blank_families_are_absent() {
    // Given: one model with a family and one with a blank family.
    let catalog = catalog_with_models(
        r#"{"anthropic": {"models": {
            "claude-sonnet-9": {"id": "claude-sonnet-9", "family": "claude-sonnet"},
            "claude-x": {"id": "claude-x", "family": ""}
        }}}"#,
    );

    // When / Then: the family is kept, a blank one is unknown, and the fetch time is exposed.
    let family = |model| {
        catalog
            .find("anthropic", model)
            .and_then(|metadata| metadata.family.as_deref())
    };
    assert_eq!(family("claude-sonnet-9"), Some("claude-sonnet"));
    assert_eq!(family("claude-x"), None);
    assert_eq!(catalog.fetched_at(), UNIX_EPOCH);
}

#[test]
fn context_tiers_and_fast_mode_prices_are_read() {
    // Given: models.dev pricing with a context tier, an unknown tier type, an unreadable
    // tier, a legacy over-200k object and fast-mode prices with their own tier.
    let catalog = catalog_with_models(
        r#"{"openai": {"models": {"gpt-6-astra": {
            "id": "gpt-6-astra",
            "cost": {
                "input": 10, "output": 50, "cache_read": 1, "cache_write": 12.5,
                "tiers": [
                    {"input": 20, "output": 75, "cache_read": 2, "cache_write": 25,
                     "tier": {"type": "context", "size": 272000}},
                    {"input": 99, "tier": {"type": "time_of_day", "size": 1}},
                    {"input": 99, "tier": {"type": "context", "size": "large"}}
                ],
                "context_over_200k": {"input": 99, "output": 99}
            },
            "experimental": {"modes": {
                "fast": {
                    "cost": {"input": 20, "output": 100, "cache_read": 2,
                             "tiers": [{"input": 40, "tier": {"type": "context", "size": 272000}}]},
                    "provider": {"body": {"service_tier": "priority"}}
                },
                "pro": {"provider": {"body": {"reasoning": {"mode": "pro"}}}}
            }}
        }}}}"#,
    );

    // When
    let model = catalog.find("openai", "gpt-6-astra").unwrap();

    // Then: only readable context tiers are kept, and fast prices stay separate.
    assert_eq!(
        model.price_tiers,
        [PriceTier {
            above_input_tokens: 272_000,
            prices: Prices {
                input: Some(20.0),
                output: Some(75.0),
                cache_read: Some(2.0),
                cache_write: Some(25.0),
            },
        }]
    );
    assert_eq!(
        model.fast_pricing,
        Some(ModePricing {
            prices: Prices {
                input: Some(20.0),
                output: Some(100.0),
                cache_read: Some(2.0),
                cache_write: None,
            },
            tiers: vec![PriceTier {
                above_input_tokens: 272_000,
                prices: Prices {
                    input: Some(40.0),
                    ..Prices::default()
                },
            }],
        })
    );
    assert_eq!(model.input_price, Some(10.0));
}

#[test]
fn modes_without_fast_cost_have_no_fast_pricing() {
    // Given: experimental modes that carry no fast cost, and an unreadable fast cost.
    let catalog = catalog_with_models(
        r#"{"openai": {"models": {
            "pro-only": {"id": "pro-only", "experimental": {"modes": {"pro": {}}}},
            "odd": {"id": "odd", "experimental": {"modes": {"fast": {"cost": "free"}}}}
        }}}"#,
    );

    // When / Then: the catalog still loads and neither model has fast prices.
    for id in ["pro-only", "odd"] {
        assert_eq!(catalog.find("openai", id).unwrap().fast_pricing, None);
    }
}
