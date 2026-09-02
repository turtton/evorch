//! config → production カタログ組立 (composition root) の結合テスト (issue #49)。
//!
//! `build_catalog` が config の preset 参照 (ロール直下とカテゴリバインディング
//! の両方) を appendix レイヤーへ流すこと、プリセット解決の失敗で fail-closed
//! になること、利用可能 agent/skill のメタデータが keyTriggers へ流れることを
//! 検証する (AC3, AC9, AC10)。

mod support;

use std::sync::Arc;

use agents::Role;
use config::{CategoryBindingConfig, Config, ConfigError, resolve_prompt_sources};
use event_bus::EventBus;
use runtime::prompt::{AvailableAgent, AvailableSkill};
use runtime::skill::{SkillScope, discover_skills};
use runtime::{AgentRuntime, CatalogBuildInput, PromptCompositionError, build_catalog};
use sandbox::DirectSandbox;
use tempfile::TempDir;
use tools::ToolExecutor;

use support::ScriptedModel;

/// Orchestrator ロール直下 binding が参照する同梱プリセット名。
const ROLE_APPENDIX_PRESET: &str = "category-writing";

/// categories.quick binding が参照する同梱プリセット名。
///
/// quick overlay (category-quick) の本文ともロール直下参照 (category-writing)
/// の本文とも一致しない名前を選び、appendix レイヤーの主張を曖昧にしない。
const CATEGORY_APPENDIX_PRESET: &str = "category-deep";

/// テストで使うカテゴリ名。
const CATEGORY: &str = "quick";

/// 存在しないプリセット名。
const MISSING_PRESET: &str = "no-such-preset-anywhere";

/// presets サブディレクトリを持つ空のユーザー設定ディレクトリを作る。
fn empty_user_dir() -> TempDir {
    let dir = TempDir::new().expect("一時ディレクトリを作成できるはずです");
    std::fs::create_dir_all(dir.path().join("presets"))
        .expect("presets サブディレクトリを作成できるはずです");
    dir
}

/// [`build_catalog`] への入力を組む。
fn build_input<'a>(
    config: &'a Config,
    user_dir: &'a TempDir,
    agents: &'a [AvailableAgent],
    skills: &'a [AvailableSkill],
) -> CatalogBuildInput<'a> {
    CatalogBuildInput {
        config,
        user_presets_dir: Some(user_dir.path()),
        available_agents: agents,
        available_skills: skills,
    }
}

// Given: Orchestrator のロール直下 preset と categories.quick preset が両方とも
//        同梱プリセットを参照する設定
// When: availability 空で build_catalog し、Orchestrator / quick で
//       system_prompt_for する
// Then: appendix レイヤーはカテゴリスコープ preset の本文で終わり、ロール直下
//       preset の本文は置き換わり、baseline / overlay は sources どおり現れる
#[test]
fn production_composition_builds_config_driven_catalog() {
    let user_dir = empty_user_dir();
    let mut config = Config::default();
    config.agents.orchestrator.preset = Some(ROLE_APPENDIX_PRESET.to_owned());
    config.agents.orchestrator.categories.insert(
        CATEGORY.to_owned(),
        CategoryBindingConfig {
            preset: Some(CATEGORY_APPENDIX_PRESET.to_owned()),
            ..CategoryBindingConfig::default()
        },
    );
    let sources = resolve_prompt_sources(&config, Some(user_dir.path()))
        .expect("同梱プリセットのみで解決できるはずです");

    let catalog = build_catalog(&build_input(&config, &user_dir, &[], &[]))
        .expect("必須部品が揃いカタログは構築できるはずです");
    let prompt = catalog
        .system_prompt_for(Role::Orchestrator, Some(CATEGORY), "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");

    let scoped_body = sources.appendices[CATEGORY_APPENDIX_PRESET].trim_end();
    let role_body = sources.appendices[ROLE_APPENDIX_PRESET].trim_end();
    assert!(
        prompt.ends_with(scoped_body),
        "appendix レイヤーはカテゴリスコープ preset の本文のはずです"
    );
    assert!(
        !prompt.contains(role_body),
        "ロール直下 preset の本文はカテゴリスコープに置き換わるはずです"
    );
    assert!(
        prompt.contains(sources.category_overlays[CATEGORY].trim_end()),
        "quick overlay は sources どおりに現れるはずです"
    );
    assert!(
        prompt.contains(sources.role_baselines["orchestrator"].trim_end()),
        "role baseline は sources どおりに現れるはずです"
    );
}

