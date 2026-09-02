//! 同一実モデルを異なるプロファイルが提供する構成の pin テスト。
//!
//! プロファイルが異なれば同一実モデルでも別候補として扱われ、解決と
//! フォールバックが宣言順に進む現行挙動を固定する。next_fallback の
//! 失敗候補一致を (プロファイル, 実モデル) の組へ変更しても、この挙動が
//! 変わらないことを担保するリファクタリングガードである。

use model::{
    Availability, CatalogCapabilities, CatalogEntry, CatalogSource, LogicalModelId, ModelCatalog,
    ProviderType,
};
use routing::{FailureKind, ProviderProfile, ResolvedRoute, Router, SessionAffinity};

fn profile(name: &str, default_model: &str) -> ProviderProfile {
    let profile_config = config::ProviderProfileConfig {
        provider_type: config::ProviderTypeConfig::OpenAi,
        api_protocol: config::ApiProtocolConfig::OpenAiResponses,
        base_url: "https://api.example.test".to_string(),
        credential: config::CredentialRefConfig::Env {
            var: "API_KEY".to_string(),
        },
        models: vec![default_model.to_string()],
        default_model: default_model.to_string(),
    };
    ProviderProfile::try_from((name, &profile_config)).expect("有効な設定は変換できる")
}

fn candidate(profile: &str, model: Option<&str>) -> config::RouteCandidateConfig {
    config::RouteCandidateConfig {
        profile: profile.to_string(),
        model: model.map(str::to_string),
    }
}

fn routing_config(routes: &[(&str, Vec<config::RouteCandidateConfig>)]) -> config::RoutingConfig {
    config::RoutingConfig {
        routes: routes
            .iter()
            .map(|(logical, candidates)| (logical.to_string(), candidates.clone()))
            .collect(),
    }
}

fn catalog_entry(model_id: &str) -> CatalogEntry {
    CatalogEntry {
        model_id: model_id.to_string(),
        provider: ProviderType::OpenAi,
        context_window: 64_000,
        max_output_tokens: 8_000,
        capabilities: CatalogCapabilities {
            tool_calling: true,
            reasoning: false,
            prompt_cache: false,
        },
        price: None,
        availability: Availability::Available,
        source: CatalogSource::Builtin,
        attributes_confirmed: true,
    }
}

fn catalog(model_ids: &[&str]) -> ModelCatalog {
    let mut catalog = ModelCatalog::builtin();
    for model_id in model_ids {
        catalog.merge_models_dev(vec![catalog_entry(model_id)]);
    }
    catalog
}

// Given: 同一実モデルを既定モデルに持つ 2 プロファイルを宣言順に並べたルート
// When: ピンなしで解決する / プロファイル b を明示ピンして解決する
// Then: それぞれのプロファイルが独立した候補として解決され、実モデルは同一のまま
#[test]
fn resolve_treats_same_model_different_profiles_as_separate_candidates() {
    let profiles = vec![profile("a", "model-shared"), profile("b", "model-shared")];
    let routing = routing_config(&[("summary", vec![candidate("a", None), candidate("b", None)])]);
    let catalog = catalog(&["model-shared"]);
    let router =
        Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");

    let mut affinity = SessionAffinity::default();
    let resolved = router
        .resolve(&mut affinity, "session-1", &LogicalModelId::from("summary"))
        .expect("宣言順先頭の候補へ解決できる");
    assert_eq!(
        (resolved.profile, resolved.model_id),
        ("a".to_string(), "model-shared".to_string()),
        "同一実モデルでも宣言順先頭のプロファイルが選ばれる"
    );
    assert_eq!(
        affinity.pinned("session-1", "summary"),
        Some("a"),
        "解決勝者のプロファイルがピンされる"
    );

    let mut affinity = SessionAffinity::default();
    affinity.pin("session-2", "summary", "b");
    let resolved = router
        .resolve(&mut affinity, "session-2", &LogicalModelId::from("summary"))
        .expect("ピン先プロファイルでも同一実モデルで解決できる");
    assert_eq!(
        (resolved.profile, resolved.model_id),
        ("b".to_string(), "model-shared".to_string()),
        "プロファイル b も同一実モデルの独立した候補として解決できる"
    );
}

// Given: 同一実モデルを既定モデルに持つ 2 プロファイルを宣言順に並べたルート
// When: 先頭プロファイルを失敗ルートとしてフォールバックする
// Then: 宣言順どおり次のプロファイルへ進み、実モデルは同一のまま
#[test]
fn next_fallback_advances_to_same_model_on_other_profile_in_declared_order() {
    let profiles = vec![profile("a", "model-shared"), profile("b", "model-shared")];
    let routing = routing_config(&[("summary", vec![candidate("a", None), candidate("b", None)])]);
    let catalog = catalog(&["model-shared"]);
    let router =
        Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");
    let mut affinity = SessionAffinity::default();

    let fallback = router
        .next_fallback(
            &mut affinity,
            "session-1",
            &LogicalModelId::from("summary"),
            &ResolvedRoute {
                profile: "a".to_string(),
                model_id: "model-shared".to_string(),
            },
            FailureKind::Server,
            None,
        )
        .expect("同一実モデルの別プロファイル候補へフォールバックできる");

    assert_eq!(
        (fallback.profile, fallback.model_id),
        ("b".to_string(), "model-shared".to_string()),
        "宣言順に次のプロファイルへ進み、実モデルは同一"
    );
    assert_eq!(
        affinity.pinned("session-1", "summary"),
        Some("b"),
        "フォールバック先が再ピンされる"
    );
}
