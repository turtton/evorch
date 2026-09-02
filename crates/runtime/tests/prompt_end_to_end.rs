//! config 駆動のプロンプトソース解決と runtime カタログの結合テスト (issue #49)。
//!
//! 将来の composition root が行う結合 — `resolve_prompt_sources` →
//! [`SystemPromptCatalog`] への写像 → `system_prompt_for` — をテスト内の
//! ヘルパーでミラーし、設定 → プロンプトの全体経路を検証する (AC3, AC2, AC10,
//! AC4)。

use agents::Role;
use config::{AgentPromptSources, Config, ConfigError, resolve_prompt_sources};
use runtime::prompt::default_role_triggers;
use runtime::{SystemPromptCatalog, SystemPromptCatalogError};
use tempfile::TempDir;

/// Orchestrator binding が appendix として参照する同梱プリセット名。
///
/// 同梱 16 名のうち、比較対象の Orchestrator/quick/claude プロンプトの他
/// レイヤー (role-orchestrator baseline / family-claude / category-quick
/// overlay) に現れない名前を選ぶ。これによりユーザー上書きテストで変化する
/// レイヤーを appendix だけに限定して主張できる。
const ORCHESTRATOR_APPENDIX_PRESET: &str = "category-research";

/// Worker binding が appendix として参照する同梱プリセット名。
const WORKER_APPENDIX_PRESET: &str = "category-writing";

/// ユーザー上書きプリセットに埋めるセンチネル本文。
const OVERRIDE_SENTINEL: &str = "USER-OVERRIDE-SENTINEL-この行はユーザー上書きのappendix本文です";

/// バインディングに同梱プリセットへの参照を持つ設定フィクスチャ。
///
/// toml クレートに依存しないよう [`Config::default`] から構築する。
fn bound_config() -> Config {
    let mut config = Config::default();
    config.agents.orchestrator.preset = Some(ORCHESTRATOR_APPENDIX_PRESET.to_owned());
    config.agents.worker.preset = Some(WORKER_APPENDIX_PRESET.to_owned());
    config
}

/// presets サブディレクトリを持つ空のユーザー設定ディレクトリを作る。
fn empty_user_dir() -> TempDir {
    let dir = TempDir::new().expect("一時ディレクトリを作成できるはずです");
    std::fs::create_dir_all(dir.path().join("presets"))
        .expect("presets サブディレクトリを作成できるはずです");
    dir
}

/// [`resolve_prompt_sources`] の結果を [`SystemPromptCatalog`] に写像する。
///
/// composition root のミラー: role baseline はロール名キーのまま、family
/// section はカタログキー (`family-` 接頭辞付き) へ、appendix は binding の
/// プリセット名参照をロール名キーへ解決して登録する。
fn catalog_from_sources(
    config: &Config,
    sources: &AgentPromptSources,
) -> Result<SystemPromptCatalog, SystemPromptCatalogError> {
    let mut builder = SystemPromptCatalog::builder().triggers(default_role_triggers());
    for (role_name, body) in &sources.role_baselines {
        builder = builder.role_baseline(role_from_name(role_name), body.clone());
    }
    for (family_key, body) in &sources.family_sections {
        builder = builder.family_section(format!("family-{family_key}"), body.clone());
    }
    for (category_name, body) in &sources.category_overlays {
        builder = builder.category_overlay(category_name.clone(), body.clone());
    }
    let role_presets = [
        (
            Role::Orchestrator,
            config.agents.orchestrator.preset.as_deref(),
        ),
        (Role::Explorer, config.agents.explorer.preset.as_deref()),
        (Role::Worker, config.agents.worker.preset.as_deref()),
        (Role::Reviewer, config.agents.reviewer.preset.as_deref()),
    ];
    for (role, preset) in role_presets {
        let Some(preset) = preset else { continue };
        let body = sources
            .appendices
            .get(preset)
            .unwrap_or_else(|| panic!("appendix プリセット '{preset}' は解決済みのはずです"));
        builder = builder.appendix(role, body.clone());
    }
    builder.build()
}

