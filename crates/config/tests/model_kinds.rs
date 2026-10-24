//! モデルの種別 (chat / image_generation) と画像生成のチェーンの検査 (ADR 0038)。

use config::{Config, ConfigError, LoadOptions, ModelKind};

fn options(root: &std::path::Path) -> LoadOptions {
    LoadOptions {
        user_config_dir: Some(root.to_path_buf()),
        read_env: false,
        ..Default::default()
    }
}

/// `document` を user config として書き、厳格な読み込みと通常の読み込みの結果を返す。
fn load_both(document: &str) -> (Result<Config, ConfigError>, Result<Config, ConfigError>) {
    let tmp = tempfile::tempdir().expect("temp");
    std::fs::write(tmp.path().join("config.toml"), document).expect("config");
    (
        Config::load_strict(&options(tmp.path())),
        Config::load(&options(tmp.path())),
    )
}

/// 厳格な読み込みが `path` で失敗し、通常の読み込みは続くことを確かめ、エラーの説明を返す。
fn strict_rejects(document: &str, path: &str) -> String {
    let (strict, tolerant) = load_both(document);
    let message = match strict {
        Err(ConfigError::InvalidField {
            path: found,
            message,
        }) if found == path => message,
        other => panic!("expected InvalidField at {path}, got {other:?}"),
    };
    tolerant.expect("a normal load only warns");
    message
}

const PROVIDERS: &str = r#"
version = 3
[providers.codex]
type = "openai-codex"
api_protocol = "openai-codex-responses"
base_url = "https://chatgpt.com/backend-api/codex"
credential = { type = "keyring", service = "evorch", account = "codex" }
models = ["gpt-6-astra", { id = "gpt-image-2", enabled = true, kind = "image_generation" }]
default_model = "gpt-6-astra"

[providers.openai]
type = "openai"
api_protocol = "openai-responses"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
models = ["gpt-5", "gpt-image-1", { id = "gpt-image-chat", enabled = true, kind = "chat" }]
default_model = "gpt-5"

[providers.local]
type = "openai-compatible"
base_url = "http://127.0.0.1:8080/v1"
api_key_env = "LOCAL_API_KEY"
models = ["local-chat", "gpt-image-1"]
default_model = "local-chat"
"#;

fn with_providers(rest: &str) -> String {
    format!("{PROVIDERS}\n{rest}")
}

// Given: kind を一切書かない既存の設定 (文字列だけの要素とテーブルの要素、ロールプロファイル)
// When: 厳格に読み込む
// Then: 移行なしでそのまま読み込め、種別は名前から推定される
#[test]
fn configs_without_kind_load_unchanged() {
    let document = r#"
version = 3
[providers.openai]
type = "openai"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
models = ["gpt-5", { id = "gpt-5-mini", enabled = false }]
default_model = "gpt-5"
[routing.routes]
worker = [{ profile = "openai" }]
mini = [{ profile = "openai", model = "gpt-5-mini" }]
[agents.explorer]
models = [{ route = "mini" }]
[role_profiles.night.routing.routes]
cheap = [{ profile = "openai", model = "gpt-5-mini" }]
[role_profiles.night.agents.worker]
models = [{ route = "cheap" }]
"#;
    let (strict, tolerant) = load_both(document);
    let config = strict.expect("strict load");
    assert_eq!(tolerant.expect("load"), config);
    let profile = &config.providers["openai"];
    assert!(profile.models.iter().all(|entry| entry.kind.is_none()));
    assert_eq!(profile.model_kind("gpt-5"), ModelKind::Chat);
    assert!(config.agents.image_generator_chain().is_empty());
}

// Given: OpenAI API キーの profile に gpt-image-1 があり、worker のチェーンに入れた設定
// When: 読み込む
// Then: 厳格な読み込みはチェーンのアドレス・route・モデルを示して失敗し、通常の読み込みは続く
#[test]
fn image_model_in_a_role_chain_is_rejected_by_strict_load() {
    let message = strict_rejects(
        &with_providers(
            "[routing.routes]\nart = [{ profile = 'openai', model = 'gpt-image-1' }]\n\
             [agents.worker]\nmodels = [{ route = 'art' }]\n",
        ),
        "agents.worker.models",
    );
    assert!(message.contains("`art`"), "{message}");
    assert!(message.contains("openai/gpt-image-1"), "{message}");
    assert!(message.contains("image_generation"), "{message}");
}

