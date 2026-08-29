//! ピン → 解決 → フォールバック → 再解決の一連の流れを公開 API 経由で検証します。

use model::{
    Availability, CatalogCapabilities, CatalogEntry, CatalogSource, LogicalModelId, ModelCatalog,
    ProviderType,
};
use routing::{FailureKind, ProviderProfile, Router, SessionAffinity};

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

fn candidate(profile: &str) -> config::RouteCandidateConfig {
    config::RouteCandidateConfig {
        profile: profile.to_string(),
        model: None,
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

// Given: 2 候補 (primary, secondary) を宣言順に持つルーティング構成とピンなしセッション
// When: 解決してピンされたプロファイルを失敗扱いでフォールバックし、再度解決する
// Then: 次候補が返って再ピンされ、再解決は再ピンされたフォールバック先を返す
#[test]
fn fallback_after_pin_resolve_roundtrip() {
    let profiles = vec![
        profile("primary", "model-a"),
        profile("secondary", "model-b"),
    ];
    let routing = config::RoutingConfig {
        routes: [(
            "summary".to_string(),
            vec![candidate("primary"), candidate("secondary")],
        )]
        .into_iter()
        .collect(),
    };
    let catalog = catalog(&["model-a", "model-b"]);
    let router =
        Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");
    let mut affinity = SessionAffinity::default();

    let resolved = router
        .resolve(&mut affinity, "session-1", &LogicalModelId::from("summary"))
        .expect("宣言順先頭の候補へ解決できる");
    assert_eq!(resolved.profile, "primary");
    assert_eq!(
        affinity.pinned("session-1", "summary"),
        Some("primary"),
        "解決勝者がピンされる"
    );

    let fallback = router
        .next_fallback(
            &mut affinity,
            "session-1",
            &LogicalModelId::from("summary"),
            resolved.profile.as_str(),
            FailureKind::Server,
        )
        .expect("失敗プロファイルの次候補へフォールバックできる");
    assert_eq!(fallback.profile, "secondary");
    assert_eq!(
        affinity.pinned("session-1", "summary"),
        Some("secondary"),
        "フォールバック先が再ピンされる"
    );

    let resolved_again = router
        .resolve(&mut affinity, "session-1", &LogicalModelId::from("summary"))
        .expect("再ピンされたプロファイルで解決できる");
    assert_eq!(resolved_again.profile, "secondary");
    assert_eq!(resolved_again.model_id, "model-b");
}
