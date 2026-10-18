use serde::Serialize;
use serde_json::Value;

use super::transport_context::{ClaudeDiagnostics, ClaudeRequestContext};
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
    context: Option<&ClaudeRequestContext>,
) -> Result<Vec<u8>, ProviderError> {
    serialize_request_with_tools(wire, metadata, reasoning, context, None)
}

pub(crate) fn serialize_request_with_tools(
    wire: &WireMessagesRequest,
    metadata: Option<&Value>,
    reasoning: &ClaudeReasoningControls,
    context: Option<&ClaudeRequestContext>,
    server_tools: Option<&[Value]>,
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
    let mut billing = fingerprint::billing_header(CLAUDE_CODE_VERSION, first_user_text);
    if let Some(context) = context {
        context.append_billing(&mut billing);
    }
    system.insert(
        0,
        WireContentBlock::Text {
            text: billing,
            cache_control: None,
        },
    );
    #[derive(Serialize)]
    #[serde(untagged)]
    enum Tools<'a> {
        Client(&'a [WireTool]),
        Server(&'a [Value]),
    }
    impl Tools<'_> {
        fn is_empty(&self) -> bool {
            match self {
                Self::Client(tools) => tools.is_empty(),
                Self::Server(tools) => tools.is_empty(),
            }
        }
    }
    #[derive(Serialize)]
    struct Envelope<'a> {
        model: &'a str,
        messages: &'a [WireMessage],
        system: &'a [WireContentBlock],
        #[serde(skip_serializing_if = "Tools::is_empty")]
        tools: Tools<'a>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        diagnostics: Option<ClaudeDiagnostics<'a>>,
        stream: bool,
    }
    let mut bytes = serde_json::to_vec(&Envelope {
        model: &wire.model,
        messages: &wire.messages,
        system: &system,
        tools: server_tools.map_or(Tools::Client(&wire.tools), Tools::Server),
        metadata,
        max_tokens: wire.max_tokens,
        temperature: wire.temperature,
        thinking: reasoning.thinking.as_ref(),
        context_management: reasoning.context_management.as_ref(),
        output_config: reasoning.output_config.as_ref(),
        diagnostics: context.map(ClaudeRequestContext::diagnostics),
        stream: wire.stream,
    })
    .map_err(|_| ProviderError::Request("Claude request serialization failed".into()))?;
    fingerprint::finalize_billing_checksum(&mut bytes)?;
    Ok(bytes)
}
