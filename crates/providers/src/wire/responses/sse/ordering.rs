//! Responses-only reconstruction; the accumulator remains authoritative for tools.
use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::responses_replay::{ResponsesOutputPosition, ResponsesReplayOrigin, is_replayable_item};
use crate::{ContentBlock, Message, ProviderError};

#[derive(Clone, Default)]
pub(super) struct OutputOrdering {
    identities: BTreeMap<u64, (ResponsesOutputPosition, String)>,
    done: BTreeMap<u64, Value>,
    text: BTreeMap<u64, String>,
}

impl std::fmt::Debug for OutputOrdering {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputOrdering")
            .field("identities", &self.identities)
            .field("confirmed_items", &self.done.len())
            .field("text", &self.text)
            .finish_non_exhaustive()
    }
}

fn conflict() -> ProviderError {
    ProviderError::InvalidSse {
        detail: "Responses output identity or confirmed item conflict".into(),
    }
}

impl OutputOrdering {
    pub(super) fn item(&mut self, value: &Value, done: bool) -> Result<(), ProviderError> {
        let Some(index) = value["output_index"].as_u64() else {
            return Ok(());
        };
        let item = &value["item"];
        let Some(id) = item["id"].as_str().filter(|s| !s.is_empty()) else {
            return Ok(());
        };
        let Some(kind) = item["type"].as_str() else {
            return Ok(());
        };
        if self
            .identities
            .get(&index)
            .is_some_and(|(old, old_kind)| old.item_id != id || old_kind != kind)
            || self
                .identities
                .iter()
                .any(|(i, (old, _))| *i != index && old.item_id == id)
            || (self.text.contains_key(&index) && kind != "message")
        {
            return Err(conflict());
        }
        self.identities.insert(
            index,
            (
                ResponsesOutputPosition {
                    output_index: index,
                    item_id: id.to_owned(),
                },
                kind.to_owned(),
            ),
        );
        if done {
            if self.done.get(&index).is_some_and(|old| old != item) {
                return Err(conflict());
            }
            self.done.insert(index, item.clone());
        }
        Ok(())
    }

    pub(super) fn text(&mut self, value: &Value) -> Result<(), ProviderError> {
        if let (Some(index), Some(delta)) =
            (value["output_index"].as_u64(), value["delta"].as_str())
        {
            if self
                .identities
                .get(&index)
                .is_some_and(|(_, kind)| kind != "message")
            {
                return Err(conflict());
            }
            self.text.entry(index).or_default().push_str(delta);
        }
        Ok(())
    }

    pub(super) fn finalize(&self, message: &mut Message, origin: Option<&ResponsesReplayOrigin>) {
        let Some(origin) = origin else { return };
        if !self.done.values().any(is_replayable_item) {
            return;
        }
        let mut ordered = BTreeMap::<u64, Vec<ContentBlock>>::new();
        let mut used_tools = BTreeSet::new();
        // Done text is an ordering witness, never a replacement for streamed text.
        for (index, item) in &self.done {
            let blocks = ordered.entry(*index).or_default();
            match item["type"].as_str() {
                Some("reasoning") if is_replayable_item(item) => {
                    blocks.push(ContentBlock::ResponsesReasoning {
                        item: item.clone(),
                        origin: origin.clone(),
                    });
                }
                Some("message") => {
                    let Some(parts) = item["content"].as_array() else {
                        return;
                    };
                    for part in parts {
                        if part["type"] == "output_text" {
                            let Some(text) = part["text"].as_str() else {
                                return;
                            };
                            blocks.push(ContentBlock::Text {
                                text: text.to_owned(),
                            });
                        }
                    }
                    // A whole-message concatenation can match even when text has
                    // crossed a reasoning boundary. Require evidence at this index.
                    let done_text: String = blocks.iter().filter_map(ContentBlock::text).collect();
                    if self.text.get(index).map(String::as_str).unwrap_or_default() != done_text {
                        return;
                    }
                }
                Some("function_call" | "custom_tool_call") => {
                    let Some(id) = item["call_id"].as_str() else {
                        return;
                    };
                    if !used_tools.insert(id) {
                        return;
                    }
                    if let Some(block) = message.content.iter().find(
                        |b| matches!(b, ContentBlock::ToolUse { id: call_id, .. } if call_id == id),
                    ) {
                        blocks.push(block.clone());
                    }
                }
                _ => {}
            }
        }
        for (index, text) in &self.text {
            if !self.done.get(index).is_some_and(|v| v["type"] == "message") {
                ordered
                    .entry(*index)
                    .or_default()
                    .push(ContentBlock::Text { text: text.clone() });
            }
        }
        let assembled_text: String = ordered
            .values()
            .flatten()
            .filter_map(ContentBlock::text)
            .collect();
        let streamed_text: String = message
            .content
            .iter()
            .filter_map(ContentBlock::text)
            .collect();
        let assembled_tools = ordered
            .values()
            .flatten()
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .count();
        let streamed_tools = message
            .content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .count();
        // Missing indices/done events: keep legacy output, but never guess opaque positions.
        if assembled_text != streamed_text || assembled_tools != streamed_tools {
            return;
        }
        let summaries = message
            .content
            .iter()
            .filter(|b| matches!(b, ContentBlock::Reasoning { .. }))
            .cloned();
        message.content = summaries.chain(ordered.into_values().flatten()).collect();
    }
}
