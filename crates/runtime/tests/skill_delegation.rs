//! T10: 委譲時 load_skills パススルーの結合テスト (issue #53 / AC6, AC9)。
//!
//! delegate / delegate_background の `load_skills` 引数が子 run の初期
//! System メッセージ (単一) へ skill 本文セクションを注入すること、未知名や
//! 未接続レジストリを子 run の生成より前に拒否すること (fail-closed)、
//! `load_skills` 未指定の場合は v0.1 の System メッセージを変えないことを
//! ScriptedModel の観測結果で検証する。

// allow: SIZE_OK — 6 個の AC6/AC9 シナリオが共有する同期 helper を同一 integration suite に置く。

mod support;

use std::path::Path;
use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, Message, Role as MessageRole, ToolResultContent};
use runtime::prompt::SystemPromptCatalog;
use runtime::skill::{SkillRegistry, SkillScope, discover_skills};
use runtime::{AgentRuntime, RunConfig, RunId};
use sandbox::DirectSandbox;
use serde_json::json;
use tempfile::{TempDir, tempdir};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response, tool_response};

const SENTINEL: &str = "SKILL-SENTINEL-DEMO";
const EXPECTED_SKILLS_SECTION: &str =
    "## Skills\n\n<!-- skill:demo BEGIN -->\nSKILL-SENTINEL-DEMO body.\n\n<!-- skill:demo END -->";

fn complete_catalog() -> SystemPromptCatalog {
    let mut builder = SystemPromptCatalog::builder();
    for role in [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
    ] {
        builder = builder.role_baseline(role, format!("{}-BASELINE", role.name().to_uppercase()));
    }
    for family in [
        "claude",
        "openai-reasoning",
        "gpt5",
        "gemini",
        "kimi",
        "generic",
    ] {
        builder = builder.family_section(format!("family-{family}"), family.to_uppercase());
    }
    builder
        .build()
        .expect("テスト用カタログは必須部品をすべて満たす")
}

fn runtime_with(
    model: Arc<ScriptedModel>,
    catalog: Option<SystemPromptCatalog>,
    skills: Option<Arc<SkillRegistry>>,
) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let mut runtime = AgentRuntime::new(bus, executor, model);
    if let Some(catalog) = catalog {
        runtime = runtime.with_system_prompts(Arc::new(catalog));
    }
    if let Some(skills) = skills {
        runtime = runtime.with_skills(skills);
    }
    runtime
}

/// name/description/body から SKILL.md を持つ skill ディレクトリを作る。
fn write_skill(dir: &Path, name: &str, description: &str, body: &str) {
    let skill_dir = dir.join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
    )
    .unwrap();
}

/// 本文に sentinel を含む有効な `demo` skill 1 件のレジストリを組み立てる。
/// TempDir は load_body が実行時ディスク読み込みするため呼び出し側で保持する。
fn registry_with_demo_skill() -> (SkillRegistry, TempDir) {
    let root = tempdir().unwrap();
    let skills = root.path().join("skills");
    write_skill(&skills, "demo", "Demo skill", "SKILL-SENTINEL-DEMO body.\n");
    let registry = discover_skills(&[(SkillScope::Repo, skills)]);
    (registry, root)
}

/// Orchestrator が load_skills 付き delegate_background を呼ぶスクリプトを登録する。
/// `None` なら引数自体を省略する (AC9 の未指定経路)。
async fn orchestrator_delegate_background_script(
    model: &ScriptedModel,
    load_skills: Option<&[&str]>,
) {
    let mut args = json!({ "role": "worker", "prompt": "W1" });
    if let Some(names) = load_skills {
        args["load_skills"] = json!(names);
    }
    let script = [
        Ok(tool_response(
            "delegate-worker",
            "delegate_background",
            args,
        )),
        Ok(text_response("all done", FinishReason::Stop)),
    ];
    model.add_keyed("ORCH", script).await;
}

async fn add_child_script(model: &ScriptedModel) {
    model
        .add_keyed("W1", [Ok(text_response("child done", FinishReason::Stop))])
        .await;
}

/// observed の 1 complete 呼び出しから、指定ロールの最初の Text ブロック本文を返す。
fn text_of_role(messages: &[Message], role: MessageRole) -> Option<&str> {
    messages
        .iter()
        .find(|message| message.role == role)
        .and_then(|message| match &message.content[0] {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
}

/// 指定ユーザプロンプトで始まる最初の complete 呼び出しを返す。
fn first_call_for_prompt<'a>(observed: &'a [Vec<Message>], prompt: &str) -> &'a [Message] {
    observed
        .iter()
        .find(|call| text_of_role(call, MessageRole::User) == Some(prompt))
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("ユーザプロンプト {prompt} の complete 呼び出しが観測される"))
}

