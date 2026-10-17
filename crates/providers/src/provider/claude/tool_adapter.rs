use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::message::{ContentBlock, Message};
use crate::sse::SseFrame;
use crate::wire::anthropic::{WireContentBlock, WireMessagesRequest};
use crate::{ProviderError, StreamEvent};

#[cfg(test)]
mod tests;

/// Request-scoped names include historical calls, so removing a tool does not rename replay.
#[derive(Default)]
pub(crate) struct ClaudeToolAdapter {
    canonical_names: BTreeMap<String, String>,
    initial_inputs: BTreeMap<usize, Value>,
}

impl ClaudeToolAdapter {
    pub(crate) fn shape_request(wire: &mut WireMessagesRequest) -> Result<Self, ProviderError> {
        let mut adapter = Self::default();
        for tool in &wire.tools {
            adapter.register(&tool.name)?;
        }
        for block in wire.messages.iter().flat_map(|message| &message.content) {
            match block {
                WireContentBlock::ToolUse { name, .. } => adapter.register(name)?,
                WireContentBlock::Raw { block, .. } if block["type"] == "tool_use" => {
                    if let Some(name) = block["name"].as_str() {
                        adapter.register(name)?;
                    }
                }
                _ => {}
            }
        }
        // Keep schemas, descriptions, canonical order and cache breakpoints unchanged.
        for tool in &mut wire.tools {
            tool.name = wire_name(&tool.name);
        }
        for block in wire
            .messages
            .iter_mut()
            .flat_map(|message| &mut message.content)
        {
            match block {
                WireContentBlock::ToolUse { name, .. } => *name = wire_name(name),
                WireContentBlock::Raw { block, .. } => {
                    adapt_raw_tool(block, |name| Some(wire_name(name)));
                }
                _ => {}
            }
        }
        Ok(adapter)
    }

    fn register(&mut self, name: &str) -> Result<(), ProviderError> {
        let wire = wire_name(name);
        if let Some(previous) = self.canonical_names.insert(wire, name.into())
            && previous != name
        {
            return Err(ProviderError::Request("Claude tool name collision".into()));
        }
        Ok(())
    }

    pub(crate) fn restore_message(&self, message: &mut Message) {
        for block in &mut message.content {
            match block {
                ContentBlock::ToolUse { name, .. } => {
                    if let Some(canonical) = self.canonical_names.get(name) {
                        name.clone_from(canonical);
                    }
                }
                ContentBlock::Anthropic { block, .. } => {
                    adapt_raw_tool(block, |name| self.canonical_names.get(name).cloned());
                }
                _ => {}
            }
        }
    }

    /// Restore names immediately and pass argument fragments through byte-for-byte.
    pub(crate) fn restore_events(
        &mut self,
        frame: &SseFrame,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let value: Value =
            serde_json::from_str(&frame.data).map_err(|_| ProviderError::InvalidSse {
                detail: "invalid Claude stream frame".into(),
            })?;
        let event_type = frame.event.as_deref().or_else(|| value["type"].as_str());
        for event in events.iter_mut() {
            if let StreamEvent::ToolCallDelta {
                index,
                name,
                arguments_delta,
                ..
            } = event
            {
                if let Some(name) = name {
                    self.initial_inputs.remove(index);
                    if event_type == Some("content_block_start")
                        && let Some(input) = value["content_block"].get("input")
                        && input.as_object().is_none_or(|object| !object.is_empty())
                    {
                        self.initial_inputs.insert(*index, input.clone());
                    }
                    if let Some(canonical) = self.canonical_names.get(name) {
                        name.clone_from(canonical);
                    }
                } else if !arguments_delta.trim().is_empty() {
                    self.initial_inputs.remove(index);
                }
            }
        }
        // A later input_json_delta replaces the initial input in the interpreter. Since the
        // accumulator only appends, publish the fallback only after that possibility ends.
        match event_type {
            Some("content_block_stop") => {
                if let Some(index) = value["index"]
                    .as_u64()
                    .and_then(|i| usize::try_from(i).ok())
                    && let Some(input) = self.initial_inputs.remove(&index)
                {
                    events.push(initial_input_event(index, input));
                }
            }
            Some("message_stop") => {
                events.extend(
                    std::mem::take(&mut self.initial_inputs)
                        .into_iter()
                        .map(|(index, input)| initial_input_event(index, input)),
                );
            }
            _ => {}
        }
        Ok(())
    }
}

fn initial_input_event(index: usize, input: Value) -> StreamEvent {
    StreamEvent::ToolCallDelta {
        index,
        id: None,
        name: None,
        arguments_delta: input.to_string(),
    }
}

fn wire_name(name: &str) -> String {
    if matches!(name, "read" | "write" | "edit" | "grep" | "shell") {
        return format!("_{name}");
    }
    let wire = format!("_mcp__evorch__{name}");
    if wire.len() <= 128
        && !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        wire
    } else {
        // Literal custom names always use the other namespace, including hash-like names.
        format!("_mcp__evorch_h__{:x}", Sha256::digest(name.as_bytes()))
    }
}

fn adapt_raw_tool(block: &mut Value, rename: impl FnOnce(&str) -> Option<String>) {
    if block["type"] != "tool_use" {
        return;
    }
    let Some(name) = block["name"].as_str() else {
        return;
    };
    if let Some(renamed) = rename(name) {
        block["name"] = renamed.into();
    }
}
