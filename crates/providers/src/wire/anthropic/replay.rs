use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::message::{ContentBlock, Message};

use super::types::{WireContentBlock, WireMessage, WireMessagesRequest};

/// Bind opaque reasoning to the actual Anthropic prefix, after OAuth shaping.
/// Cache breakpoints move as history grows and are not part of that prefix.
pub(crate) fn prefix_hash(request: &WireMessagesRequest, message_count: usize) -> String {
    let mut digest = prefix_digest(request);
    for message in &request.messages[..message_count] {
        hash_message(&mut digest, message);
    }
    format!("{:x}", digest.finalize())
}

fn prefix_digest(request: &WireMessagesRequest) -> Sha256 {
    let mut prefix = json!({"system":request.system,"tools":request.tools});
    for key in ["system", "tools"] {
        if let Some(blocks) = prefix[key].as_array_mut() {
            for block in blocks {
                remove_cache_control(block);
            }
        }
    }
    let mut digest = Sha256::new();
    hash_value(&mut digest, &prefix);
    digest
}

fn hash_message(digest: &mut Sha256, message: &WireMessage) {
    let mut value = json!(message);
    if let Some(blocks) = value["content"].as_array_mut() {
        for block in blocks {
            remove_cache_control(block);
        }
    }
    hash_value(digest, &value);
}

fn hash_value(digest: &mut Sha256, value: &Value) {
    let bytes = value.to_string();
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes.as_bytes());
}

fn remove_cache_control(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.remove("cache_control");
    }
}

pub(crate) fn bind_response(message: &mut Message, hash: &str) {
    for block in &mut message.content {
        if let ContentBlock::Anthropic { block, prefix_hash } = block
            && is_signed(block)
        {
            *prefix_hash = Some(hash.into());
        }
    }
}

pub(crate) fn discard_unbound_thinking(request: &mut WireMessagesRequest) {
    let mut digest = prefix_digest(request);
    for message in &mut request.messages {
        let hash = format!("{:x}", digest.clone().finalize());
        message.content.retain(|block| match block {
            WireContentBlock::Thinking { .. } => false,
            WireContentBlock::Raw { block, prefix_hash } if is_signed(block) => {
                prefix_hash.as_deref() == Some(hash.as_str())
            }
            _ => true,
        });
        if message.content.is_empty() {
            message.content.push(WireContentBlock::Text {
                text: "[Reasoning omitted from replay]".into(),
                cache_control: None,
            });
        }
        hash_message(&mut digest, message);
    }
}

fn is_signed(block: &Value) -> bool {
    matches!(
        block["type"].as_str(),
        Some("thinking" | "redacted_thinking")
    )
}
