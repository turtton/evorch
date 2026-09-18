use crate::message::ContentBlock;

#[derive(Clone, Copy)]
pub(super) enum ReasoningReplay {
    Preserve,
    Omit,
}

impl ReasoningReplay {
    pub(super) fn for_model(model: &str) -> Self {
        let name = model.rsplit('/').next().unwrap_or_default();
        if ["kimi", "moonshot"].iter().any(|prefix| {
            name.get(..prefix.len())
                .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        }) {
            Self::Preserve
        } else {
            Self::Omit
        }
    }

    pub(super) fn content(self, blocks: &[ContentBlock]) -> Option<String> {
        match self {
            Self::Preserve => blocks.iter().fold(None, |output, block| match block {
                ContentBlock::Reasoning { text } => {
                    let mut output = output.unwrap_or_else(String::new);
                    output.push_str(text);
                    Some(output)
                }
                ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. } => output,
            }),
            Self::Omit => None,
        }
    }
}
