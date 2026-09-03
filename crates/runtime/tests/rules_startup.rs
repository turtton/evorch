mod support;

// allow: SIZE_OK — issue #61 の startup/budget acceptance scenarios を 1 integration binary で検証する。

use std::path::Path;
use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, Message, Role as MessageRole};
use runtime::{AgentRuntime, ProjectTrust, RulesSettings, RulesSource, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use tools::ToolExecutor;

use support::{ScriptedModel, text_response, tool_response};

fn settings(max_injection_bytes: u64) -> RulesSettings {
    RulesSettings {
        context_window_tokens: 200_000,
        response_headroom_tokens: 16_384,
        max_injection_bytes,
    }
}

fn runtime_with_rules(model: Arc<ScriptedModel>, source: RulesSource) -> AgentRuntime {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    AgentRuntime::new(bus, executor, model).with_project_rules(Arc::new(source))
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("fixture path は親を持つ"))
        .expect("fixture ディレクトリを作成できる");
    std::fs::write(path, content).expect("fixture ファイルを書き込める");
}

fn text_of_role(messages: &[Message], role: MessageRole) -> Option<&str> {
    messages
        .iter()
        .find(|message| message.role == role)
        .and_then(|message| {
            message.content.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                ContentBlock::Reasoning { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
        })
}

fn rules_texts(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } if text.starts_with("[project-rules]") => {
                Some(text.as_str())
            }
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect()
}

// Given: root・nested・project scoped・user alwaysApply rules を持つ承認済み project
// When: catalog なしの runtime が最初の complete を呼ぶ
// Then: 単一 System は root と user rule のみを含む
#[tokio::test]
async fn startup_system_message_contains_root_and_user_always_apply_only() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let root = tmp.path().join("project");
    let user = tmp.path().join("user");
    write(&root.join("AGENTS.md"), "ROOT-MARK");
    write(&root.join("crates/AGENTS.md"), "NESTED-MARK");
    write(
        &root.join(".omo/rules/aa.md"),
        "---\nalwaysApply: true\n---\nSCOPED-MARK",
    );
    write(
        &user.join("always.md"),
        "---\nalwaysApply: true\n---\nUSER-MARK",
    );
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let runtime = runtime_with_rules(
        model.clone(),
        RulesSource::new(
            ProjectTrust::Approved,
            settings(65_536),
            Some(user),
            Some(root),
        ),
    );

    let run_id =
        runtime.delegate_background(Role::Worker, "prompt".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let first_call = observed.first().expect("最初の complete を観測できる");
    assert_eq!(
        first_call
            .iter()
            .filter(|message| message.role == MessageRole::System)
            .count(),
        1
    );
    let system = text_of_role(first_call, MessageRole::System).expect("System text が存在する");
    assert!(system.contains("ROOT-MARK"));
    assert!(system.contains("USER-MARK"));
    assert!(!system.contains("NESTED-MARK"));
    assert!(!system.contains("SCOPED-MARK"));
}

// Given: project root rule と user alwaysApply rule を持つ未承認 project
// When: 最初の complete を呼ぶ
// Then: System は user scope だけを含み project rule を含まない
#[tokio::test]
async fn unapproved_startup_loads_user_scope_only() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let root = tmp.path().join("project");
    let user = tmp.path().join("user");
    write(&root.join("AGENTS.md"), "ROOT-MARK");
    write(
        &user.join("always.md"),
        "---\nalwaysApply: true\n---\nUSER-MARK",
    );
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let runtime = runtime_with_rules(
        model.clone(),
        RulesSource::new(
            ProjectTrust::Unapproved,
            settings(65_536),
            Some(user),
            Some(root),
        ),
    );

    let run_id =
        runtime.delegate_background(Role::Worker, "prompt".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let system = text_of_role(
        observed.first().expect("最初の complete を観測できる"),
        MessageRole::System,
    )
    .expect("user rules の System text が存在する");
    assert!(system.contains("USER-MARK"));
    assert!(!system.contains("ROOT-MARK"));
}

// Given: catalog と skills がなく root project rule だけを持つ runtime
// When: 最初の complete を呼ぶ
// Then: 履歴は rules を含む単一 System、続いて prompt の User になる
#[tokio::test]
async fn rules_only_run_creates_single_system_message_when_no_catalog_or_skills() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    write(&tmp.path().join("AGENTS.md"), "ROOT-MARK");
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let runtime = runtime_with_rules(
        model.clone(),
        RulesSource::new(
            ProjectTrust::Approved,
            settings(65_536),
            None,
            Some(tmp.path().to_path_buf()),
        ),
    );

    let run_id =
        runtime.delegate_background(Role::Worker, "prompt".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let first_call = observed.first().expect("最初の complete を観測できる");
    assert_eq!(first_call.len(), 2);
    assert_eq!(first_call[0].role, MessageRole::System);
    assert_eq!(first_call[1].role, MessageRole::User);
    assert_eq!(
        first_call
            .iter()
            .filter(|message| message.role == MessageRole::System)
            .count(),
        1
    );
    assert!(
        matches!(&first_call[0].content[0], ContentBlock::Text { text } if text.contains("ROOT-MARK"))
    );
    assert!(matches!(&first_call[1].content[0], ContentBlock::Text { text } if text == "prompt"));
}

