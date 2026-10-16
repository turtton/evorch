use serde::Serialize;
use serde_json::Value;

use super::{ClaudeReasoningControls, fingerprint, profile::CLAUDE_CODE_VERSION};
use crate::ProviderError;
use crate::wire::anthropic::{
    WireContentBlock, WireMessage, WireMessagesRequest, WireRole, WireTool,
};

/// The billing block is a transport envelope, separate from signed model history.
pub(crate) fn serialize_request(
    wire: &WireMessagesRequest,
    metadata: Option<&Value>,
    reasoning: &ClaudeReasoningControls,
) -> Result<Vec<u8>, ProviderError> {
    let first_user_text = wire
        .messages
        .iter()
        .find(|m| m.role == WireRole::User)
        .into_iter()
        .flat_map(|m| &m.content)
        .find_map(|block| match block {
            WireContentBlock::Text { text, .. } => Some(text.as_str()),
            WireContentBlock::Raw { block, .. } if block["type"] == "text" => {
                block["text"].as_str()
            }
            _ => None,
        })
        .unwrap_or("");
    let mut system = wire.system.clone().unwrap_or_default();
    system.insert(
        0,
        WireContentBlock::Text {
            text: fingerprint::billing_header(CLAUDE_CODE_VERSION, first_user_text),
            cache_control: None,
        },
    );
    #[derive(Serialize)]
    struct Envelope<'a> {
        model: &'a str,
        messages: &'a [WireMessage],
        system: &'a [WireContentBlock],
        #[serde(skip_serializing_if = "<[WireTool]>::is_empty")]
        tools: &'a [WireTool],
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<&'a Value>,
        max_tokens: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        temperature: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking: Option<&'a Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        context_management: Option<&'a Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        output_config: Option<&'a Value>,
        stream: bool,
    }
    let mut bytes = serde_json::to_vec(&Envelope {
        model: &wire.model,
        messages: &wire.messages,
        system: &system,
        tools: &wire.tools,
        metadata,
        max_tokens: wire.max_tokens,
        temperature: wire.temperature,
        thinking: reasoning.thinking.as_ref(),
        context_management: reasoning.context_management.as_ref(),
        output_config: reasoning.output_config.as_ref(),
        stream: wire.stream,
    })
    .map_err(|_| ProviderError::Request("Claude request serialization failed".into()))?;
    fingerprint::finalize_billing_checksum(&mut bytes)?;
    Ok(bytes)
}
