use super::super::types::{WireContent, WireInputPart};
use crate::error::ProviderError;
use crate::message::{ContentBlock, ToolResultContent};

pub(super) fn content_blocks(content: &WireContent) -> Result<Vec<ContentBlock>, ProviderError> {
    if let WireContent::Multimodal(parts) = content {
        return parts
            .iter()
            .map(|part| match part {
                WireInputPart::Text { text } => Ok(ContentBlock::Text { text: text.clone() }),
                WireInputPart::ImageUrl { image_url } => {
                    let (media_type, data) = image_url
                        .url
                        .strip_prefix("data:")
                        .and_then(|url| url.split_once(";base64,"))
                        .ok_or_else(|| ProviderError::InvalidJson {
                            detail: "image input requires a base64 data URL".into(),
                        })?;
                    Ok(ContentBlock::Image {
                        media_type: media_type.into(),
                        data: data.into(),
                    })
                }
            })
            .collect();
    }
    Ok(wire_texts(content)?
        .into_iter()
        .map(|text| ContentBlock::Text { text })
        .collect())
}

pub(super) fn tool_result_content(
    content: &WireContent,
) -> Result<Vec<ToolResultContent>, ProviderError> {
    Ok(wire_texts(content)?
        .into_iter()
        .map(|text| ToolResultContent::Text { text })
        .collect())
}

fn wire_texts(content: &WireContent) -> Result<Vec<String>, ProviderError> {
    match content {
        WireContent::Multimodal(_) => Err(ProviderError::InvalidJson {
            detail: "image content cannot be decoded as text".into(),
        }),
        WireContent::Text(text) => Ok(vec![text.clone()]),
        WireContent::Parts(parts) => parts
            .iter()
            .map(|part| {
                if part.kind == "text" {
                    Ok(part.text.clone())
                } else {
                    Err(ProviderError::InvalidJson {
                        detail: format!("未対応の content part type です: {}", part.kind),
                    })
                }
            })
            .collect(),
    }
}