/// 指定プロンプトで始まる run の System メッセージ本文を返す。
///
/// 最初のメッセージが System であること、System が 1 件のみであること
/// (単一 System 不変条件) をこの場で検証してから本文を返す。
fn single_system_text<'a>(observed: &'a [Vec<Message>], prompt: &str) -> &'a str {
    let call = first_call_for_prompt(observed, prompt);
    assert_eq!(call[0].role, MessageRole::System, "履歴は System で始まる");
    let system_count = call
        .iter()
        .filter(|message| message.role == MessageRole::System)
        .count();
    assert_eq!(system_count, 1, "System メッセージは 1 件のみ");
    text_of_role(call, MessageRole::System).expect("System メッセージの本文")
}

/// 指定ツール呼び出しに対するエラー ToolResult の内容を返す。
fn error_tool_result<'a>(call: &'a [Message], tool_call_id: &str) -> &'a [ToolResultContent] {
    call.iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id: id,
                content,
                is_error: true,
            } if id == tool_call_id => Some(content.as_slice()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("op はエラー ToolResult で拒否される"))
}

/// 親 run を終端まで実行し、子 run (直後の ID) も終端まで実行して観測を返す。
async fn run_parent_and_child(runtime: &AgentRuntime, model: &ScriptedModel) -> Vec<Vec<Message>> {
    let parent =
        runtime.delegate_background(Role::Orchestrator, "ORCH".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    let child = RunId::new(parent.get() + 1);
    assert_eq!(runtime.wait(child).await, Ok(AgentRunPhase::Done));
    model.observed().await
}

// Given: カタログと demo skill レジストリを接続したランタイム、load_skills ["demo"] の
//        delegate_background を呼ぶ Orchestrator スクリプト、子用スクリプト
// When: 親子両方の run を終端まで実行する
// Then: 子の最初の complete 呼び出しは System 1 件のみで、カタログ baseline と
//       skill 本文 sentinel を両方含む (AC6 + 単一 System 不変条件)
#[tokio::test]
async fn delegated_child_with_load_skills_composes_skill_body_into_single_system_message() {
    let model = Arc::new(ScriptedModel::new([]));
    orchestrator_delegate_background_script(&model, Some(&["demo"])).await;
    add_child_script(&model).await;
    let (registry, _root) = registry_with_demo_skill();
    let runtime = runtime_with(
        model.clone(),
        Some(complete_catalog()),
        Some(Arc::new(registry)),
    );

    let observed = run_parent_and_child(&runtime, &model).await;

    let system = single_system_text(&observed, "W1");
    assert!(
        system.contains("WORKER-BASELINE"),
        "子の System はカタログ baseline を含む: {system}"
    );
    assert!(
        system.contains(SENTINEL),
        "子の System は skill 本文を含む: {system}"
    );
    assert!(
        system.contains("## Skills"),
        "子の System は skills セクションヘッダを含む: {system}"
    );
}

// Given: demo skill レジストリに存在しない "nope" を load_skills に指定した
//        delegate_background (子用スクリプトは未登録)
// When: 親 run を終端まで実行する
// Then: 委譲 op は "unknown skill" を含むエラー ToolResult で拒否され、
//       子 run は登録もモデル呼び出しもされない (AC6 error-before-spawn)
#[tokio::test]
async fn delegate_background_rejects_unknown_skill_before_spawn() {
    let model = Arc::new(ScriptedModel::new([]));
    orchestrator_delegate_background_script(&model, Some(&["nope"])).await;
    let (registry, _root) = registry_with_demo_skill();
    let runtime = runtime_with(
        model.clone(),
        Some(complete_catalog()),
        Some(Arc::new(registry)),
    );

    let parent =
        runtime.delegate_background(Role::Orchestrator, "ORCH".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let second_turn = observed.get(1).expect("親の 2 回目の complete 呼び出し");
    let rejection = error_tool_result(second_turn, "delegate-worker");
    assert!(rejection.iter().any(|item| matches!(
        item,
        ToolResultContent::Text { text } if text.contains("unknown skill")
    )));
    assert_eq!(runtime.list_agents().len(), 1, "子 run は登録されない");
    assert!(
        !observed
            .iter()
            .any(|call| text_of_role(call, MessageRole::User) == Some("W1")),
        "子 run のモデル呼び出しは発生しない"
    );
}

// Given: skill レジストリ未接続のランタイムと load_skills ["demo"] の
//        delegate_background (子用スクリプトは未登録)
// When: 親 run を終端まで実行する
// Then: 委譲 op は "not configured" を含むエラー ToolResult で拒否され、
//       子 run は登録もモデル呼び出しもされない (AC6 error-before-spawn)
#[tokio::test]
async fn delegate_background_rejects_load_skills_without_registry_before_spawn() {
    let model = Arc::new(ScriptedModel::new([]));
    orchestrator_delegate_background_script(&model, Some(&["demo"])).await;
    let runtime = runtime_with(model.clone(), Some(complete_catalog()), None);

    let parent =
        runtime.delegate_background(Role::Orchestrator, "ORCH".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let second_turn = observed.get(1).expect("親の 2 回目の complete 呼び出し");
    let rejection = error_tool_result(second_turn, "delegate-worker");
    assert!(rejection.iter().any(|item| matches!(
        item,
        ToolResultContent::Text { text } if text.contains("not configured")
    )));
    assert_eq!(runtime.list_agents().len(), 1, "子 run は登録されない");
    assert!(
        !observed
            .iter()
            .any(|call| text_of_role(call, MessageRole::User) == Some("W1")),
        "子 run のモデル呼び出しは発生しない"
    );
}

// Given: カタログ未接続 (v0.1 構成) と demo skill レジストリ、load_skills
//        ["demo"] の delegate_background
// When: 親子両方の run を終端まで実行する
// Then: 子の System は skills セクションのみでカタログテキストを含まない
#[tokio::test]
async fn child_without_catalog_gets_skills_only_system_message() {
    let model = Arc::new(ScriptedModel::new([]));
    orchestrator_delegate_background_script(&model, Some(&["demo"])).await;
    add_child_script(&model).await;
    let (registry, _root) = registry_with_demo_skill();
    let runtime = runtime_with(model.clone(), None, Some(Arc::new(registry)));

    let observed = run_parent_and_child(&runtime, &model).await;

    let system = single_system_text(&observed, "W1");
    assert_eq!(system, EXPECTED_SKILLS_SECTION);
}

// Given: load_skills を指定しない同一スクリプトを、skill 配線なしのランタイムと
//        レジストリ接続済みのランタイムの両方で実行する
// When: 親子両方の run を終端まで実行する
// Then: 子の System メッセージは両ランタイムでバイト一致する
//       (AC9: skill 配線は未指定経路の System を変えない)
#[tokio::test]
async fn child_without_load_skills_keeps_pre_skills_baseline_system_message() {
    let model_without = Arc::new(ScriptedModel::new([]));
    orchestrator_delegate_background_script(&model_without, None).await;
    add_child_script(&model_without).await;
    let runtime_without = runtime_with(model_without.clone(), Some(complete_catalog()), None);
    let observed_without = run_parent_and_child(&runtime_without, &model_without).await;

    let model_with = Arc::new(ScriptedModel::new([]));
    orchestrator_delegate_background_script(&model_with, None).await;
    add_child_script(&model_with).await;
    let (registry, _root) = registry_with_demo_skill();
    let runtime_with_skills = runtime_with(
        model_with.clone(),
        Some(complete_catalog()),
        Some(Arc::new(registry)),
    );
    let observed_with = run_parent_and_child(&runtime_with_skills, &model_with).await;

    let system_without = single_system_text(&observed_without, "W1");
    let system_with = single_system_text(&observed_with, "W1");
    assert!(system_without.contains("WORKER-BASELINE"));
    assert_eq!(
        system_without, system_with,
        "load_skills 未指定の System は skill 配線なしとバイト一致する (AC9)"
    );
    assert!(!system_with.contains("skill:"));
}

// Given: load_skills ["demo"] の foreground delegate を呼ぶ Orchestrator スクリプト
// When: 親子両方の run を終端まで実行する
// Then: 子の最初の complete 呼び出しは System 1 件のみで、baseline と skill 本文を
//       両方含む (DelegateArgs 経路も load_skills を受け付ける)
#[tokio::test]
async fn foreground_delegate_with_load_skills_composes_skill_body_into_single_system_message() {
    let model = Arc::new(ScriptedModel::new([]));
    let script = [
        Ok(tool_response(
            "delegate-worker",
            "delegate",
            json!({ "role": "worker", "prompt": "W1", "load_skills": ["demo"] }),
        )),
        Ok(text_response("all done", FinishReason::Stop)),
    ];
    model.add_keyed("ORCH", script).await;
    add_child_script(&model).await;
    let (registry, _root) = registry_with_demo_skill();
    let runtime = runtime_with(
        model.clone(),
        Some(complete_catalog()),
        Some(Arc::new(registry)),
    );

    let observed = run_parent_and_child(&runtime, &model).await;

    let system = single_system_text(&observed, "W1");
    assert!(
        system.contains("WORKER-BASELINE") && system.contains(SENTINEL),
        "foreground delegate の子の System は baseline と skill 本文を含む: {system}"
    );
}
