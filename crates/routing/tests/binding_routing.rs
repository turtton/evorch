//! agents バインディング (config::AgentsConfig) と Router の接続を
//! 公開 API 経由で検証する pin テスト。
//!
//! `binding_for` が返す論理モデル名がルートテーブルのキーとして Router に渡り、
//! 解決とフォールバックが既存の候補順序ルールどおりに振る舞うことを
//! クロスクレート結合レベルで固定する。

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

/// worker.categories.quick の logical_model のみ指定したバインディング構成。
/// preset は参照の余地を残しつつ未指定 (None) とする。
fn worker_quick_binding() -> config::AgentsConfig {
    config::AgentsConfig {
        worker: config::RoleBindingConfig {
            categories: [(
                "quick".to_string(),
                config::CategoryBindingConfig {
                    logical_model: Some("worker-quick".to_string()),
                    ..config::CategoryBindingConfig::default()
                },
            )]
            .into_iter()
            .collect(),
            ..config::RoleBindingConfig::default()
        },
        ..config::AgentsConfig::default()
    }
}

// Given: worker.categories.quick に logical_model "worker-quick" を持つバインディングと、
//        同名の論理モデルを 1 候補 (profile-a) に振るルーティング構成
// When: binding_for("worker", Some("quick")) で論理モデルを取り出し、
//       その名前を Router で解決する
// Then: バインディングの論理モデル "worker-quick" が Router の解決を経て
//       (profile-a, model-quick) の具体ルートになり、勝者がピンされる
#[test]
fn agents_binding_logical_model_resolves_via_router_to_profile_and_model() {
    let agents = worker_quick_binding();
    let profiles = vec![profile("profile-a", "model-quick")];
    let routing = routing_config(&[("worker-quick", vec![candidate("profile-a", None)])]);
    let catalog = catalog(&["model-quick"]);
    let router =
        Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");

    let binding = agents
        .binding_for("worker", Some("quick"))
        .expect("quick カテゴリのバインディングを解決できる");
    assert_eq!(binding.logical_model, "worker-quick");
    assert_eq!(binding.preset, None, "プリセット未指定は None のまま");

    let mut affinity = SessionAffinity::default();
    let resolved = router
        .resolve(
            &mut affinity,
            "session-1",
            &LogicalModelId::from(binding.logical_model.as_str()),
        )
        .expect("バインディングの論理モデルをルートへ解決できる");

    assert_eq!(
        resolved,
        ResolvedRoute {
            profile: "profile-a".to_string(),
            model_id: "model-quick".to_string(),
        }
    );
    assert_eq!(
        affinity.pinned("session-1", "worker-quick"),
        Some("profile-a"),
        "解決勝者のプロファイルがピンされる"
    );
}

// Given: worker.categories.quick のバインディングから得た論理モデル "worker-quick" を、
//        同一実モデルを既定に持つ 2 プロファイル (profile-a, profile-b) に宣言順で
//        振るルーティング構成
// When: 解決して先頭候補を得た後、それを失敗ルートとしてフォールバックする
// Then: 次候補 (profile-b, model-shared) が返り実モデルは同一のまま、
//       セッションのピンはプロファイル名のみ profile-b へ張り替わる
#[test]
fn binding_driven_fallback_keeps_same_model_other_profile_as_next_candidate() {
    let agents = worker_quick_binding();
    let profiles = vec![
        profile("profile-a", "model-shared"),
        profile("profile-b", "model-shared"),
    ];
    let routing = routing_config(&[(
        "worker-quick",
        vec![candidate("profile-a", None), candidate("profile-b", None)],
    )]);
    let catalog = catalog(&["model-shared"]);
    let router =
        Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");
    let binding = agents
        .binding_for("worker", Some("quick"))
        .expect("quick カテゴリのバインディングを解決できる");
    let logical = LogicalModelId::from(binding.logical_model);
    let mut affinity = SessionAffinity::default();

    let resolved = router
        .resolve(&mut affinity, "session-1", &logical)
        .expect("宣言順先頭の候補へ解決できる");
    assert_eq!(
        resolved,
        ResolvedRoute {
            profile: "profile-a".to_string(),
            model_id: "model-shared".to_string(),
        },
        "同一実モデルでも宣言順先頭のプロファイルが選ばれる"
    );

    let fallback = router
        .next_fallback(
            &mut affinity,
            "session-1",
            &logical,
            &resolved,
            FailureKind::Server,
            None,
        )
        .expect("同一実モデルの別プロファイル候補へフォールバックできる");
    assert_eq!(
        fallback,
        ResolvedRoute {
            profile: "profile-b".to_string(),
            model_id: "model-shared".to_string(),
        },
        "実モデルを変えずに別プロファイルへフォールバックする"
    );
    assert_eq!(
        affinity.pinned("session-1", logical.as_str()),
        Some("profile-b"),
        "フォールバック先プロファイル名のみが再ピンされる"
    );
}
