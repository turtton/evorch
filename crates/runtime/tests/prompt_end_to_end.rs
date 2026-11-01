//! config 駆動のプロンプトソース解決と runtime カタログの結合テスト (issue #49)。
//!
//! production composition root [`build_catalog`] が `resolve_prompt_sources` の
//! 結果を [`SystemPromptCatalog`] へ写像する経路を通し、設定 → プロンプトの
//! 全体経路を検証する (AC3, AC2, AC10, AC4)。

use agents::Role;
use config::{Config, ConfigError, resolve_prompt_sources};
use runtime::prompt::PromptPartKind;
use runtime::{CatalogBuildInput, PromptCompositionError, SystemPromptCatalog, build_catalog};
use tempfile::TempDir;

/// Orchestrator binding が appendix として参照する同梱プリセット名。
///
/// 同梱プリセットのうち、Orchestrator/claude プロンプトの他
/// レイヤー (role-orchestrator baseline / family-claude)
/// に現れない名前を選ぶ。これによりユーザー上書きテストで変化する
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
    config.agents.worker.base.preset = Some(WORKER_APPENDIX_PRESET.to_owned());
    config
}

/// presets サブディレクトリを持つ空のユーザー設定ディレクトリを作る。
fn empty_user_dir() -> TempDir {
    let dir = TempDir::new().expect("一時ディレクトリを作成できるはずです");
    std::fs::create_dir_all(dir.path().join("presets"))
        .expect("presets サブディレクトリを作成できるはずです");
    dir
}

/// production composition root ([`build_catalog`]) でカタログを構築する。
///
/// availability は空のリストを渡す (keyTriggers は既定の 4 ロール分のみ)。
fn production_catalog(
    config: &Config,
    user_dir: &TempDir,
) -> Result<SystemPromptCatalog, PromptCompositionError> {
    build_catalog(&CatalogBuildInput {
        config,
        user_presets_dir: Some(user_dir.path()),
        available_agents: &[],
        available_skills: &[],
    })
}

// Given: 同梱プリセットのみで解決したソースから構築したカタログ
// When: Orchestrator / カテゴリなし / claude-opus-4-1 で system_prompt_for を 2 回呼ぶ
// Then: 出力はバイト単位で同一 (AC3) で、Worker / quick 出力とは異なり、
//       appendix には binding 参照のプリセット本文が入る
#[test]
fn config_driven_catalog_produces_byte_identical_system_prompt() {
    let user_dir = empty_user_dir();
    let config = bound_config();
    let sources = resolve_prompt_sources(&config, Some(user_dir.path()))
        .expect("同梱プリセットのみで解決できるはずです");
    let catalog =
        production_catalog(&config, &user_dir).expect("必須部品が揃いカタログは構築できるはずです");

    let first = catalog
        .system_prompt_for(Role::Orchestrator, None, "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");
    let second = catalog
        .system_prompt_for(Role::Orchestrator, None, "claude-opus-4-1")
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
        "ロールとカテゴリが違えば出力は異なるはずです"
    );
    assert_eq!(
        worker,
        [
            sources.role_baselines["worker"].trim_end(),
            sources.category_overlays["quick"].trim_end(),
            sources.appendices[WORKER_APPENDIX_PRESET].trim_end(),
        ]
        .join("\n\n"),
        "空の既定 family は余分な区切りを残さず省略される"
    );
    assert!(
        catalog
            .system_prompt_sections_for(Role::Worker, Some("quick"), "claude-opus-4-1")
            .unwrap()
            .iter()
            .all(|section| section.kind != PromptPartKind::ModelFamily)
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
    std::fs::write(
        user_dir.path().join("presets/family-claude.md"),
        "USER-CLAUDE-FAMILY-SENTINEL\n",
    )
    .unwrap();
    let config = bound_config();
    let before_sources = resolve_prompt_sources(&config, Some(user_dir.path()))
        .expect("上書き前は同梱プリセットで解決できるはずです");
    let before_catalog =
        production_catalog(&config, &user_dir).expect("必須部品が揃いカタログは構築できるはずです");
    let before = before_catalog
        .system_prompt_for(Role::Worker, Some("quick"), "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");

    let override_path = user_dir
        .path()
        .join("presets")
        .join(format!("{WORKER_APPENDIX_PRESET}.md"));
    std::fs::write(&override_path, format!("{OVERRIDE_SENTINEL}\n"))
        .expect("ユーザー上書きプリセットを書き込めるはずです");

    let after_catalog =
        production_catalog(&config, &user_dir).expect("必須部品が揃いカタログは構築できるはずです");
    let after = after_catalog
        .system_prompt_for(Role::Worker, Some("quick"), "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");

    assert!(
        after.contains(OVERRIDE_SENTINEL),
        "上書き後はセンチネルを含むはずです"
    );
    assert!(
        !before.contains(OVERRIDE_SENTINEL),
        "上書き前はセンチネルを含まないはずです"
    );
    let original_appendix = before_sources.appendices[WORKER_APPENDIX_PRESET].trim_end();
    let unchanged_prefix = before
        .strip_suffix(original_appendix)
        .expect("上書き前は同梱 appendix で終わる");
    assert_eq!(after, format!("{unchanged_prefix}{OVERRIDE_SENTINEL}"));
    assert!(after.contains(before_sources.family_sections["claude"].trim_end()));
    assert!(
        after.ends_with(OVERRIDE_SENTINEL),
        "上書き後の末尾セクションはセンチネルのはずです"
    );
}