/// [`resolve_prompt_sources`] のロール名キーを [`Role`] に変換する。
fn role_from_name(name: &str) -> Role {
    match name {
        "orchestrator" => Role::Orchestrator,
        "explorer" => Role::Explorer,
        "worker" => Role::Worker,
        "reviewer" => Role::Reviewer,
        other => panic!("resolve_prompt_sources は固定 4 ロール以外のキーを返さない: {other}"),
    }
}

// Given: 同梱プリセットのみで解決したソースから構築したカタログ
// When: Orchestrator / quick / claude-opus-4-1 で system_prompt_for を 2 回呼ぶ
// Then: 出力はバイト単位で同一 (AC3) で、同一入力の Worker 出力とは異なり、
//       appendix には binding 参照のプリセット本文が入る
#[test]
fn config_driven_catalog_produces_byte_identical_system_prompt() {
    let user_dir = empty_user_dir();
    let config = bound_config();
    let sources = resolve_prompt_sources(&config, Some(user_dir.path()))
        .expect("同梱プリセットのみで解決できるはずです");
    let catalog = catalog_from_sources(&config, &sources)
        .expect("必須部品が揃いカタログは構築できるはずです");

    let first = catalog
        .system_prompt_for(Role::Orchestrator, Some("quick"), "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");
    let second = catalog
        .system_prompt_for(Role::Orchestrator, Some("quick"), "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");
    let worker = catalog
        .system_prompt_for(Role::Worker, Some("quick"), "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");

    assert_eq!(
        first, second,
        "同一入力に対する出力はバイト単位で同一 (AC3)"
    );
    assert_ne!(
        first, worker,
        "同一入力でもロールが違えば出力は異なるはずです"
    );
    assert!(
        first.ends_with(sources.appendices[ORCHESTRATOR_APPENDIX_PRESET].trim_end()),
        "Orchestrator の末尾セクションは binding 参照のプリセット本文のはずです"
    );
}

// Given: binding 参照の appendix プリセットをユーザー上書きした設定ディレクトリ
// When: ソースを再解決してカタログを再構築し、同一引数で system_prompt_for する
// Then: 新しいプロンプトはセンチネルを含み、baseline / family / overlay の各
//       レイヤーは上書き前と同一で、変化は末尾の appendix レイヤーのみ (AC2)
#[test]
fn user_override_preset_changes_appendix_layer_only() {
    let user_dir = empty_user_dir();
    let config = bound_config();
    let before_sources = resolve_prompt_sources(&config, Some(user_dir.path()))
        .expect("上書き前は同梱プリセットで解決できるはずです");
    let before_catalog = catalog_from_sources(&config, &before_sources)
        .expect("必須部品が揃いカタログは構築できるはずです");
    let before = before_catalog
        .system_prompt_for(Role::Orchestrator, Some("quick"), "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");

    let override_path = user_dir
        .path()
        .join("presets")
        .join(format!("{ORCHESTRATOR_APPENDIX_PRESET}.md"));
    std::fs::write(&override_path, format!("{OVERRIDE_SENTINEL}\n"))
        .expect("ユーザー上書きプリセットを書き込めるはずです");

    let after_sources = resolve_prompt_sources(&config, Some(user_dir.path()))
        .expect("ユーザー上書き後も解決できるはずです");
    let after_catalog = catalog_from_sources(&config, &after_sources)
        .expect("必須部品が揃いカタログは構築できるはずです");
    let after = after_catalog
        .system_prompt_for(Role::Orchestrator, Some("quick"), "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");

    assert!(
        after.contains(OVERRIDE_SENTINEL),
        "上書き後はセンチネルを含むはずです"
    );
    assert!(
        !before.contains(OVERRIDE_SENTINEL),
        "上書き前はセンチネルを含まないはずです"
    );
    let before_baseline = before
        .split("\n\n")
        .next()
        .expect("baseline セクションは存在するはずです");
    let after_baseline = after
        .split("\n\n")
        .next()
        .expect("baseline セクションは存在するはずです");
    assert_eq!(
        before_baseline, after_baseline,
        "baseline セクションはバイト単位で不変のはずです"
    );
    assert!(
        after.contains(before_sources.family_sections["claude"].trim_end()),
        "family セクションは不変のはずです"
    );
    assert!(
        after.contains(before_sources.category_overlays["quick"].trim_end()),
        "quick overlay は不変のはずです"
    );
    assert!(
        before.ends_with(before_sources.appendices[ORCHESTRATOR_APPENDIX_PRESET].trim_end()),
        "上書き前の末尾セクションは同梱プリセット本文のはずです"
    );
    assert!(
        after.ends_with(OVERRIDE_SENTINEL),
        "上書き後の末尾セクションはセンチネルのはずです"
    );
}

