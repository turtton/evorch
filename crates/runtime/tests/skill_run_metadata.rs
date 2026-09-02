//! run 開始時の skill metadata 露出の結合テスト (issue #53 / AC4)。
//!
//! `with_config_prompts_and_skills` が discovery 結果を catalog の keyTriggers
//! metadata として接続することを、実際の Orchestrator run の最初の complete
//! 呼び出しの System メッセージで検証する。run 開始時に prompt へ露出するのは
//! name+description の metadata 行のみで、SKILL.md 本文は決して現れない。

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use agents::Role;
use config::Config;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, Role as MessageRole};
use runtime::skill::SkillScope;
use runtime::{AgentRuntime, CatalogBuildInput, RunConfig};
use sandbox::DirectSandbox;
use tempfile::TempDir;
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

const SKILL_BODY_SENTINEL: &str = "SKILL-BODY-SENTINEL-NOT-IN-PROMPT";

/// 既定 config の catalog と指定 skill dirs の discovery を接続した runtime。
///
/// `user_presets_dir: None` でも既定 config は同梱プリセットのみで解決できる。
fn runtime_with(model: Arc<ScriptedModel>, skill_dirs: &[(SkillScope, PathBuf)]) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let config = Config::default();
    AgentRuntime::new(bus, executor, model)
        .with_config_prompts_and_skills(
            &CatalogBuildInput {
                config: &config,
                user_presets_dir: None,
                available_agents: &[],
                available_skills: &[],
            },
            skill_dirs,
        )
        .expect("既定 config と同梱プリセットのみでカタログは構築できるはずです")
}

fn first_system_text(observed: &[Vec<providers::Message>]) -> &str {
    let first_call = observed.first().expect("complete は 1 回以上観測される");
    assert_eq!(
        first_call.len(),
        2,
        "履歴は [System, User] で始まるはずです"
    );
    assert_eq!(
        first_call[0].role,
        MessageRole::System,
        "最初のメッセージは System のはずです"
    );
    assert_eq!(first_call[1].role, MessageRole::User);
    match &first_call[0].content[0] {
        ContentBlock::Text { text } => text,
        _ => panic!("System メッセージの先頭ブロックは Text のはずです"),
    }
}

// Given: repo スコープに有効な skill `demo` (description "Demo skill"、本文は
//        公開禁止 sentinel) を 1 件持つ skills ディレクトリ
// When: with_config_prompts_and_skills で接続したランタイムで Orchestrator run
//       ("META-OBSERVE") を終端まで実行する
// Then: 最初の complete 呼び出しの System は keyTriggers の metadata 行
//       `- demo: Demo skill` を含み、本文 sentinel を含まない
#[tokio::test]
async fn run_start_exposes_skill_metadata_but_never_body() {
    let skills_root = TempDir::new().expect("一時ディレクトリを作成できるはずです");
    let skills_dir = skills_root.path().join("skills");
    let skill_dir = skills_dir.join("demo");
    std::fs::create_dir_all(&skill_dir).expect("skill ディレクトリを作成できるはずです");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: demo\ndescription: Demo skill\n---\n{SKILL_BODY_SENTINEL}"),
    )
    .expect("SKILL.md を作成できるはずです");

    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "META-OBSERVE",
            [Ok(text_response("done", FinishReason::Stop))],
        )
        .await;
    let runtime = runtime_with(model.clone(), &[(SkillScope::Repo, skills_dir)]);

    let run_id = runtime.delegate_background(
        Role::Orchestrator,
        "META-OBSERVE".to_string(),
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let system_text = first_system_text(&observed);
    // keyTriggers の箇条書き形式は `- {name}: {description}` なので、発見された
    // skill `demo` の metadata 行は `- demo: Demo skill` になる。
    assert!(
        system_text.contains("- demo: Demo skill"),
        "System には skill metadata 行 `- demo: Demo skill` が現れるはずです"
    );
    assert!(
        !system_text.contains(SKILL_BODY_SENTINEL),
        "SKILL.md 本文は run 開始時の prompt に現れないはずです"
    );
}

// Given: 存在しない skills ディレクトリ (scope ディレクトリの欠損は空として許容)
// When: 同じ配線で Orchestrator run ("META-OBSERVE") を終端まで実行する
// Then: run は Done で完了し、System には skill metadata 行が一切現れない
#[tokio::test]
async fn run_start_with_empty_skill_dirs_keeps_baseline_prompt() {
    let root = TempDir::new().expect("一時ディレクトリを作成できるはずです");
    let missing_dir = root.path().join("does-not-exist");

    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "META-OBSERVE",
            [Ok(text_response("done", FinishReason::Stop))],
        )
        .await;
    let runtime = runtime_with(model.clone(), &[(SkillScope::Repo, missing_dir)]);

    let run_id = runtime.delegate_background(
        Role::Orchestrator,
        "META-OBSERVE".to_string(),
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let system_text = first_system_text(&observed);
    assert!(
        !system_text.contains("- demo") && !system_text.contains("Demo skill"),
        "発見 skill が 0 件のとき metadata 行は現れないはずです"
    );
}
