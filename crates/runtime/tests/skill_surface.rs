//! T9: `skill_load` メタ操作のエンドツーエンド挙動 (issue #53, AC5/AC8)。
//!
//! skill レジストリ接続済みランタイムの Worker が stage 2 (本文) / stage 3
//! (リソース) を取得できること、および capability 外ロール (Reviewer) は
//! ADR 0002 の capability-denied で拒否されること、レジストリ未接続ランタイム
//! では fail-closed でエラーになることをランタイムの実ループ上で検証する。

mod support;

use std::fs;
use std::path::Path;
use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, ToolResultContent};
use runtime::skill::{SkillRegistry, SkillScope, discover_skills};
use runtime::{AgentRuntime, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use tempfile::tempdir;
use tools::ToolExecutor;

use support::{ScriptedModel, text_response, tool_response};

const BODY_SENTINEL: &str = "DEMO BODY SENTINEL\n";
const RESOURCE_SENTINEL: &str = "DEMO REFERENCE SENTINEL\n";

/// name/description/body から SKILL.md を持つ skill ディレクトリを作る。
fn write_skill(dir: &Path, name: &str, description: &str, body: &str) {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
    )
    .unwrap();
}

/// 本文と references/note.md を持つ `demo` skill 1 件のレジストリを組み立てる。
fn demo_registry() -> (SkillRegistry, tempfile::TempDir) {
    let root = tempdir().unwrap();
    let skills = root.path().join("skills");
    write_skill(&skills, "demo", "Demo skill", BODY_SENTINEL);
    let references = skills.join("demo").join("references");
    fs::create_dir_all(&references).unwrap();
    fs::write(references.join("note.md"), RESOURCE_SENTINEL).unwrap();
    let registry = discover_skills(&[(SkillScope::Repo, skills)]);
    assert!(registry.diagnostics.is_empty());
    (registry, root)
}

fn runtime_with(model: Arc<ScriptedModel>) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(128));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    AgentRuntime::new(bus, executor, model)
}

/// モデル履歴から指定 tool_call_id の ToolResult 本文と is_error を取り出す。
fn tool_result(messages: &[providers::Message], call_id: &str) -> Option<(String, bool)> {
    messages.iter().find_map(|message| {
        message.content.iter().find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == call_id => content.first().map(|item| match item {
                ToolResultContent::Text { text } => (text.clone(), *is_error),
            }),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
    })
}

// Given: demo skill を持つレジストリ接続済みランタイムと skill_load を 1 回
//        要求して停止する Worker
// When:  {"name":"demo"} で skill_load を実行する (stage 2)
// Then:  ToolResult success の内容が SKILL.md の本文そのものになる (AC5)
#[tokio::test]
async fn worker_skill_load_returns_skill_body() {
    let (registry, _root) = demo_registry();
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "SKILL-BODY",
            [
                Ok(tool_response("c1", "skill_load", json!({ "name": "demo" }))),
                Ok(text_response("done", FinishReason::Stop)),
            ],
        )
        .await;
    let runtime = runtime_with(Arc::clone(&model)).with_skills(Arc::new(registry));

    let run =
        runtime.delegate_background(Role::Worker, "SKILL-BODY".to_string(), RunConfig::default());

    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    let observed = model.observed().await;
    let final_turn = observed.last().expect("worker final model turn");
    assert_eq!(
        tool_result(final_turn, "c1"),
        Some((BODY_SENTINEL.to_string(), false))
    );
}

// Given: references/note.md を持つ demo skill と skill_load を 1 回要求する Worker
// When:  {"name":"demo","resource":"references/note.md"} で skill_load を実行する (stage 3)
// Then:  ToolResult success の内容がリソースファイルの内容そのものになる (AC5)
#[tokio::test]
async fn worker_skill_load_with_resource_returns_resource_content() {
    let (registry, _root) = demo_registry();
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "SKILL-RESOURCE",
            [
                Ok(tool_response(
                    "c1",
                    "skill_load",
                    json!({ "name": "demo", "resource": "references/note.md" }),
                )),
                Ok(text_response("done", FinishReason::Stop)),
            ],
        )
        .await;
    let runtime = runtime_with(Arc::clone(&model)).with_skills(Arc::new(registry));

    let run = runtime.delegate_background(
        Role::Worker,
        "SKILL-RESOURCE".to_string(),
        RunConfig::default(),
    );

    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    let observed = model.observed().await;
    let final_turn = observed.last().expect("worker final model turn");
    assert_eq!(
        tool_result(final_turn, "c1"),
        Some((RESOURCE_SENTINEL.to_string(), false))
    );
}

// Given: skill_load が capability 外の Reviewer (レジストリは接続済み)
// When:  Reviewer が skill_load を強制呼び出しする
// Then:  ToolResult error となり ADR 0002 の capability-denied 識別子を載せる (AC8)
#[tokio::test]
async fn reviewer_skill_load_is_capability_denied() {
    let (registry, _root) = demo_registry();
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "REVIEW",
            [
                Ok(tool_response("c1", "skill_load", json!({ "name": "demo" }))),
                Ok(text_response("done", FinishReason::Stop)),
            ],
        )
        .await;
    let runtime = runtime_with(Arc::clone(&model)).with_skills(Arc::new(registry));

    let run =
        runtime.delegate_background(Role::Reviewer, "REVIEW".to_string(), RunConfig::default());

    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    let observed = model.observed().await;
    let final_turn = observed.last().expect("reviewer final model turn");
    let (content, is_error) = tool_result(final_turn, "c1").expect("skill_load result");
    assert!(is_error);
    assert!(
        content.contains("ADR 0002"),
        "capability-denied 識別子が欠落: {content}"
    );
    assert!(
        content.contains("skill_load"),
        "拒否対象ツール名が欠落: {content}"
    );
}

// Given: skill レジストリ未接続のランタイムと skill_load を 1 回要求する Worker
// When:  Worker が skill_load を強制呼び出しする
// Then:  ToolResult error となり "not configured" の fail-closed 識別子を載せる
#[tokio::test]
async fn skill_load_without_registry_fails_closed() {
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "NO-REGISTRY",
            [
                Ok(tool_response("c1", "skill_load", json!({ "name": "demo" }))),
                Ok(text_response("done", FinishReason::Stop)),
            ],
        )
        .await;
    let runtime = runtime_with(Arc::clone(&model));

    let run = runtime.delegate_background(
        Role::Worker,
        "NO-REGISTRY".to_string(),
        RunConfig::default(),
    );

    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));
    let observed = model.observed().await;
    let final_turn = observed.last().expect("worker final model turn");
    let (content, is_error) = tool_result(final_turn, "c1").expect("skill_load result");
    assert!(is_error);
    assert!(
        content.contains("not configured"),
        "fail-closed 識別子が欠落: {content}"
    );
}