// Given: 同梱・ユーザーのどちらにも存在しないプリセット名を参照する設定
// When: カタログ構築より前の段階で resolve_prompt_sources する
// Then: PresetNotFound の型付きエラーになり、Display は識別子を含むがプリセット
//       本文は一切含まない (fail-closed でカタログは構築されない)
#[test]
fn missing_referenced_preset_fails_before_catalog_exists() {
    let user_dir = empty_user_dir();
    let config = bound_config();
    // 本文センチネルは正常系の解決結果から導出する (本文をテスト定数に書かない)
    let good_sources =
        resolve_prompt_sources(&config, Some(user_dir.path())).expect("正常系は解決できるはずです");
    let body_sentinels = [
        good_sources.role_baselines["orchestrator"]
            .lines()
            .next()
            .expect("baseline 本文は非空のはずです"),
        good_sources.family_sections["claude"]
            .lines()
            .next()
            .expect("family 本文は非空のはずです"),
        good_sources.appendices[ORCHESTRATOR_APPENDIX_PRESET]
            .lines()
            .next()
            .expect("appendix 本文は非空のはずです"),
    ];

    let mut missing_config = bound_config();
    missing_config.agents.orchestrator.preset = Some("no-such-preset-anywhere".to_owned());
    let error = resolve_prompt_sources(&missing_config, Some(user_dir.path()))
        .expect_err("存在しないプリセット参照は解決に失敗するはずです");

    let display = error.to_string();
    assert!(
        matches!(&error, ConfigError::PresetNotFound { name } if name == "no-such-preset-anywhere"),
        "PresetNotFound の型付きエラーのはずです: {error:?}"
    );
    assert!(
        display.contains("no-such-preset-anywhere"),
        "エラー Display は識別子を含むはずです: {display}"
    );
    for sentinel in body_sentinels {
        assert!(
            !display.contains(sentinel),
            "エラー Display にプリセット本文を含まないはずです: {display}"
        );
    }
    // Err が先に返るため catalog_from_sources (builder) は一度も呼ばれない。
    // このテストは resolve の呼び出しのみで、カタログ構築に到達しないこと自体が検証対象。
}

// Given: 同梱プリセットのみで構築したカタログ
// When: 未知の model id で system_prompt_for する (AC4 fail-safe を全体経路で検証)
// Then: family-generic セクション本文が選ばれ、他ファミリの本文は一切現れない
#[test]
fn unknown_model_id_uses_generic_family_section_end_to_end() {
    let user_dir = empty_user_dir();
    let config = bound_config();
    let sources = resolve_prompt_sources(&config, Some(user_dir.path()))
        .expect("同梱プリセットのみで解決できるはずです");
    let catalog = catalog_from_sources(&config, &sources)
        .expect("必須部品が揃いカタログは構築できるはずです");

    let prompt = catalog
        .system_prompt_for(Role::Orchestrator, Some("quick"), "totally-unknown-model")
        .expect("登録済みの部品のみを参照するはずです");

    assert!(
        prompt.contains(sources.family_sections["generic"].trim_end()),
        "未知 model id は family-generic セクションを選ぶはずです"
    );
    for (family_key, body) in &sources.family_sections {
        if family_key == "generic" {
            continue;
        }
        assert!(
            !prompt.contains(body.trim_end()),
            "family-{family_key} の本文は現れないはずです"
        );
    }
    assert!(
        prompt.ends_with(sources.appendices[ORCHESTRATOR_APPENDIX_PRESET].trim_end()),
        "appendix は binding 参照のプリセット本文で終わるはずです"
    );
}