// Given: 同梱・ユーザーのどちらにも存在しないプリセット名を参照する設定
// When: カタログ構築より前の段階で production composition root
//       (build_catalog) を呼ぶ
// Then: PresetResolution 経由の PresetNotFound の型付きエラーになり、Display は
//       識別子を含むがプリセット本文は一切含まない (fail-closed でカタログは
//       構築されない)
#[test]
fn missing_referenced_preset_fails_before_catalog_exists() {
    let user_dir = empty_user_dir();
    std::fs::write(
        user_dir.path().join("presets/family-claude.md"),
        "PRIVATE-FAMILY-BODY-SENTINEL\n",
    )
    .unwrap();
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
    let error = production_catalog(&missing_config, &user_dir)
        .expect_err("存在しないプリセット参照は解決に失敗するはずです");

    let display = error.to_string();
    assert!(
        matches!(
            &error,
            PromptCompositionError::PresetResolution(ConfigError::PresetNotFound { name })
                if name == "no-such-preset-anywhere"
        ),
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
    // resolve_prompt_sources の Err が先に返るため、カタログ builder
    // (SystemPromptCatalogBuilder) は一度も呼ばれない。このテストは production
    // 経路の呼び出しのみで、カタログ構築に到達しないこと自体が検証対象。
}

// ユーザーが上書きした各 family の選択と非漏洩を production 経路で検証する。
#[test]
fn user_family_overrides_are_selected_by_model_without_leaking_other_families() {
    let user_dir = empty_user_dir();
    let cases = [
        ("claude", "claude-opus-4-1"),
        ("openai-reasoning", "o3"),
        ("gpt5", "gpt-5"),
        ("gemini", "gemini-2.5-pro"),
        ("kimi", "kimi-k2"),
        ("generic", "totally-unknown-model"),
    ];
    for (family, _) in cases {
        std::fs::write(
            user_dir.path().join(format!("presets/family-{family}.md")),
            format!("USER-FAMILY-{family}-SENTINEL\n"),
        )
        .unwrap();
    }
    let config = bound_config();
    let sources = resolve_prompt_sources(&config, Some(user_dir.path())).unwrap();
    let catalog = production_catalog(&config, &user_dir).unwrap();

    for (selected, model) in cases {
        let sections = catalog
            .system_prompt_sections_for(Role::Orchestrator, None, model)
            .unwrap();
        let family = sections
            .iter()
            .find(|section| section.kind == PromptPartKind::ModelFamily)
            .expect("非空のユーザー family が組み込まれる");
        assert_eq!(family.preset, Some(format!("family-{selected}")));
        assert_eq!(family.text, sources.family_sections[selected].trim_end());
        let prompt = catalog
            .system_prompt_for(Role::Orchestrator, None, model)
            .unwrap();
        for (key, body) in &sources.family_sections {
            assert_eq!(
                prompt.contains(body.trim_end()),
                key == selected,
                "{model}: {key}"
            );
        }
        assert!(prompt.ends_with(sources.appendices[ORCHESTRATOR_APPENDIX_PRESET].trim_end()));
    }
}

/// 同梱の role×family preset 名と、その本文を一意に識別する見出し。
const REVIEWER_GPT5_PRESET: &str = "role-reviewer-gpt5";
const REVIEWER_GPT5_HEADING: &str = "## 指摘の重要度";

// Given: 同梱プリセットのみで production 経路から構築したカタログ
// When: Reviewer / Worker を category・model を変えて system_prompt_sections_for する
// Then: 同梱の role-reviewer-gpt5 は category なしの Reviewer × GPT-5 以降 (gpt-6.1-sol を含む)
//       にだけ入り、それ以外 (category あり・別ロール・Claude・gpt-4o) には入らない
#[test]
fn bundled_reviewer_gpt5_preset_applies_only_to_plain_reviewer_on_gpt_family() {
    let user_dir = empty_user_dir();
    let config = bound_config();
    let sources = resolve_prompt_sources(&config, Some(user_dir.path())).unwrap();
    let body = sources.role_family_sections[&("reviewer".to_owned(), "gpt5".to_owned())].clone();
    assert!(body.starts_with(REVIEWER_GPT5_HEADING));
    let catalog = production_catalog(&config, &user_dir).unwrap();

    let applies = [
        (Role::Reviewer, None, "gpt-5"),
        (Role::Reviewer, None, "gpt-6.1-sol"),
        (Role::Reviewer, None, "openai/gpt-6-astra"),
    ];
    for (role, category, model) in applies {
        let sections = catalog
            .system_prompt_sections_for(role, category, model)
            .unwrap();
        let layer = sections
            .iter()
            .find(|section| section.kind == PromptPartKind::RoleFamily)
            .unwrap_or_else(|| panic!("{model} の Reviewer に層が付くはずです"));
        assert_eq!(layer.preset.as_deref(), Some(REVIEWER_GPT5_PRESET));
        assert_eq!(layer.text, body.trim_end());
    }

    let skipped = [
        (Role::Reviewer, Some("plan-review"), "gpt-5"),
        (Role::Reviewer, Some("lesson_review"), "gpt-6.1-sol"),
        (Role::Reviewer, None, "claude-opus-4-1"),
        (Role::Reviewer, None, "gpt-4o"),
        (Role::Worker, None, "gpt-6.1-sol"),
        (Role::Orchestrator, None, "gpt-6.1-sol"),
    ];
    for (role, category, model) in skipped {
        let prompt = catalog.system_prompt_for(role, category, model).unwrap();
        assert!(
            !prompt.contains(REVIEWER_GPT5_HEADING),
            "{} / {category:?} / {model} には付かないはずです",
            role.name()
        );
    }
}

// Given: role-reviewer-gpt5 をユーザー上書きした設定ディレクトリ
// When: 上書き前後のカタログで Reviewer / gpt-6.1-sol の各セクションを比較する
// Then: RoleFamily 層だけが上書き本文に差し替わり、他の層は同一
#[test]
fn user_override_replaces_only_the_reviewer_gpt5_layer() {
    let user_dir = empty_user_dir();
    let config = bound_config();
    let before = production_catalog(&config, &user_dir)
        .unwrap()
        .system_prompt_sections_for(Role::Reviewer, None, "gpt-6.1-sol")
        .unwrap();
    std::fs::write(
        user_dir
            .path()
            .join(format!("presets/{REVIEWER_GPT5_PRESET}.md")),
        format!("{OVERRIDE_SENTINEL}\n"),
    )
    .unwrap();
    let after = production_catalog(&config, &user_dir)
        .unwrap()
        .system_prompt_sections_for(Role::Reviewer, None, "gpt-6.1-sol")
        .unwrap();

    assert_eq!(before.len(), after.len());
    for (old, new) in before.iter().zip(&after) {
        if new.kind == PromptPartKind::RoleFamily {
            assert_eq!(new.text, OVERRIDE_SENTINEL);
            assert_ne!(old.text, new.text);
        } else {
            assert_eq!(old, new);
        }
    }
}