// Given: 長い root rule と短い最深 rule に対する小さい注入予算
// When: 同じ最深 target を 2 回 read する
// Then: 最深 rule と root 省略 marker が各回注入され、再アクセスで再注入される
#[tokio::test]
async fn tiny_budget_keeps_closest_rule_and_marks_omissions() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let target = tmp.path().join("crates/core/x.rs");
    write(
        &tmp.path().join("AGENTS.md"),
        &"ROOT-LONG-CONTENT ".repeat(40),
    );
    write(&tmp.path().join("crates/core/AGENTS.md"), "DEEP-CLOSEST");
    write(&target, "target");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("call-1", "read", json!({ "path": target }))),
        Ok(tool_response(
            "call-2",
            "read",
            json!({ "path": tmp.path().join("crates/core/x.rs") }),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let runtime = runtime_with_rules(
        model.clone(),
        RulesSource::new(
            ProjectTrust::Approved,
            settings(220),
            None,
            Some(tmp.path().to_path_buf()),
        ),
    );

    let run_id =
        runtime.delegate_background(Role::Worker, "read twice".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    assert_eq!(observed.len(), 3);
    let second_call_rules = rules_texts(&observed[1]);
    let third_call_rules = rules_texts(&observed[2]);
    assert_eq!(second_call_rules.len(), 1);
    assert_eq!(third_call_rules.len(), 2);
    for text in third_call_rules {
        assert!(text.contains("DEEP-CLOSEST"));
        assert!(!text.contains("ROOT-LONG-CONTENT"));
        assert!(text.contains(
            "- [rules omitted: AGENTS.md; re-read or grep the target path to re-inject]"
        ));
    }
}

async fn run_once_and_observe(root: &Path, target: &Path) -> Vec<Vec<Message>> {
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("call-1", "read", json!({ "path": target }))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let runtime = runtime_with_rules(
        model.clone(),
        RulesSource::new(
            ProjectTrust::Approved,
            settings(65_536),
            None,
            Some(root.to_path_buf()),
        ),
    );
    let run_id =
        runtime.delegate_background(Role::Worker, "read".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    model.observed().await
}

// Given: V1 の project rule を持つ fixture
// When: 1 run 後に rule を V2 へ編集し fresh runtime で再度 read する
// Then: 次の注入は V2 を含み V1 を含まない
#[tokio::test]
async fn edited_rules_file_reflected_in_next_injection() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let rule = tmp.path().join("AGENTS.md");
    let target = tmp.path().join("x.rs");
    write(&rule, "RULE-V1");
    write(&target, "target");

    let first = run_once_and_observe(tmp.path(), &target).await;
    let first_rules = rules_texts(first.last().expect("first run の最後の complete"));
    assert_eq!(first_rules.len(), 1);
    assert!(first_rules[0].contains("RULE-V1"));

    write(&rule, "RULE-V2");
    let second = run_once_and_observe(tmp.path(), &target).await;
    let second_rules = rules_texts(second.last().expect("second run の最後の complete"));
    assert_eq!(second_rules.len(), 1);
    assert!(second_rules[0].contains("RULE-V2"));
    assert!(!second_rules[0].contains("RULE-V1"));
}

// Given: 日本語で予算超過する root と最深 rule
// When: 小さい予算で最深 target の read が成功する
// Then: UTF-8 replacement なしで最深 marker と truncation marker を含む 1 件が注入される
#[tokio::test]
async fn truncation_respects_utf8_boundary_and_keeps_deep_rule() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let target = tmp.path().join("crates/core/x.rs");
    write(&tmp.path().join("AGENTS.md"), &"親規則日本語".repeat(40));
    write(
        &tmp.path().join("crates/core/AGENTS.md"),
        &format!("DEEP-CLOSEST-{}", "最深規則日本語".repeat(40)),
    );
    write(&target, "target");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("call-1", "read", json!({ "path": target }))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let runtime = runtime_with_rules(
        model.clone(),
        RulesSource::new(
            ProjectTrust::Approved,
            settings(180),
            None,
            Some(tmp.path().to_path_buf()),
        ),
    );

    let run_id =
        runtime.delegate_background(Role::Worker, "read".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let injected = rules_texts(observed.last().expect("最後の complete を観測できる"));
    assert_eq!(injected.len(), 1);
    assert!(injected[0].contains("DEEP-CLOSEST-"));
    assert!(injected[0].contains("[rules truncated: crates/core/AGENTS.md]"));
    assert!(!injected[0].contains('\u{fffd}'));
}
