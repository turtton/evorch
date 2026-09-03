mod support;

// allow: SIZE_OK — issue #61 の post-tool acceptance scenarios を 1 integration binary で検証する。

use std::path::Path;
use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus, EventKind, ToolEvent};
use providers::{ContentBlock, FinishReason, Message, Role as MessageRole, ToolResultContent};
use runtime::{AgentRuntime, ProjectTrust, RulesSettings, RulesSource, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response, tool_response, tool_responses};

const RULE_MARKER: &str = "RULES-INJECTION-UNIQUE-MARKER";
const TARGET_CONTENT: &str = "fn wave_two_target() {}\n";

fn settings(max_injection_bytes: u64) -> RulesSettings {
    RulesSettings {
        context_window_tokens: 200_000,
        response_headroom_tokens: 16_384,
        max_injection_bytes,
    }
}

fn runtime_with_rules(
    model: Arc<ScriptedModel>,
    trust: ProjectTrust,
    project_root: &Path,
) -> (AgentRuntime, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model).with_project_rules(
        Arc::new(RulesSource::new(
            trust,
            settings(65_536),
            None,
            Some(project_root.to_path_buf()),
        )),
    );
    (runtime, bus)
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

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("fixture path は親を持つ"))
        .expect("fixture ディレクトリを作成できる");
    std::fs::write(path, content).expect("fixture ファイルを書き込める");
}

// Given: root AGENTS.md と深い read 対象を持つ承認済み project rules runtime
// When: read が成功して次のモデル呼び出しへ進む
// Then: ToolResult の後ろに rules を含む User メッセージが 1 件だけ注入される
#[tokio::test]
async fn read_success_injects_single_user_rules_message() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let target = tmp.path().join("src/deep/x.rs");
    std::fs::create_dir_all(target.parent().expect("対象ファイルは親を持つ"))
        .expect("対象ディレクトリを作成できる");
    std::fs::write(tmp.path().join("AGENTS.md"), RULE_MARKER).expect("project rules を書き込める");
    std::fs::write(&target, TARGET_CONTENT).expect("read 対象を書き込める");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "call-1",
            "read",
            json!({ "path": target.to_string_lossy() }),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let settings = settings(65_536);
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model.clone()).with_project_rules(
        Arc::new(RulesSource::new(
            ProjectTrust::Approved,
            settings,
            None,
            Some(tmp.path().to_path_buf()),
        )),
    );

    let run_id =
        runtime.delegate_background(Role::Worker, "read it".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let last_call = observed
        .last()
        .expect("最後の complete 呼び出しを観測できる");
    let tool_result_index = last_call
        .iter()
        .position(|message| {
            message.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolResult {
                        tool_call_id,
                        content,
                        is_error: false,
                    } if tool_call_id == "call-1"
                        && content.iter().any(|item| matches!(
                            item,
                            ToolResultContent::Text { text } if text.contains(TARGET_CONTENT)
                        ))
                )
            })
        })
        .expect("成功した read の ToolResult が元のファイル内容を保持する");
    let injected: Vec<_> = last_call
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == MessageRole::User
                && message.content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text { text }
                            if text.contains("[project-rules]") && text.contains(RULE_MARKER)
                    )
                })
        })
        .collect();

    assert_eq!(
        injected.len(),
        1,
        "rules User メッセージは 1 件だけ存在する"
    );
    assert!(
        injected[0].0 > tool_result_index,
        "rules User メッセージは ToolResult の後ろに存在する"
    );
}