// Given: 同梱・ユーザーのどちらにも存在しないプリセット名を参照する設定
// When: build_catalog する
// Then: PresetResolution 経由の PresetNotFound の型付きエラーになり、カタログは
//       構築されない (provider 呼び出しより前に失敗する)
#[test]
fn production_composition_fail_closed_before_any_provider_call() {
    let user_dir = empty_user_dir();
    let mut config = Config::default();
    config.agents.orchestrator.preset = Some(MISSING_PRESET.to_owned());

    let error = build_catalog(&build_input(&config, &user_dir, &[], &[]))
        .expect_err("存在しないプリセット参照は解決に失敗するはずです");

    assert!(
        matches!(
            &error,
            PromptCompositionError::PresetResolution(ConfigError::PresetNotFound { name })
                if name == MISSING_PRESET
        ),
        "PresetResolution 経由の PresetNotFound のはずです: {error:?}"
    );
}

// Given: 正常設定から導出したリアルなプリセット本文センチネルと、存在しない
//        プリセットを参照する設定
// When: 後者で build_catalog / with_config_prompts する
// Then: いずれのエラー Display も本文センチネルを含まず、with_config_prompts
//       は Err を返してカタログを接続しない
#[test]
fn composition_error_display_never_contains_preset_body() {
    let user_dir = empty_user_dir();
    let mut good_config = Config::default();
    good_config.agents.orchestrator.preset = Some(ROLE_APPENDIX_PRESET.to_owned());
    let good_sources = resolve_prompt_sources(&good_config, Some(user_dir.path()))
        .expect("正常系は解決できるはずです");
    let body_sentinel = good_sources.appendices[ROLE_APPENDIX_PRESET]
        .lines()
        .next()
        .expect("appendix 本文は非空のはずです");

    let mut missing_config = Config::default();
    missing_config.agents.orchestrator.preset = Some(MISSING_PRESET.to_owned());
    let composition_error = build_catalog(&build_input(&missing_config, &user_dir, &[], &[]))
        .expect_err("存在しないプリセット参照は失敗するはずです");
    assert!(
        !composition_error.to_string().contains(body_sentinel),
        "composition エラー Display にプリセット本文を含まないはずです"
    );

    let bus = Arc::new(EventBus::new(8));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let Err(convenience_error) =
        AgentRuntime::new(Arc::clone(&bus), executor, Arc::new(ScriptedModel::new([])))
            .with_config_prompts(&build_input(&missing_config, &user_dir, &[], &[]))
    else {
        panic!("カタログ接続は解決失敗でエラーになるはずです");
    };
    assert!(
        matches!(
            &convenience_error,
            PromptCompositionError::PresetResolution(ConfigError::PresetNotFound { name })
                if name == MISSING_PRESET
        ),
        "with_config_prompts は PresetResolution で失敗するはずです: {convenience_error:?}"
    );
    assert!(
        !convenience_error.to_string().contains(body_sentinel),
        "with_config_prompts のエラー Display にプリセット本文を含まないはずです"
    );
}

// Given: 利用可能 agent と skill のメタデータを渡した build_catalog
// When: Orchestrator (Intent Gate 挿入対象) の system prompt を解決する
// Then: keyTriggers ブロックに既定 4 ロールに加え agent/skill 名が載る
#[test]
fn availability_triggers_flow_into_intent_gate() {
    let user_dir = empty_user_dir();
    let config = Config::default();
    let agents = [AvailableAgent {
        name: "TestAgent".to_owned(),
        description: "test-agent-desc".to_owned(),
    }];
    let skills = [AvailableSkill {
        name: "TestSkill".to_owned(),
        description: "test-skill-desc".to_owned(),
    }];

    let catalog = build_catalog(&build_input(&config, &user_dir, &agents, &skills))
        .expect("必須部品が揃いカタログは構築できるはずです");
    let prompt = catalog
        .system_prompt_for(Role::Orchestrator, None, "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");

    let block = key_triggers_block(&prompt);
    for name in [
        "Orchestrator",
        "Explorer",
        "Worker",
        "Reviewer",
        "TestAgent",
        "TestSkill",
    ] {
        assert!(
            block.contains(&format!("- {name}:")),
            "keyTriggers ブロックに {name} が載るはずです"
        );
    }
}

