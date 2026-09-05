//! 設定からprovider群とRouterを一括構築する契約を検証します。

use std::collections::BTreeMap;
use std::sync::Arc;

use config::{
    ApiProtocolConfig, Config, CredentialRefConfig, LoadOptions, ProviderProfileConfig,
    ProviderTypeConfig, RoleBindingConfig, RouteCandidateConfig, RoutingConfig,
};
use model::{LogicalModelId, ModelCatalog};
use routing::factory::FactoryOptions;
use routing::{ComposeDeps, MapEnv, RoutingError, SessionAffinity, compose_providers};
use sandbox::credential::{CredentialStore, FileCredentialStore};

const PROFILE: &str = "local";
const MODEL: &str = "local-model";
const API_KEY: &str = "SENTINEL-API-KEY-79";
const API_KEY_ENV: &str = "LOCAL_API_KEY";

fn provider_config(provider_type: ProviderTypeConfig) -> ProviderProfileConfig {
    ProviderProfileConfig {
        provider_type,
        api_protocol: ApiProtocolConfig::OpenAiCompletions,
        base_url: "https://example.test/v1".to_string(),
        credential: CredentialRefConfig::Env {
            var: API_KEY_ENV.to_string(),
        },
        models: vec![MODEL.to_string()],
        default_model: MODEL.to_string(),
    }
}

fn config_with(provider_type: ProviderTypeConfig) -> Config {
    Config {
        providers: BTreeMap::from([(PROFILE.to_string(), provider_config(provider_type))]),
        ..Config::default()
    }
}

fn credential_store() -> Arc<dyn CredentialStore> {
    let directory = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    Arc::new(FileCredentialStore::open(directory.path()).expect("資格情報ストアを開ける"))
}

fn deps(env: MapEnv) -> ComposeDeps {
    ComposeDeps {
        credential_store: credential_store(),
        event_bus: None,
        env: Arc::new(env),
        catalog: ModelCatalog::builtin(),
        factory: FactoryOptions::default(),
    }
}

fn populated_env() -> MapEnv {
    MapEnv::from_iter([(API_KEY_ENV, API_KEY)])
}

// Given: 環境変数認証のproviderと未設定の環境 / When: compose / Then: profile名と変数名を持つMissingCredentialで全体を失敗させる
#[test]
fn compose_fails_when_environment_credential_is_missing() {
    let error = compose_providers(
        &config_with(ProviderTypeConfig::OpenAiCompatible),
        deps(MapEnv::default()),
    )
    .expect_err("未設定credentialを拒否する");

    assert_eq!(
        error,
        RoutingError::MissingCredential {
            profile: PROFILE.to_string(),
            var: API_KEY_ENV.to_string()
        }
    );
    assert!(!error.to_string().contains(API_KEY));
}

// Given: 空白だけの環境変数credential / When: compose / Then: EmptyCredentialで全体を失敗させる
#[test]
fn compose_fails_when_environment_credential_is_empty() {
    let error = compose_providers(
        &config_with(ProviderTypeConfig::OpenAiCompatible),
        deps(MapEnv::from_iter([(API_KEY_ENV, "  ")])),
    )
    .expect_err("空credentialを拒否する");

    assert_eq!(
        error,
        RoutingError::EmptyCredential {
            profile: PROFILE.to_string(),
            var: API_KEY_ENV.to_string()
        }
    );
}

// Given: 有効なproviderとworker用論理モデル / When: compose後にresolve / Then: 発見済みcatalogからdefault_modelへ解決する
#[test]
fn compose_builds_clients_and_router_with_discovered_models() {
    let mut config = config_with(ProviderTypeConfig::OpenAiCompatible);
    config.agents.worker = RoleBindingConfig {
        logical_model: Some("coding".to_string()),
        ..RoleBindingConfig::default()
    };
    let composed = compose_providers(&config, deps(populated_env())).expect("composeに成功する");
    let mut affinity = SessionAffinity::default();

    let resolved = composed
        .router
        .resolve(&mut affinity, "session-1", &LogicalModelId::from("coding"))
        .expect("worker論理モデルを解決できる");

    assert_eq!(resolved.profile, PROFILE);
    assert_eq!(resolved.model_id, MODEL);
    assert!(composed.provider(PROFILE).is_some());
    assert!(!composed.is_empty());
}