// Given: project root 外・root・中間・最深・sibling に異なる AGENTS.md を持つ fixture
// When: 最深 target の read が成功する
// Then: root から最深まで順序通り 1 回注入され、root 外と sibling は含まれない
#[tokio::test]
async fn nested_chain_injected_root_to_deep_caps_at_root_excludes_siblings() {
    let outer = tempfile::tempdir().expect("外側一時ディレクトリを作成できる");
    let root = outer.path().join("proj");
    let target = root.join("crates/core/src/x.rs");
    write(&outer.path().join("AGENTS.md"), "MARKER-ABOVE");
    write(&root.join("AGENTS.md"), "MARKER-ROOT");
    write(&root.join("crates/AGENTS.md"), "MARKER-MID");
    write(&root.join("crates/core/AGENTS.md"), "MARKER-DEEP");
    write(&root.join("other/AGENTS.md"), "MARKER-SIBLING");
    write(&target, TARGET_CONTENT);
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("call-1", "read", json!({ "path": target }))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _bus) = runtime_with_rules(model.clone(), ProjectTrust::Approved, &root);

    let run_id =
        runtime.delegate_background(Role::Worker, "read".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let last_call = observed.last().expect("最後の complete を観測できる");
    let injected = rules_texts(last_call);
    assert_eq!(injected.len(), 1);
    let text = injected[0];
    let root_pos = text.find("MARKER-ROOT").expect("root rule が注入される");
    let mid_pos = text.find("MARKER-MID").expect("mid rule が注入される");
    let deep_pos = text.find("MARKER-DEEP").expect("deep rule が注入される");
    assert!(root_pos < mid_pos && mid_pos < deep_pos);
    assert!(!text.contains("MARKER-SIBLING"));
    assert!(!text.contains("MARKER-ABOVE"));
}

// Given: root rule を共有する a と b の target を持つ fixture
// When: 1 回の assistant 応答で 2 件の read が成功する
// Then: rules メッセージは 1 件で共有 root は重複せず a と b の規則を含む
#[tokio::test]
async fn two_targets_one_batch_single_deduped_injection() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let a = tmp.path().join("a/x.rs");
    let b = tmp.path().join("b/y.rs");
    write(&tmp.path().join("AGENTS.md"), "MARKER-ROOT");
    write(&tmp.path().join("a/AGENTS.md"), "MARKER-A");
    write(&tmp.path().join("b/AGENTS.md"), "MARKER-B");
    write(&a, "a");
    write(&b, "b");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_responses([
            ("call-1", "read", json!({ "path": a })),
            ("call-2", "read", json!({ "path": b })),
        ])),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _bus) = runtime_with_rules(model.clone(), ProjectTrust::Approved, tmp.path());

    let run_id =
        runtime.delegate_background(Role::Worker, "read both".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let injected = rules_texts(observed.last().expect("最後の complete を観測できる"));
    assert_eq!(injected.len(), 1);
    assert_eq!(injected[0].matches("MARKER-ROOT").count(), 1);
    assert!(injected[0].contains("MARKER-A"));
    assert!(injected[0].contains("MARKER-B"));
}

// Given: alwaysApply・一致 glob・不一致 glob・不正 glob の scoped rules
// When: crates/core/x.rs の read が成功する
// Then: 適用対象だけを注入し、不正 glob はパス付き disabled marker で fail-closed になる
#[tokio::test]
async fn scoped_rules_dirs_always_apply_and_glob_fail_closed() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let target = tmp.path().join("crates/core/x.rs");
    let invalid = tmp.path().join(".github/instructions/i.instructions.md");
    write(&target, TARGET_CONTENT);
    write(
        &tmp.path().join(".omo/rules/a.md"),
        "---\nalwaysApply: true\n---\nMARKED-OMO-ALWAYS",
    );
    write(
        &tmp.path().join(".claude/rules/g.md"),
        "---\nglobs: ['crates/**']\n---\nMARKED-CLAUDE-GLOB",
    );
    write(
        &tmp.path().join(".cursor/rules/n.md"),
        "---\nglobs: ['nomatch/**']\n---\nMARKED-NOMATCH",
    );
    write(&invalid, "---\nglobs: ['[unclosed']\n---\nMARKED-INVALID");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("call-1", "read", json!({ "path": target }))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _bus) = runtime_with_rules(model.clone(), ProjectTrust::Approved, tmp.path());

    let run_id =
        runtime.delegate_background(Role::Worker, "read".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let injected = rules_texts(observed.last().expect("最後の complete を観測できる"));
    assert_eq!(injected.len(), 1);
    let text = injected[0];
    assert!(text.contains("MARKED-OMO-ALWAYS"));
    assert!(text.contains("MARKED-CLAUDE-GLOB"));
    assert!(!text.contains("MARKED-NOMATCH"));
    assert!(!text.contains("MARKED-INVALID"));
    assert!(text.contains(&format!(
        "- [rules disabled: invalid rules glob: {}]",
        invalid.display()
    )));
}

