mod support;

use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, Role as MessageRole, ToolResultContent};
use runtime::{AgentRuntime, ProjectTrust, RulesSettings, RulesSource, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use tools::ToolExecutor;

use support::{ScriptedModel, text_response, tool_response};

const RULE_MARKER: &str = "RULES-INJECTION-UNIQUE-MARKER";
const TARGET_CONTENT: &str = "fn wave_two_target() {}\n";

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
    let settings = RulesSettings {
        context_window_tokens: 200_000,
        response_headroom_tokens: 16_384,
        max_injection_bytes: 65_536,
    };
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
