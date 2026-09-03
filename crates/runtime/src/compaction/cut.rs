use providers::{ContentBlock, Message, Role};

use super::estimator::estimate_tokens;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CutPlan {
    pub start: usize,
    pub end: usize,
}

pub(crate) fn select_cut(
    messages: &[Message],
    keep_recent_tokens: u64,
    protected_prefix: usize,
) -> Option<CutPlan> {
    let prefix = protected_prefix.min(messages.len());
    let mut cut = messages.len();
    let mut kept_tokens = 0_u64;
    while cut > prefix && kept_tokens < keep_recent_tokens {
        cut -= 1;
        kept_tokens =
            kept_tokens.saturating_add(estimate_tokens(std::slice::from_ref(&messages[cut])));
    }
    if cut == prefix {
        return None;
    }

    loop {
        let previous = cut;
        cut = preserve_tool_groups(messages, cut, prefix);
        cut = preserve_open_tail(messages, cut, prefix);
        if cut == previous {
            break;
        }
    }

    if cut == messages.len() || !is_kept_boundary(&messages[cut]) {
        cut = (prefix..cut)
            .rev()
            .find(|index| is_kept_boundary(&messages[*index]))?;
        loop {
            let adjusted = preserve_tool_groups(messages, cut, prefix);
            if adjusted == cut {
                break;
            }
            cut = adjusted;
        }
    }

    (cut > prefix).then_some(CutPlan {
        start: prefix,
        end: cut,
    })
}

fn preserve_tool_groups(messages: &[Message], cut: usize, prefix: usize) -> usize {
    let mut adjusted = cut;
    for (use_index, message) in messages.iter().enumerate() {
        for block in &message.content {
            let ContentBlock::ToolUse { id, .. } = block else {
                continue;
            };
            let mut last_index = use_index;
            for (result_index, result_message) in messages.iter().enumerate() {
                if result_message.content.iter().any(|result| {
                    matches!(result, ContentBlock::ToolResult { tool_call_id, .. } if tool_call_id == id)
                }) {
                    last_index = last_index.max(result_index);
                }
            }
            if use_index < adjusted && last_index >= adjusted {
                adjusted = use_index.max(prefix);
            }
        }
    }
    adjusted
}

fn preserve_open_tail(messages: &[Message], cut: usize, prefix: usize) -> usize {
    let final_index = messages.len().checked_sub(1);
    let mut adjusted = cut;
    for (use_index, message) in messages.iter().enumerate() {
        for block in &message.content {
            let ContentBlock::ToolUse { id, .. } = block else {
                continue;
            };
            let result_indices = messages.iter().enumerate().filter_map(|(index, candidate)| {
                candidate
                    .content
                    .iter()
                    .any(|result| {
                        matches!(result, ContentBlock::ToolResult { tool_call_id, .. } if tool_call_id == id)
                    })
                    .then_some(index)
            });
            let mut found_result = false;
            let mut result_is_final = false;
            for result_index in result_indices {
                found_result = true;
                result_is_final |= Some(result_index) == final_index;
            }
            if !found_result || result_is_final {
                adjusted = adjusted.min(use_index.max(prefix));
            }
        }
    }
    adjusted
}

fn is_kept_boundary(message: &Message) -> bool {
    match message.role {
        Role::System => false,
        Role::Assistant => true,
        Role::User => {
            let has_text = message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { .. }));
            let has_tool_result = message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
            has_text && !has_tool_result
        }
    }
}

#[cfg(test)]
mod tests {
    use providers::{ContentBlock, Message, Role, ToolResultContent};

    use super::select_cut;
    use crate::compaction::estimator::estimate_tokens;

    fn text(role: Role, value: &str) -> Message {
        Message {
            role,
            content: vec![ContentBlock::Text {
                text: value.to_string(),
            }],
        }
    }

