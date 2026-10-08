//! MCP argument correlation across streamed envelopes and the exec channel.
//! Cursor's envelope call_id and McpArgs.tool_call_id are distinct namespaces.
use super::{protocol_error, wire};
use crate::ProviderError;
use serde_json::{Map, Value};
use std::collections::HashMap;

#[derive(Default)]
struct Arguments {
    started: Map<String, Value>,
    partial: String,
    exec: Map<String, Value>,
    completed: Map<String, Value>,
}

#[derive(Default)]
pub(super) struct McpArguments {
    envelopes: HashMap<String, String>,
    calls: HashMap<String, Arguments>,
    pending: HashMap<String, String>,
}

impl McpArguments {
    /// Observe toolCallStarted, partialToolCall, and toolCallCompleted without
    /// treating a server-side announcement as authorization for local execution.
    pub fn observe(&mut self, kind: u32, update: &[u8]) -> Result<(), ProviderError> {
        let envelope = wire::string(update, 1)?;
        let tool = wire::nested(update, 2)?;
        let args = tool
            .map(|tool| wire::nested(tool, 15))
            .transpose()?
            .flatten()
            .map(|mcp| wire::nested(mcp, 1))
            .transpose()?
            .flatten();
        let mut id = args
            .map(|args| wire::string(args, 3))
            .transpose()?
            .unwrap_or_default();
        if id.is_empty() && args.is_some() {
            id = tool
                .map(|tool| wire::string(tool, 57))
                .transpose()?
                .unwrap_or_default();
        }
        if !id.is_empty() && !envelope.is_empty() {
            if self
                .envelopes
                .get(&envelope)
                .is_some_and(|previous| previous != &id)
            {
                return Err(protocol_error(
                    "Cursor changed a streamed tool call identity",
                ));
            }
            self.envelopes.insert(envelope.clone(), id.clone());
        }
        if id.is_empty() {
            id = self.envelopes.get(&envelope).cloned().unwrap_or_default();
        }
        let delta = if kind == 7 {
            wire::string(update, 3)?
        } else {
            String::new()
        };
        if id.is_empty() {
            if !envelope.is_empty() && !delta.is_empty() {
                append_snapshot(self.pending.entry(envelope).or_default(), &delta)?;
            }
            return Ok(());
        }
        let call = self.calls.entry(id).or_default();
        if let Some(pending) = self.pending.remove(&envelope) {
            append_snapshot(&mut call.partial, &pending)?;
        }
        if !delta.is_empty() {
            append_snapshot(&mut call.partial, &delta)?;
        }
        if let Some(args) = args {
            let decoded = decode_args(args)?;
            if kind == 3 {
                merge(&mut call.completed, decoded);
            } else {
                merge(&mut call.started, decoded);
            }
        }
        Ok(())
    }

    pub fn record_exec(&mut self, id: &str, args: &[u8]) -> Result<(), ProviderError> {
        merge(
            &mut self.calls.entry(id.into()).or_default().exec,
            decode_args(args)?,
        );
        Ok(())
    }

    pub fn finish(&self, id: &str) -> Result<Value, ProviderError> {
        let Some(call) = self.calls.get(id) else {
            return Ok(Value::Object(Map::new()));
        };
        let mut input = call.started.clone();
        if !call.partial.is_empty() {
            let partial: Value = serde_json::from_str(&call.partial).map_err(|_| {
                protocol_error("Incomplete or invalid Cursor streamed MCP arguments")
            })?;
            let Value::Object(partial) = partial else {
                return Err(protocol_error("Cursor MCP arguments must be an object"));
            };
            merge(&mut input, partial);
        }
        // A later completion can add keys omitted by exec, and is authoritative
        // for its scalars. All layers retain structured data over string fallbacks.
        merge(&mut input, call.exec.clone());
        merge(&mut input, call.completed.clone());
        Ok(Value::Object(input))
    }
}

fn append_snapshot(buffer: &mut String, snapshot: &str) -> Result<(), ProviderError> {
    // args_text_delta is normally cumulative, but some builds send fragments.
    let suffix = snapshot.strip_prefix(buffer.as_str()).unwrap_or(snapshot);
    if buffer.len().saturating_add(suffix.len()) > wire::MAX_FRAME {
        return Err(protocol_error("Cursor tool arguments exceed size limit"));
    }
    buffer.push_str(suffix);
    Ok(())
}

fn decode_args(args: &[u8]) -> Result<Map<String, Value>, ProviderError> {
    wire::fields(args)?
        .into_iter()
        .filter(|field| field.number == 2 && field.wire == 2)
        .map(|field| {
            Ok((
                wire::string(field.data, 1)?,
                decode_argument(wire::nested(field.data, 2)?.unwrap_or_default())?,
            ))
        })
        .collect()
}

fn decode_argument(bytes: &[u8]) -> Result<Value, ProviderError> {
    match wire::decode_value(bytes) {
        Ok(Value::String(text)) if text.trim_start().starts_with(['{', '[', '"']) => {
            Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
        }
        Ok(value) => Ok(value),
        Err(_) => {
            let text = std::str::from_utf8(bytes)
                .map_err(|_| protocol_error("Invalid Cursor MCP argument encoding"))?;
            Ok(serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.into())))
        }
    }
}

fn merge(target: &mut Map<String, Value>, source: Map<String, Value>) {
    for (key, value) in source {
        if value.is_string()
            && target
                .get(&key)
                .is_some_and(|value| value.is_object() || value.is_array())
        {
            continue;
        }
        target.insert(key, value);
    }
}
