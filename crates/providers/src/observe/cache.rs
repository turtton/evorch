use crate::message::{ChatRequest, ContentBlock, ToolResultContent};

/// Approximate the reusable prefix, excluding the newest conversation message.
/// This is a byte heuristic, not a tokenizer or a prediction of cache residency.
pub(super) fn expected_cacheable_tokens(request: &ChatRequest) -> u64 {
    let newest = request
        .messages
        .iter()
        .rposition(|message| message.role != crate::message::Role::System);
    let message_bytes = request
        .messages
        .iter()
        .enumerate()
        .filter(|(index, _)| Some(*index) != newest)
        .flat_map(|(_, message)| &message.content)
        .map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.len(),
            ContentBlock::ToolUse { id, name, input } => {
                id.len() + name.len() + input.to_string().len()
            }
            ContentBlock::ToolResult { content, .. } => content
                .iter()
                .map(|part| match part {
                    ToolResultContent::Text { text } => text.len(),
                })
                .sum(),
            ContentBlock::Image { .. } => 0,
        })
        .sum::<usize>();
    let tool_bytes = request
        .tools
        .iter()
        .map(|tool| tool.name.len() + tool.description.len() + tool.input_schema.to_string().len())
        .sum::<usize>();
    u64::try_from(message_bytes.saturating_add(tool_bytes).div_ceil(4)).unwrap_or(u64::MAX)
}