    fn tool_use(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: "read".to_string(),
                input: serde_json::json!({ "path": "/tmp/input" }),
            }],
        }
    }

    fn tool_results(ids: &[&str]) -> Message {
        Message {
            role: Role::User,
            content: ids
                .iter()
                .map(|id| ContentBlock::ToolResult {
                    tool_call_id: (*id).to_string(),
                    content: vec![ToolResultContent::Text {
                        text: "result".to_string(),
                    }],
                    is_error: false,
                })
                .collect(),
        }
    }

    fn message_tokens(message: &Message) -> u64 {
        estimate_tokens(std::slice::from_ref(message))
    }

    // Given: 保護 prefix と古い会話と直近会話 / When: tail から直近予算を確保 / Then: prefix 後から直近会話前までを切る
    #[test]
    fn walks_backward_and_preserves_protected_prefix() {
        let messages = [
            text(Role::System, "system"),
            text(Role::User, "old question"),
            text(Role::Assistant, "old answer"),
            text(Role::User, "recent question"),
            text(Role::Assistant, "recent answer"),
        ];
        let keep = message_tokens(&messages[3]).saturating_add(message_tokens(&messages[4]));

        let plan = select_cut(&messages, keep, 1).expect("old conversation is compactable");

        assert_eq!((plan.start, plan.end), (1, 3));
    }

    // Given: 予算境界が ToolResult 直前に来る履歴 / When: cut point を選択 / Then: matching ToolUse まで kept region を広げる
    #[test]
    fn kept_tool_result_keeps_matching_tool_use() {
        let messages = [
            text(Role::System, "system"),
            text(Role::User, "old"),
            tool_use("call-1"),
            tool_results(&["call-1"]),
            text(Role::Assistant, "done"),
        ];
        let keep = message_tokens(&messages[3]).saturating_add(message_tokens(&messages[4]));

        let plan = select_cut(&messages, keep, 1).expect("old user message is compactable");

        assert_eq!((plan.start, plan.end), (1, 2));
    }

    // Given: 予算境界が ToolUse 後かつ結果前に来る履歴 / When: cut point を選択 / Then: ToolUse と全 ToolResult を kept region に置く
    #[test]
    fn kept_tool_use_keeps_all_tool_results() {
        let messages = [
            text(Role::System, "system"),
            text(Role::User, "old"),
            tool_use("call-1"),
            tool_results(&["call-1"]),
            text(Role::Assistant, "done"),
        ];
        let keep = message_tokens(&messages[3])
            .saturating_add(message_tokens(&messages[4]))
            .saturating_sub(1);

        let plan = select_cut(&messages, keep, 1).expect("old user message is compactable");

        assert_eq!((plan.start, plan.end), (1, 2));
    }

    // Given: ToolResult が予算境界の次メッセージ / When: cut point を選択 / Then: ToolResult 直前を避けて ToolUse の Assistant 境界を使う
    #[test]
    fn never_cuts_directly_before_tool_result_message() {
        let messages = [
            text(Role::System, "system"),
            text(Role::User, "old"),
            tool_use("call-1"),
            tool_results(&["call-1"]),
            text(Role::Assistant, "done"),
        ];
        let keep = message_tokens(&messages[3]).saturating_add(message_tokens(&messages[4]));

        let plan = select_cut(&messages, keep, 1).expect("a safe boundary exists");

        assert_eq!((plan.start, plan.end), (1, 2));
    }

    // Given: tail の ToolUse に結果がまだない履歴 / When: keep budget 0 で cut point を選択 / Then: open ToolUse は kept region に残す
    #[test]
    fn open_tool_use_at_tail_always_stays_kept() {
        let messages = [
            text(Role::System, "system"),
            text(Role::User, "old"),
            tool_use("call-open"),
        ];

        let plan = select_cut(&messages, 0, 1).expect("old user message is compactable");

        assert_eq!((plan.start, plan.end), (1, 2));
    }

    // Given: tail の ToolUse と final ToolResult / When: keep budget 0 で cut point を選択 / Then: open turn 全体を kept region に残す
    #[test]
    fn final_tool_result_pair_always_stays_kept() {
        let messages = [
            text(Role::System, "system"),
            text(Role::User, "old"),
            tool_use("call-final"),
            tool_results(&["call-final"]),
        ];

        let plan = select_cut(&messages, 0, 1).expect("old user message is compactable");

        assert_eq!((plan.start, plan.end), (1, 2));
    }

    // Given: prefix 後の全メッセージを覆う keep budget / When: cut point を選択 / Then: compactable range はない
    #[test]
    fn returns_none_when_budget_covers_all_unprotected_messages() {
        let messages = [
            text(Role::System, "system"),
            text(Role::User, "question"),
            text(Role::Assistant, "answer"),
        ];
        let keep = message_tokens(&messages[1]).saturating_add(message_tokens(&messages[2]));

        assert_eq!(select_cut(&messages, keep, 1), None);
    }

    // Given: 1 ToolUse に複数 ToolResult message / When: 境界が結果群に入る / Then: tool group 全体を kept region に置く
    #[test]
    fn multi_result_tool_call_stays_whole() {
        let messages = [
            text(Role::System, "system"),
            text(Role::User, "old"),
            tool_use("call-many"),
            tool_results(&["call-many"]),
            tool_results(&["call-many"]),
            text(Role::Assistant, "done"),
        ];
        let keep = message_tokens(&messages[4]).saturating_add(message_tokens(&messages[5]));

        let plan = select_cut(&messages, keep, 1).expect("old user message is compactable");

        assert_eq!((plan.start, plan.end), (1, 2));
    }
}
