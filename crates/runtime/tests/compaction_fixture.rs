mod support;

use std::collections::HashSet;

use providers::{ContentBlock, Message, Role, ToolResultContent};
use support::load_compaction_fixture;

fn joined_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_use_ids(message: &Message) -> Vec<&str> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolResult { .. } => None,
        })
        .collect()
}

fn tool_result_ids(message: &Message) -> Vec<&str> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. } => None,
        })
        .collect()
}

fn tool_result_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|item| match item {
                        ToolResultContent::Text { text } => Some(text.as_str()),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            ContentBlock::Text { .. }
            | ContentBlock::Reasoning { .. }
            | ContentBlock::ToolUse { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Given: the compaction long-session fixture on disk
// When: it is loaded through the support loader
// Then: it parses as Vec<providers::Message> and every ingredient class
// required by AC3/AC9 (issue #63) is present: system prompt, goal text,
// decision statements, unresolved items, verification output,
// AGENT_MESSAGE-prefixed user texts (send + reply kinds), >= 8 closed
// tool pairs, and an open tool pair at the very tail.
#[test]
fn compaction_long_session_fixture_contains_required_ingredients() {
    let messages = load_compaction_fixture();

    assert!(
        (30..=60).contains(&messages.len()),
        "expected 30-60 messages for old-vs-recent token separation, got {}",
        messages.len()
    );

    assert!(
        matches!(messages[0].role, Role::System),
        "message 0 must be the system prompt"
    );
    assert!(
        joined_text(&messages[0]).contains("orchestrator"),
        "system prompt must be orchestrator-style"
    );

    assert!(
        matches!(messages[1].role, Role::User),
        "message 1 must be the initial user prompt"
    );
    assert!(
        joined_text(&messages[1]).contains("Fix the flaky compaction retry in scheduler"),
        "initial user prompt must state the session goal"
    );

    let assistant_text = messages
        .iter()
        .filter(|message| matches!(message.role, Role::Assistant))
        .map(joined_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        assistant_text.contains("Decision: use exponential backoff with jitter"),
        "assistant text must contain an explicit decision statement"
    );
    assert!(
        assistant_text
            .contains("Still unresolved: why the second retry occasionally skips the backoff"),
        "assistant text must contain the unresolved item"
    );

    let all_use_ids = messages.iter().flat_map(tool_use_ids).collect::<Vec<_>>();
    let use_id_set = all_use_ids.iter().copied().collect::<HashSet<_>>();
    let all_result_ids = messages
        .iter()
        .flat_map(tool_result_ids)
        .collect::<HashSet<_>>();
    assert!(
        all_use_ids.len() >= 8 && all_result_ids.len() >= 8,
        "expected >= 8 tool uses and results, got {} uses / {} results",
        all_use_ids.len(),
        all_result_ids.len()
    );

    let tool_output = messages
        .iter()
        .map(tool_result_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        tool_output.contains("test result: ok. 12 passed; 0 failed"),
        "tool results must contain the verification output"
    );
    assert!(
        tool_output.contains("test result: FAILED."),
        "tool results must contain a failing verification run"
    );

    let agent_texts = messages
        .iter()
        .filter(|message| matches!(message.role, Role::User))
        .filter_map(|message| match message.content.as_slice() {
            [ContentBlock::Text { text }] if text.starts_with("[agent-message id=") => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        agent_texts.len() >= 2,
        "expected >= 2 AGENT_MESSAGE-prefixed user texts, got {}",
        agent_texts.len()
    );
    for text in &agent_texts {
        let (header, body) = text
            .split_once('\n')
            .unwrap_or_else(|| panic!("agent message must have a body: {text}"));
        assert!(
            header.starts_with("[agent-message id=")
                && header.contains(" from=")
                && header.contains(" kind=")
                && header.ends_with(']'),
            "agent message header must match the runtime format: {header}"
        );
        assert!(
            !body.trim().is_empty(),
            "agent message body must be non-empty"
        );
    }
    assert!(
        agent_texts.iter().any(|text| text.contains("kind=send")),
        "expected at least one kind=send agent message"
    );
    assert!(
        agent_texts.iter().any(|text| text.contains("kind=reply")),
        "expected at least one kind=reply agent message"
    );

    let last = messages.last().expect("non-empty fixture");
    let tail_use_ids = tool_use_ids(last);
    assert!(
        !tail_use_ids.is_empty(),
        "the very last message must carry a ToolUse (open pair)"
    );
    for id in &tail_use_ids {
        assert!(
            !all_result_ids.contains(*id),
            "tail ToolUse {id} must stay open: no ToolResult may exist yet"
        );
    }
    let tail_result_ids = messages[messages.len() - 3..]
        .iter()
        .flat_map(tool_result_ids)
        .collect::<Vec<_>>();
    assert!(
        !tail_result_ids.is_empty(),
        "a ToolResult must sit within the final 3 messages (closed pair near the tail)"
    );
    for id in &tail_result_ids {
        assert!(
            use_id_set.contains(*id),
            "tail ToolResult {id} must pair with an earlier ToolUse"
        );
    }
}
