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
    pending_inputs: BTreeMap<usize, StreamInput>,
}

struct StreamInput {
    initial: Value,
    partial_json: String,
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
        // Keep the canonical tool order and its cache breakpoint unchanged.
        for tool in &mut wire.tools {
            if uses_file_path(&tool.name) {
                swap_path_schema(&mut tool.input_schema);
            }
            tool.name = wire_name(&tool.name);
        }
        for block in wire
            .messages
            .iter_mut()
            .flat_map(|message| &mut message.content)
        {
            match block {
                WireContentBlock::ToolUse { name, input, .. } => {
                    if uses_file_path(name) {
                        swap_path_keys(input);
                    }
                    *name = wire_name(name);
                }
                WireContentBlock::Raw { block, .. } => {
                    adapt_raw_tool(block, |name| Some(wire_name(name)), uses_file_path);
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
                ContentBlock::ToolUse { name, input, .. } => {
                    if let Some(canonical) = self.canonical_names.get(name) {
                        if uses_file_path(canonical) {
                            swap_path_keys(input);
                        }
                        name.clone_from(canonical);
                    }
                }
                ContentBlock::Anthropic { block, .. } => {
                    adapt_raw_tool(
                        block,
                        |name| self.canonical_names.get(name).cloned(),
                        |name| {
                            self.canonical_names
                                .get(name)
                                .is_some_and(|n| uses_file_path(n))
                        },
                    );
                }
                _ => {}
            }
        }
    }

    /// Only complete JSON can safely rename keys; expose names immediately and buffer arguments.
    pub(crate) fn restore_events(
        &mut self,
        frame: &SseFrame,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let value: Value =
            serde_json::from_str(&frame.data).map_err(|_| ProviderError::InvalidSse {
                detail: "invalid Claude stream frame".into(),
            })?;
        for event in events.iter_mut() {
            if let StreamEvent::ToolCallDelta {
                index,
                name,
                arguments_delta,
                ..
            } = event
            {
                if let Some(name) = name {
                    self.pending_inputs.remove(index);
                    if let Some(canonical) = self.canonical_names.get(name) {
                        if uses_file_path(canonical) {
                            self.pending_inputs.insert(
                                *index,
                                StreamInput {
                                    initial: value["content_block"]["input"].clone(),
                                    partial_json: String::new(),
                                },
                            );
                        }
                        name.clone_from(canonical);
                    }
                } else if let Some(input) = self.pending_inputs.get_mut(index) {
                    input.partial_json.push_str(arguments_delta);
                    arguments_delta.clear();
                }
            }
        }
        events.retain(|event| {
            !matches!(event, StreamEvent::ToolCallDelta {
            id: None, name: None, arguments_delta, ..
        } if arguments_delta.is_empty())
        });
        match frame.event.as_deref().or_else(|| value["type"].as_str()) {
            Some("content_block_stop") => {
                if let Some(index) = value["index"]
                    .as_u64()
                    .and_then(|i| usize::try_from(i).ok())
                    && let Some(input) = self.pending_inputs.remove(&index)
                {
                    events.push(input.finish(index));
                }
            }
            Some("message_stop") => {
                events.extend(
                    std::mem::take(&mut self.pending_inputs)
                        .into_iter()
                        .map(|(index, input)| input.finish(index)),
                );
            }
            _ => {}
        }
        Ok(())
    }
}

impl StreamInput {
    fn finish(self, index: usize) -> StreamEvent {
        let input = if self.partial_json.trim().is_empty() {
            Ok(self.initial)
        } else {
            serde_json::from_str(&self.partial_json)
        };
        let arguments_delta = match input {
            Ok(mut input) => {
                swap_path_keys(&mut input);
                input.to_string()
            }
            // Keep the original bytes for StreamAccumulator's malformed-call diagnostics.
            Err(_) => self.partial_json,
        };
        StreamEvent::ToolCallDelta {
            index,
            id: None,
            name: None,
            arguments_delta,
        }
    }
}

fn uses_file_path(name: &str) -> bool {
    matches!(name, "read" | "write")
}

fn wire_name(name: &str) -> String {
    match name {
        "read" => "Read".into(),
        "write" => "Write".into(),
        _ => {
            let wire = format!("mcp__evorch__{name}");
            if wire.len() <= 128
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
                && !name.is_empty()
            {
                wire
            } else {
                // A disjoint namespace prevents literal names from colliding with encoded names.
                format!("mcp__evorch_h__{:x}", Sha256::digest(name.as_bytes()))
            }
        }
    }
}

fn adapt_raw_tool(
    block: &mut Value,
    rename: impl FnOnce(&str) -> Option<String>,
    swap: impl FnOnce(&str) -> bool,
) {
    if block["type"] != "tool_use" {
        return;
    }
    let Some(name) = block["name"].as_str() else {
        return;
    };
    let Some(renamed) = rename(name) else { return };
    if swap(name)
        && let Some(input) = block.get_mut("input")
    {
        swap_path_keys(input);
    }
    block["name"] = renamed.into();
}

/// Swapping both keys preserves existing aliases and conflicting values across replay.
fn swap_path_keys(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let path = object.remove("path");
    let file_path = object.remove("file_path");
    if let Some(path) = path {
        object.insert("file_path".into(), path);
    }
    if let Some(file_path) = file_path {
        object.insert("path".into(), file_path);
    }
}

fn swap_path_schema(schema: &mut Value) {
    if let Some(properties) = schema.get_mut("properties") {
        swap_path_keys(properties);
    }
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        for name in required {
            match name.as_str() {
                Some("path") => *name = "file_path".into(),
                Some("file_path") => *name = "path".into(),
                _ => {}
            }
        }
    }
}