// Given: カテゴリ・compactor・ロールプロファイル・ロール名の route (チェーン未指定) に画像モデル
// When: 厳格に読み込む
// Then: それぞれのチェーンのアドレスで拒否される
#[test]
fn every_conversation_chain_checks_its_model_kinds() {
    for (rest, path) in [
        (
            "[routing.routes]\nart = [{ profile = 'codex', model = 'gpt-image-2' }]\n\
             [agents.worker.categories.visual]\nmodels = [{ route = 'art' }]\n",
            "agents.worker.categories.visual.models",
        ),
        (
            "[routing.routes]\nart = [{ profile = 'codex', model = 'gpt-image-2' }]\n\
             [agents.reviewer.categories.plan-review]\nmodels = [{ route = 'art' }]\n",
            "agents.reviewer.categories.plan-review.models",
        ),
        (
            "[routing.routes]\nart = [{ profile = 'codex', model = 'gpt-image-2' }]\n\
             [agents.roles.compactor]\nmodels = [{ route = 'art' }]\n",
            "agents.roles.compactor.models",
        ),
        (
            "[role_profiles.night.routing.routes]\nart = [{ profile = 'openai', model = 'dall-e-3' }]\n\
             [role_profiles.night.agents.roles.planner]\nmodels = [{ route = 'art' }]\n",
            "role_profiles.night.agents.roles.planner.models",
        ),
        (
            "[routing.routes]\nplanner = [{ profile = 'openai', model = 'gpt-image-1' }]\n",
            "agents.roles.planner.models",
        ),
    ] {
        let message = strict_rejects(&with_providers(rest), path);
        assert!(message.contains("image_generation"), "{path}: {message}");
    }
}

// Given: 名前からは画像と推定されるが kind = "chat" と明示したモデル
// When: worker のチェーンに入れて厳格に読み込む
// Then: 明示の種別が優先して受け付けられる
#[test]
fn explicit_chat_kind_overrides_inference() {
    let (strict, _) = load_both(&with_providers(
        "[routing.routes]\nchat = [{ profile = 'openai', model = 'gpt-image-chat' }]\n\
         [agents.worker]\nmodels = [{ route = 'chat' }]\n",
    ));
    strict.expect("explicit chat kind is accepted");
}

// Given: default_model に画像生成のモデルを選んだ profile
// When: 読み込む
// Then: 厳格な読み込みは profile の default_model のパスで失敗する
#[test]
fn image_default_model_is_rejected() {
    let message = strict_rejects(
        "version = 3\n[providers.openai]\ntype = 'openai'\nbase_url = 'https://api.openai.com/v1'\n\
         api_key_env = 'OPENAI_API_KEY'\nmodels = ['gpt-5', 'gpt-image-1']\ndefault_model = 'gpt-image-1'\n",
        "providers.openai.default_model",
    );
    assert!(message.contains("gpt-image-1"), "{message}");
}

// Given: Codex で生成し、上限に達したら OpenAI API キーへ移る image_generator のチェーン
// When: 厳格に読み込む
// Then: 受け付けられ、専用の取得口からチェーンを順に取り出せる
#[test]
fn image_generator_chain_accepts_codex_and_openai_image_routes() {
    let (strict, _) = load_both(&with_providers(
        "[routing.routes]\n\
         codex-image = [{ profile = 'codex', model = 'gpt-image-2' }]\n\
         openai-image = [{ profile = 'openai', model = 'gpt-image-1' }]\n\
         [agents.roles.image_generator]\n\
         models = [{ route = 'codex-image' }, { route = 'openai-image' }]\n",
    ));
    let config = strict.expect("image generator chain is valid");
    let routes: Vec<_> = config
        .agents
        .image_generator_chain()
        .iter()
        .map(|entry| entry.route.as_str())
        .collect();
    assert_eq!(routes, ["codex-image", "openai-image"]);
    assert!(
        config.agents.binding_for("image_generator", None).is_err(),
        "image_generator is not an agent role"
    );
}

// Given: image_generator のチェーンに会話用のモデル、画像生成に対応しない provider、
//        model 省略 (default_model は chat) の候補
// When: 厳格に読み込む
// Then: いずれも image_generator のアドレスで拒否される
#[test]
fn image_generator_chain_rejects_chat_models_and_other_providers() {
    for (route, expected) in [
        ("[{ profile = 'openai', model = 'gpt-5' }]", "openai/gpt-5"),
        ("[{ profile = 'codex' }]", "codex/gpt-6-astra"),
        (
            "[{ profile = 'local', model = 'gpt-image-1' }]",
            "openai-compatible",
        ),
    ] {
        let message = strict_rejects(
            &with_providers(&format!(
                "[routing.routes]\nimage = {route}\n\
                 [agents.roles.image_generator]\nmodels = [{{ route = 'image' }}]\n"
            )),
            "agents.roles.image_generator.models",
        );
        assert!(message.contains(expected), "{route}: {message}");
        assert!(message.contains("`image`"), "{route}: {message}");
    }
}

// Given: 会話用と画像生成用の候補が混ざった route (どのロールも使っていない)
// When: 厳格に読み込む
// Then: route のパスで拒否される
#[test]
fn routes_must_not_mix_model_kinds() {
    let message = strict_rejects(
        &with_providers(
            "[routing.routes]\nmixed = [{ profile = 'codex' }, { profile = 'openai', model = 'gpt-image-1' }]\n",
        ),
        "routing.routes.mixed",
    );
    assert!(message.contains("codex/gpt-6-astra: chat"), "{message}");
    assert!(
        message.contains("openai/gpt-image-1: image_generation"),
        "{message}"
    );
}