// Given: project rule を持つ未承認 project
// When: read が成功する
// Then: どの complete 呼び出しにも post-tool rules は注入されない
#[tokio::test]
async fn unapproved_project_never_injects_post_tool() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let target = tmp.path().join("x.rs");
    write(&tmp.path().join("AGENTS.md"), "PROJECT-SECRET");
    write(&target, TARGET_CONTENT);
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("call-1", "read", json!({ "path": target }))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _bus) = runtime_with_rules(model.clone(), ProjectTrust::Unapproved, tmp.path());

    let run_id =
        runtime.delegate_background(Role::Worker, "read".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    assert!(
        model
            .observed()
            .await
            .iter()
            .all(|call| rules_texts(call).is_empty())
    );
}

async fn run_read_fixture(
    target: &Path,
    project_root: &Path,
    rules_enabled: bool,
) -> (Vec<ToolEvent>, Vec<ToolResultContent>) {
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("call-1", "read", json!({ "path": target }))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = if rules_enabled {
        AgentRuntime::new(Arc::clone(&bus), executor, model.clone()).with_project_rules(Arc::new(
            RulesSource::new(
                ProjectTrust::Approved,
                settings(65_536),
                None,
                Some(project_root.to_path_buf()),
            ),
        ))
    } else {
        AgentRuntime::new(Arc::clone(&bus), executor, model.clone())
    };
    let mut receiver = bus.subscribe();

    let run_id =
        runtime.delegate_background(Role::Worker, "read".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));
    let mut tool_events = Vec::new();
    while tool_events.len() < 2 {
        let event = timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("tool event を待機できる")
            .expect("event receiver は開いている");
        if let EventKind::Tool(
            event @ (ToolEvent::ToolStarted { .. } | ToolEvent::ToolCompleted { .. }),
        ) = event.kind
        {
            tool_events.push(event);
        }
    }
    let observed = model.observed().await;
    let result = observed
        .last()
        .expect("最後の complete を観測できる")
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error: false,
            } if tool_call_id == "call-1" => Some(content.clone()),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .expect("成功した read の ToolResult を観測できる");
    (tool_events, result)
}

// Given: 同一ファイルを読む rules 無効 run と有効 run
// When: 両方を実エージェントループで完了する
// Then: tool events・ToolResult・ディスク内容は rules 注入の有無で変化しない
#[tokio::test]
async fn injection_does_not_alter_tool_result_events_or_disk() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let target = tmp.path().join("x.rs");
    let original = b"fixture bytes\n";
    write(&tmp.path().join("AGENTS.md"), "MARKER-ROOT");
    std::fs::write(&target, original).expect("fixture を書き込める");

    let (without_events, without_result) = run_read_fixture(&target, tmp.path(), false).await;
    let (with_events, with_result) = run_read_fixture(&target, tmp.path(), true).await;

    assert_eq!(without_events, with_events);
    assert_eq!(without_result, with_result);
    assert_eq!(
        std::fs::read(&target).expect("fixture を再読できる"),
        original
    );
}

// Given: project rule と存在しない read target
// When: failed read の後に Stop する
// Then: run は完了し、rules は注入されない
#[tokio::test]
async fn failed_read_injects_nothing() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    write(&tmp.path().join("AGENTS.md"), "MARKER-ROOT");
    let missing = tmp.path().join("missing.rs");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "call-read",
            "read",
            json!({ "path": missing }),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _bus) = runtime_with_rules(model.clone(), ProjectTrust::Approved, tmp.path());

    let run_id = runtime.delegate_background(Role::Worker, "try".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    assert!(
        model
            .observed()
            .await
            .iter()
            .all(|call| rules_texts(call).is_empty())
    );
}

// Given: project rule を持つ approved runtime
// When: shell 呼び出しが成功して Stop する
// Then: run は完了し、shell 後にも rules は注入されない
#[tokio::test]
async fn shell_is_never_a_trigger() {
    let tmp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    write(&tmp.path().join("AGENTS.md"), "MARKER-ROOT");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "call-shell",
            "shell",
            json!({ "command": "ls" }),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _bus) = runtime_with_rules(model.clone(), ProjectTrust::Approved, tmp.path());

    let run_id =
        runtime.delegate_background(Role::Worker, "shell".to_string(), RunConfig::default());
    assert_eq!(runtime.wait(run_id).await, Ok(AgentRunPhase::Done));

    assert!(
        model
            .observed()
            .await
            .iter()
            .all(|call| rules_texts(call).is_empty())
    );
}