// Given: routing.routesが空 / When: compose / Then: 4ロール名のdefault routeを合成する
#[test]
fn compose_synthesizes_default_role_routes() {
    let composed = compose_providers(
        &config_with(ProviderTypeConfig::OpenAiCompatible),
        deps(populated_env()),
    )
    .expect("composeに成功する");

    for logical in ["orchestrator", "explorer", "worker", "reviewer"] {
        let resolved = composed
            .router
            .resolve(
                &mut SessionAffinity::default(),
                "session-default",
                &LogicalModelId::from(logical),
            )
            .expect("default routeを解決できる");
        assert_eq!(resolved.profile, PROFILE);
    }
}

// Given: 明示的routeだけを持つ設定 / When: compose / Then: routeを合成せず指定内容をそのまま使う
#[test]
fn compose_respects_explicit_routes() {
    let mut config = config_with(ProviderTypeConfig::OpenAiCompatible);
    config.routing = RoutingConfig {
        routes: BTreeMap::from([(
            "custom".to_string(),
            vec![RouteCandidateConfig {
                profile: PROFILE.to_string(),
                model: None,
            }],
        )]),
    };
    let composed = compose_providers(&config, deps(populated_env())).expect("composeに成功する");
    let resolved = composed
        .router
        .resolve(
            &mut SessionAffinity::default(),
            "session-custom",
            &LogicalModelId::from("custom"),
        )
        .expect("明示的routeを解決できる");

    let error = composed
        .router
        .resolve(
            &mut SessionAffinity::default(),
            "session-explicit",
            &LogicalModelId::from("orchestrator"),
        )
        .expect_err("未指定routeは合成されない");

    assert_eq!(resolved.profile, PROFILE);
    assert_eq!(resolved.model_id, MODEL);
    assert_eq!(
        error,
        RoutingError::UnknownLogicalModel("orchestrator".to_string())
    );
}

// Given: providerが0件 / When: compose / Then: NoProvidersを返す
#[test]
fn compose_rejects_zero_providers() {
    let error = compose_providers(&Config::default(), deps(MapEnv::default()))
        .expect_err("provider無しを拒否する");

    assert_eq!(error, RoutingError::NoProviders);
}

// Given: map内に未対応anthropic provider / When: compose / Then: 一件でも未対応なら全体を失敗させる
#[test]
fn compose_fails_closed_for_unsupported_provider_type() {
    let error = compose_providers(
        &config_with(ProviderTypeConfig::Anthropic),
        deps(populated_env()),
    )
    .expect_err("未対応providerを拒否する");

    assert_eq!(
        error,
        RoutingError::UnsupportedProviderType {
            provider_type: "anthropic".to_string()
        }
    );
}

// Given: APIキーを解決済みのComposedProvider / When: Debug出力 / Then: auth値を一切含めない
#[test]
fn composed_provider_debug_never_contains_api_key() {
    let composed = compose_providers(
        &config_with(ProviderTypeConfig::OpenAiCompatible),
        deps(populated_env()),
    )
    .expect("composeに成功する");

    let rendered = format!(
        "{:?}",
        composed.provider(PROFILE).expect("providerが存在する")
    );

    assert!(!rendered.contains(API_KEY));
    assert!(!rendered.contains("ProviderAuth"));
}

// Given: typeとapi_key_env sugarを使うevorch.toml / When: Config::load後にcompose / Then: 正規化済み設定から成功する
#[test]
fn loaded_sugar_config_composes_across_crates() {
    let temporary = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let project = temporary.path().join("project");
    std::fs::create_dir_all(&project).expect("project directoryを作成できる");
    std::fs::write(
        project.join("evorch.toml"),
        format!(
            r#"[providers.{PROFILE}]
type = "openai-compatible"
base_url = "https://example.test/v1"
api_key_env = "{API_KEY_ENV}"
models = ["{MODEL}"]
default_model = "{MODEL}"
"#
        ),
    )
    .expect("設定を書き込める");
    let loaded = Config::load(&LoadOptions {
        project_dir: Some(project),
        user_config_dir: Some(temporary.path().join("empty-user")),
        read_env: false,
        ..LoadOptions::default()
    })
    .expect("sugar設定を読み込める");

    let composed = compose_providers(&loaded, deps(populated_env())).expect("composeに成功する");

    assert!(composed.provider(PROFILE).is_some());
}