/// プロンプトから keyTriggers ブロック (マーカー間) を取り出す。
fn key_triggers_block(prompt: &str) -> &str {
    const BEGIN: &str = "<!-- keyTriggers BEGIN -->";
    const END: &str = "<!-- keyTriggers END -->";
    let start = prompt
        .find(BEGIN)
        .expect("Orchestrator には keyTriggers ブロックがあるはずです");
    let stop = prompt[start..]
        .find(END)
        .expect("keyTriggers ブロックは閉じられているはずです");
    &prompt[start..start + stop]
}

// Given: repo scope に有効な skill metadata と、本文に公開禁止 sentinel を持つ SKILL.md
// When: discover_skills から build_catalog を経て Orchestrator / Worker の prompt を解決する
// Then: Orchestrator には name と description のみが現れ、本文と Worker には skill metadata が現れない
#[test]
fn skills_metadata_exposed_but_body_never_loaded_into_prompt() {
    let repo_dir = TempDir::new().expect("一時リポジトリディレクトリを作成できるはずです");
    let skill_dir = repo_dir.path().join("demo-skill");
    std::fs::create_dir_all(&skill_dir).expect("skill ディレクトリを作成できるはずです");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: Demo skill description\n---\nBODY-SENTINEL-DO-NOT-EXPOSE",
    )
    .expect("SKILL.md を作成できるはずです");

    let registry = discover_skills(&[(SkillScope::Repo, repo_dir.path().to_owned())]);
    let available_skills = registry.available_skills();
    assert_eq!(available_skills.len(), 1);
    assert_eq!(available_skills[0].name, "demo-skill");
    assert_eq!(available_skills[0].description, "Demo skill description");

    let user_dir = empty_user_dir();
    let config = Config::default();
    let catalog = build_catalog(&build_input(&config, &user_dir, &[], &available_skills))
        .expect("必須部品が揃いカタログは構築できるはずです");
    let orchestrator_prompt = catalog
        .system_prompt_for(Role::Orchestrator, None, "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");
    let worker_prompt = catalog
        .system_prompt_for(Role::Worker, None, "claude-opus-4-1")
        .expect("登録済みの部品のみを参照するはずです");

    assert!(orchestrator_prompt.contains("demo-skill"));
    assert!(orchestrator_prompt.contains("Demo skill description"));
    assert!(!orchestrator_prompt.contains("BODY-SENTINEL-DO-NOT-EXPOSE"));
    assert!(!worker_prompt.contains("demo-skill"));
    assert!(!worker_prompt.contains("BODY-SENTINEL-DO-NOT-EXPOSE"));
}

// Given: 同一 config から、空の skills metadata で構築した catalog と skills 入力なしの baseline
// When: Orchestrator / Worker の system prompt をそれぞれ解決する
// Then: 空 skills の出力は baseline とバイト単位で一致し、空入力で prompt は変化しない
#[test]
fn empty_skills_prompt_is_byte_identical_to_baseline() {
    let user_dir = empty_user_dir();
    let config = Config::default();
    let empty_skills: [AvailableSkill; 0] = [];
    let demo_skills = [AvailableSkill {
        name: "demo-skill".to_owned(),
        description: "Demo skill description".to_owned(),
    }];

    let empty_catalog = build_catalog(&build_input(&config, &user_dir, &[], &empty_skills))
        .expect("必須部品が揃いカタログは構築できるはずです");
    let skills_catalog = build_catalog(&build_input(&config, &user_dir, &[], &demo_skills))
        .expect("skill metadata 付きでもカタログは構築できるはずです");
    let baseline_catalog = build_catalog(&build_input(&config, &user_dir, &[], &[]))
        .expect("skills なしの baseline は構築できるはずです");

    for role in [Role::Orchestrator, Role::Worker] {
        let empty_prompt = empty_catalog
            .system_prompt_for(role, None, "claude-opus-4-1")
            .expect("登録済みの部品のみを参照するはずです");
        let baseline_prompt = baseline_catalog
            .system_prompt_for(role, None, "claude-opus-4-1")
            .expect("登録済みの部品のみを参照するはずです");
        assert_eq!(empty_prompt, baseline_prompt, "role = {}", role.name());

        let skills_prompt = skills_catalog
            .system_prompt_for(role, None, "claude-opus-4-1")
            .expect("登録済みの部品のみを参照するはずです");
        assert_eq!(empty_prompt == skills_prompt, role == Role::Worker);
    }
}
