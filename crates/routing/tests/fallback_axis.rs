//! FallbackTriggered イベントが失敗前後の (プロファイル, 実モデル) 組を保持し、
//! FallbackAxis による変化軸の分類ができることを公開 API 経由で検証します。

use std::sync::Arc;

use event_bus::{EventBus, EventKind, FallbackAxis, ProviderEvent};
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

fn router_with_bus(
    profiles: Vec<ProviderProfile>,
    routing: &config::RoutingConfig,
    catalog: ModelCatalog,
) -> (Router, Arc<EventBus>, event_bus::EventReceiver) {
    let bus = Arc::new(EventBus::new(8));
    let rx = bus.subscribe();
    let router = Router::new(profiles, routing, catalog)
        .expect("有効な構成で Router を構築できる")
        .with_event_bus(Some(bus.clone()));
    (router, bus, rx)
}

// Given: 異なる既定モデルを持つ 2 プロファイルを宣言順に並べ、EventBus を接続したルート
// When: 先頭候補を失敗ルートとしてフォールバックする
// Then: FallbackTriggered が from_provider / from_model / to_provider / to_model の
//       4 つすべてを保持し、1 件だけ発行される
#[tokio::test]
async fn fallback_event_retains_failed_and_selected_profile_model_identity() {
    let profiles = vec![profile("a", "model-a"), profile("b", "model-b")];
    let routing = routing_config(&[("summary", vec![candidate("a", None), candidate("b", None)])]);
    let catalog = catalog(&["model-a", "model-b"]);
    let (router, _bus, mut rx) = router_with_bus(profiles, &routing, catalog);
    let mut affinity = SessionAffinity::default();

    let resolved = router
        .next_fallback(
            &mut affinity,
            "session-1",
            &LogicalModelId::from("summary"),
            &ResolvedRoute {
                profile: "a".to_string(),
                model_id: "model-a".to_string(),
            },
            FailureKind::Server,
            None,
        )
        .expect("次候補へフォールバックできる");
    assert_eq!(
        resolved,
        ResolvedRoute {
            profile: "b".to_string(),
            model_id: "model-b".to_string(),
        }
    );

    let event = rx.recv().await.expect("イベントを受信できる");
    let EventKind::Provider(ProviderEvent::FallbackTriggered {
        from_provider,
        from_model,
        to_provider,
        to_model,
        ..
    }) = event.kind
    else {
        panic!("FallbackTriggered イベントを期待しました: {:?}", event.kind);
    };
    assert_eq!(from_provider, "a");
    assert_eq!(from_model, Some("model-a".to_string()));
    assert_eq!(to_provider, "b");
    assert_eq!(to_model, "model-b");

    let second = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
    assert!(
        second.is_err(),
        "フォールバックイベントは選択 1 回につき 1 件"
    );
}

// Given: 変化軸が Provider / Model / Both になる 3 つのフォールバック構成。
// When: それぞれフォールバックしてイベントを受信し、イベントフィールドから
//       FallbackAxis::classify で変化軸を分類する。
// Then: それぞれ Provider / Model / Both に分類される。
#[tokio::test]
async fn fallback_event_distinguishes_provider_model_and_both_axes() {
    struct AxisCase {
        label: &'static str,
        profiles: Vec<ProviderProfile>,
        routing: config::RoutingConfig,
        catalog: ModelCatalog,
        failed: ResolvedRoute,
        expected: FallbackAxis,
    }

    let cases = vec![
        AxisCase {
            label: "Provider 軸: 別プロファイル・同一モデル",
            profiles: vec![profile("a", "model-shared"), profile("b", "model-shared")],
            routing: routing_config(&[(
                "summary",
                vec![candidate("a", None), candidate("b", None)],
            )]),
            catalog: catalog(&["model-shared"]),
            failed: ResolvedRoute {
                profile: "a".to_string(),
                model_id: "model-shared".to_string(),
            },
            expected: FallbackAxis::Provider,
        },
        AxisCase {
            label: "Model 軸: 同一プロファイル・別モデル",
            profiles: vec![profile("a", "model-x")],
            routing: routing_config(&[(
                "summary",
                vec![
                    candidate("a", Some("model-x")),
                    candidate("a", Some("model-y")),
                ],
            )]),
            catalog: catalog(&["model-x", "model-y"]),
            failed: ResolvedRoute {
                profile: "a".to_string(),
                model_id: "model-x".to_string(),
            },
            expected: FallbackAxis::Model,
        },
        AxisCase {
            label: "Both 軸: 別プロファイル・別モデル",
            profiles: vec![profile("a", "model-a"), profile("b", "model-b")],
            routing: routing_config(&[(
                "summary",
                vec![candidate("a", None), candidate("b", None)],
            )]),
            catalog: catalog(&["model-a", "model-b"]),
            failed: ResolvedRoute {
                profile: "a".to_string(),
                model_id: "model-a".to_string(),
            },
            expected: FallbackAxis::Both,
        },
    ];

    for AxisCase {
        label,
        profiles,
        routing,
        catalog,
        failed,
        expected,
    } in cases
    {
        let (router, _bus, mut rx) = router_with_bus(profiles, &routing, catalog);
        let mut affinity = SessionAffinity::default();

        let resolved = router
            .next_fallback(
                &mut affinity,
                "session-1",
                &LogicalModelId::from("summary"),
                &failed,
                FailureKind::Server,
                None,
            )
            .expect("フォールバック先が選択される");

        let event = rx.recv().await.expect("イベントを受信できる");
        let EventKind::Provider(ProviderEvent::FallbackTriggered {
            from_provider,
            from_model,
            to_provider,
            to_model,
            ..
        }) = event.kind
        else {
            panic!("FallbackTriggered イベントを期待しました: {:?}", event.kind);
        };

        let axis = FallbackAxis::classify(
            &from_provider,
            from_model.as_deref().unwrap_or_default(),
            &to_provider,
            &to_model,
        );
        assert_eq!(axis, expected, "分類ミスマッチ: {label}");
        assert_ne!(
            (resolved.profile.clone(), resolved.model_id.clone()),
            (failed.profile.clone(), failed.model_id.clone()),
            "失敗ルートが再選択されてはならない: {label}"
        );
    }
}