// Given: 種別の問題が複数ある設定
// When: 問題を列挙する
// Then: すべての問題が順に返り、同じ問題は 1 度だけ現れる
#[test]
fn chain_problems_lists_every_problem_once() {
    let (_, tolerant) = load_both(&with_providers(
        "[routing.routes]\nart = [{ profile = 'openai', model = 'gpt-image-1' }]\n\
         chat = [{ profile = 'openai' }]\n\
         [agents.worker]\nmodels = [{ route = 'art' }, { route = 'art' }]\n\
         [agents.roles.image_generator]\nmodels = [{ route = 'chat' }]\n",
    ));
    let config = tolerant.expect("normal load");
    let paths: Vec<_> = config
        .chain_problems()
        .into_iter()
        .map(|problem| match problem {
            ConfigError::InvalidField { path, .. } => path,
            other => panic!("unexpected problem {other:?}"),
        })
        .collect();
    assert_eq!(
        paths,
        [
            "agents.worker.models",
            "agents.roles.image_generator.models"
        ]
    );
    assert!(config.validate_chains().is_err());
}

// Given: kind に未知の値
// When: 読み込む
// Then: 要素の位置を含むパスで拒否される
#[test]
fn unknown_kind_value_reports_indexed_path() {
    let (strict, tolerant) = load_both(
        "version = 3\n[providers.p]\ntype = 'openai'\nbase_url = 'https://api.openai.com/v1'\n\
         api_key_env = 'K'\nmodels = ['gpt-5', { id = 'x', enabled = true, kind = 'video' }]\ndefault_model = 'gpt-5'\n",
    );
    for result in [strict, tolerant] {
        let error = result.expect_err("unknown kind is rejected");
        assert!(
            matches!(&error, ConfigError::InvalidField { path, message }
                if path == "providers.p.models[1].kind" && message.contains("image_generation")),
            "{error}"
        );
    }
}

// Given: kind を明示したモデルと明示しないモデルを持つ入力
// When: profile を保存する
// Then: 明示した kind だけがテーブルとして書かれ、書いていない kind は足されない
#[test]
fn saving_writes_only_explicit_kinds() {
    let tmp = tempfile::tempdir().expect("temp");
    let path = tmp.path().join("config.toml");
    let input = config::OpenAiCompatibleProviderInput {
        name: "local".into(),
        provider_type: config::ProviderTypeConfig::OpenAiCompatible,
        base_url: "http://127.0.0.1:8080/v1".into(),
        credential: config::ProviderCredentialInput::Env {
            var: "LOCAL_API_KEY".into(),
        },
        models: vec![
            config::ModelEntryConfig::enabled("local-chat"),
            config::ModelEntryConfig::enabled_with_inferred_kind("gpt-image-1"),
        ],
        excluded_models: Vec::new(),
        default_model: "local-chat".into(),
    };
    config::save_openai_compatible_provider(&path, &input).expect("save");

    let text = std::fs::read_to_string(&path).expect("text");
    assert!(text.contains("\"local-chat\""), "{text}");
    assert!(
        text.contains(r#"{ id = "gpt-image-1", enabled = true, kind = "image_generation" }"#),
        "{text}"
    );
    let config = Config::load_strict(&options(tmp.path())).expect("saved config loads");
    let models = &config.providers["local"].models;
    assert_eq!(models[0].kind, None);
    assert_eq!(models[1].kind, Some(ModelKind::ImageGeneration));

    // An image default_model is refused before anything is written.
    let mut image_default = input;
    image_default.default_model = "gpt-image-1".into();
    let error = config::save_openai_compatible_provider(&path, &image_default)
        .expect_err("image default model is rejected");
    assert!(
        matches!(&error, ConfigError::InvalidField { path, .. } if path == "providers.local.default_model"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&path).expect("text"), text);
}

// Given: image_generator のチェーンを含む agents
// When: 保存して読み直す
// Then: チェーンが保たれる
#[test]
fn image_generator_chain_survives_saving_agents() {
    let tmp = tempfile::tempdir().expect("temp");
    let path = tmp.path().join("config.toml");
    let mut agents = config::AgentsConfig::default();
    agents.roles.image_generator.models = vec![config::ChainEntryConfig::route("image")];
    config::save_agent_bindings(&path, &agents).expect("save agents");

    let config = Config::load(&options(tmp.path())).expect("load");
    assert_eq!(
        config.agents.roles.image_generator,
        agents.roles.image_generator
    );
}

// Given: 生成した JSON Schema
// When: モデル要素と追加ロールの定義を調べる
// Then: kind (chat / image_generation) と image_generator が載っている
#[test]
fn schema_describes_kind_and_image_generator() {
    let schema: serde_json::Value =
        serde_json::from_str(&config::json_schema()).expect("schema JSON");
    let entry = schema["$defs"]["ModelEntryConfig"]["anyOf"]
        .as_array()
        .and_then(|branches| branches.iter().find(|branch| branch["type"] == "object"))
        .expect("object model entry");
    assert!(entry["properties"]["kind"].is_object(), "{entry}");
    let kinds = serde_json::to_string(&schema["$defs"]["ModelKind"]).expect("kind");
    assert!(kinds.contains("\"chat\"") && kinds.contains("\"image_generation\""));
    assert!(schema["$defs"]["AdditionalRoleBindings"]["properties"]["image_generator"].is_object());
}
