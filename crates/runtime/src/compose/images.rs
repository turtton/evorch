use providers::{ContentBlock, Message, ToolResultContent, ToolSpec};

/// History for the resolved model. A model without image input gets
/// tool-result images as notes naming the saved file, and, when it can
/// delegate, how to have `multimodal_looker` inspect it.
pub(super) fn for_model(
    messages: &[Message],
    tools: &[ToolSpec],
    accepts_images: bool,
) -> Vec<Message> {
    if accepts_images {
        return messages.to_vec();
    }
    let can_delegate = tools.iter().any(|tool| tool.name == "delegate");
    messages
        .iter()
        .map(|message| Message {
            role: message.role,
            content: message
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                    } => ContentBlock::ToolResult {
                        tool_call_id: tool_call_id.clone(),
                        content: content
                            .iter()
                            .map(|part| match part {
                                ToolResultContent::Image { source, .. } => {
                                    ToolResultContent::Text {
                                        text: omitted_note(source.as_deref(), can_delegate),
                                    }
                                }
                                ToolResultContent::Text { .. } => part.clone(),
                            })
                            .collect(),
                        is_error: *is_error,
                    },
                    _ => block.clone(),
                })
                .collect(),
        })
        .collect()
}

pub(super) fn has_tool_result_images(messages: &[Message]) -> bool {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|block| {
            matches!(block, ContentBlock::ToolResult { content, .. }
            if content.iter().any(|part| matches!(part, ToolResultContent::Image { .. })))
        })
}

fn omitted_note(source: Option<&str>, can_delegate: bool) -> String {
    match (source, can_delegate) {
        (Some(path), true) => format!(
            "[Image omitted: the current model cannot view images. It is saved at {path}. To inspect it, delegate to multimodal_looker and ask it to read that path.]"
        ),
        (Some(path), false) => {
            format!("[Image omitted: the current model cannot view images. It is saved at {path}.]")
        }
        (None, _) => "[Image omitted: the current model cannot view images.]".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use providers::Role;

    use super::*;

    fn history() -> Vec<Message> {
        vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: "call".into(),
                content: vec![
                    ToolResultContent::Text {
                        text: "Image /w/shot.png".into(),
                    },
                    ToolResultContent::Image {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                        source: Some("/w/shot.png".into()),
                    },
                ],
                is_error: false,
            }],
        }]
    }

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
        }
    }

    fn texts(messages: &[Message]) -> Vec<String> {
        match &messages[0].content[0] {
            ContentBlock::ToolResult { content, .. } => content
                .iter()
                .map(|part| part.text_or_image_note().into_owned())
                .collect(),
            other => panic!("unexpected block {other:?}"),
        }
    }

    #[test]
    fn image_capable_models_receive_images_unchanged() {
        assert_eq!(for_model(&history(), &[], true), history());
    }

    #[test]
    fn non_vision_model_gets_the_path_and_a_looker_hint_only_when_it_can_delegate() {
        let delegating = texts(&for_model(&history(), &[tool("delegate")], false));
        assert_eq!(delegating[0], "Image /w/shot.png");
        assert!(delegating[1].contains("/w/shot.png"));
        assert!(delegating[1].contains("multimodal_looker"));

        let leaf = texts(&for_model(&history(), &[tool("read")], false));
        assert!(leaf[1].contains("/w/shot.png"));
        assert!(!leaf[1].contains("multimodal_looker"));
    }
}
