//! A provider may omit usage (Codex does). The placeholder zero must not make the
//! post-tool rules injection budget believe the whole context window is free.
mod support;

use std::path::Path;
use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, Usage};
use runtime::{AgentRuntime, ProjectTrust, RulesSettings, RulesSource, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

const HEAD_MARKER: &str = "RULES-HEAD-MARKER";
const TAIL_MARKER: &str = "RULES-TAIL-MARKER";
const WINDOW_TOKENS: u64 = 12_000;

/// A tool-use turn whose own text is ~11k estimated tokens, so only ~1k of the window is left.
fn big_tool_turn(target: &Path, usage: Usage, usage_reported: bool) -> ChatResponse {
    ChatResponse {
        usage_reported,
        message: Message {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "x".repeat(44_000),
                },
                ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "read".into(),
                    input: json!({ "path": target }),
                },
            ],
        },
        usage,
        finish_reason: FinishReason::ToolUse,
    }
}

/// The text of the one rules message the model sees on its last call.
async fn injected_rules(usage: Usage, usage_reported: bool) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("src/x.rs");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "fn x() {}\n").unwrap();
    // ~30 KB of rules: it fits a mostly free window but not what the turn leaves.
    let rules = format!("{HEAD_MARKER}\n{}\n{TAIL_MARKER}", "r".repeat(30_000));
    std::fs::write(tmp.path().join("AGENTS.md"), rules).unwrap();
    let model = Arc::new(ScriptedModel::new([
        Ok(big_tool_turn(&target, usage, usage_reported)),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let bus = Arc::new(EventBus::new(64));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model.clone())
        .with_sequential_run_ids()
        .with_project_rules(Arc::new(RulesSource::new(
            ProjectTrust::Approved,
            RulesSettings {
                context_window_tokens: WINDOW_TOKENS,
                response_headroom_tokens: 0,
                max_injection_bytes: 65_536,
            },
            None,
            Some(tmp.path().to_path_buf()),
            None,
        )));
    let run = runtime.delegate_background(Role::Worker, "read it".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));

    let observed = model.observed().await;
    let last_call = observed.last().expect("the follow-up call is observed");
    let texts: Vec<_> = last_call
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text }
                if text.starts_with("[project-rules]") || text.contains("[rules omitted") =>
            {
                Some(text.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(texts.len(), 1, "exactly one rules message: {texts:?}");
    texts.into_iter().next().unwrap()
}

// Given: a tool-use turn that fills most of the window and a provider that reports its usage
// When: the read succeeds / Then: the rules fit the reported remainder and arrive whole
#[tokio::test]
async fn reported_small_usage_leaves_room_for_the_whole_rule() {
    let usage = Usage {
        input_tokens: 500,
        output_tokens: 100,
        ..Usage::default()
    };
    let rules = injected_rules(usage, true).await;
    assert!(rules.contains(HEAD_MARKER) && rules.contains(TAIL_MARKER));
}

// Given: the same turn, but the provider reported no usage (placeholder zero)
// When: the read succeeds / Then: the budget comes from the request/response estimate, so the
// rule is cut or omitted instead of being injected as if the window were empty
#[tokio::test]
async fn unreported_usage_does_not_make_the_window_look_free() {
    let rules = injected_rules(Usage::default(), false).await;
    assert!(!rules.contains(TAIL_MARKER), "{} bytes", rules.len());
    assert!(
        rules.contains("[rules omitted") || rules.contains("[rules truncated"),
        "{rules:.200}"
    );
}

// Given: the same turn, but the provider explicitly reported zero tokens
// When: the read succeeds / Then: a measured zero is honored, unlike a missing report
#[tokio::test]
async fn reported_zero_usage_is_still_a_measurement() {
    let rules = injected_rules(Usage::default(), true).await;
    assert!(rules.contains(TAIL_MARKER));
}
