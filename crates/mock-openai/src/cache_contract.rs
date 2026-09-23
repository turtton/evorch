//! Independent assertions for the prompt prefix captured at the HTTP boundary.
//!
//! These checks protect reusable request content; they do not predict a service's
//! cache hit rate. They deliberately do not use the provider's cache observer.
//! JSON object key order is not observable through `Value`; array order and all
//! string bytes (including serialized function arguments) are compared exactly.

use serde_json::{Map, Value};

/// Wire format of the captured request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheProtocol {
    OpenAi,
    Codex,
    Anthropic,
}

impl CacheProtocol {
    fn history_key(self) -> &'static str {
        match self {
            Self::OpenAi | Self::Anthropic => "messages",
            Self::Codex => "input",
        }
    }
}

/// Require unchanged input settings and the entire previous conversation prefix.
///
/// Only root `stream` / `stream_options` and the placement of Anthropic message
/// block `cache_control: {"type": "ephemeral"}` markers are normalized. Tool
/// definitions, nested tool arguments, system content, and unknown fields remain
/// part of the comparison. Identical requests are permitted for retries.
///
/// # Errors
/// Returns an explanation for a malformed request or a changed input prefix.
pub fn assert_append_only(
    protocol: CacheProtocol,
    previous: &Value,
    current: &Value,
) -> Result<(), String> {
    let (previous_settings, previous_history) = request_parts(protocol, previous)?;
    let (current_settings, current_history) = request_parts(protocol, current)?;
    same_settings(&previous_settings, &current_settings)?;
    if current_history.len() < previous_history.len() {
        return Err("conversation history shrank without a compaction boundary".to_owned());
    }
    for (index, (old, new)) in previous_history.iter().zip(&current_history).enumerate() {
        if old != new {
            return Err(format!("previous conversation item {index} changed"));
        }
    }
    Ok(())
}

/// Permit conversation replacement at one verified compaction boundary.
///
/// The caller MUST identify the boundary using a successful `Compacted` event
/// from the same run, pass that event's checkpoint ID, and consume the exception
/// exactly once. A request containing checkpoint text alone does not authorize
/// this assertion. All later ordinary requests must use [`assert_append_only`]
/// against the new baseline. Failed compaction does not create an exception.
///
/// Input settings and system/developer instructions still must remain unchanged.
/// The checkpoint must be new and occur exactly once as a user message, with a
/// nonempty summary after `[COMPACTION CHECKPOINT {checkpoint_id}]\n`.
///
/// # Errors
/// Returns an explanation for malformed input, a changed configuration or
/// instruction, or a missing, reused, or duplicated checkpoint.
pub fn assert_compaction_transition(
    protocol: CacheProtocol,
    previous: &Value,
    current: &Value,
    checkpoint_id: &str,
) -> Result<(), String> {
    if checkpoint_id.is_empty() || checkpoint_id.contains(['\n', '\r', '[', ']']) {
        return Err("invalid compaction checkpoint ID".to_owned());
    }
    let (previous_settings, previous_history) = request_parts(protocol, previous)?;
    let (current_settings, current_history) = request_parts(protocol, current)?;
    same_settings(&previous_settings, &current_settings)?;
    let instructions = |history: &[Value]| {
        history
            .iter()
            .filter(|item| matches!(item["role"].as_str(), Some("system" | "developer")))
            .cloned()
            .collect::<Vec<_>>()
    };
    if instructions(&previous_history) != instructions(&current_history) {
        return Err("system/developer messages changed during compaction".to_owned());
    }
    let marker = format!("[COMPACTION CHECKPOINT {checkpoint_id}]\n");
    if previous_history
        .iter()
        .flat_map(message_texts)
        .any(|text| text.starts_with(&marker))
    {
        return Err("compaction checkpoint was already present in previous input".to_owned());
    }
    let checkpoints: Vec<_> = current_history
        .iter()
        .filter(|item| item["role"] == "user")
        .flat_map(message_texts)
        .filter_map(|text| text.strip_prefix(&marker))
        .collect();
    if checkpoints.len() != 1 || checkpoints[0].trim().is_empty() {
        return Err(
            "expected exactly one new user compaction checkpoint with a summary".to_owned(),
        );
    }
    Ok(())
}

fn request_parts(
    protocol: CacheProtocol,
    request: &Value,
) -> Result<(Map<String, Value>, Vec<Value>), String> {
    let mut settings = request
        .as_object()
        .ok_or("request must be a JSON object")?
        .clone();
    required_string(request, "model")?;
    if protocol == CacheProtocol::Codex && !request["instructions"].is_string() {
        return Err("Codex instructions must be present as a string".to_owned());
    }
    if let Some(tools) = request.get("tools")
        && !tools
            .as_array()
            .is_some_and(|tools| tools.iter().all(Value::is_object))
    {
        return Err("tools must be an array of objects".to_owned());
    }
    let mut history = settings
        .remove(protocol.history_key())
        .and_then(|value| value.as_array().cloned())
        .ok_or_else(|| format!("{} must be present as an array", protocol.history_key()))?;
    for (index, item) in history.iter_mut().enumerate() {
        validate_item(protocol, item)
            .map_err(|error| format!("{}[{index}]: {error}", protocol.history_key()))?;
        if protocol == CacheProtocol::Anthropic
            && let Some(blocks) = item["content"].as_array_mut()
        {
            for block in blocks {
                // Deliberately nonrecursive: a tool input/schema can have a
                // meaningful property named cache_control, stream, or messages.
                if block.get("cache_control") == Some(&serde_json::json!({"type": "ephemeral"}))
                    && let Some(block) = block.as_object_mut()
                {
                    block.remove("cache_control");
                }
            }
        }
    }
    settings.remove("stream");
    settings.remove("stream_options");
    Ok((settings, history))
}

fn same_settings(
    previous: &Map<String, Value>,
    current: &Map<String, Value>,
) -> Result<(), String> {
    if previous != current {
        return Err(
            "request input settings changed (model, tools, instructions, or other fields)"
                .to_owned(),
        );
    }
    Ok(())
}

fn required_string(value: &Value, key: &str) -> Result<(), String> {
    if !value[key].as_str().is_some_and(|value| !value.is_empty()) {
        return Err(format!("{key} must be a nonempty string"));
    }
    Ok(())
}

fn validate_item(protocol: CacheProtocol, item: &Value) -> Result<(), String> {
    if !item.is_object() {
        return Err("conversation item must be an object".to_owned());
    }
    if protocol == CacheProtocol::Codex {
        match item["type"].as_str() {
            Some("function_call") => {
                for field in ["call_id", "name", "arguments"] {
                    required_string(item, field)?;
                }
                return Ok(());
            }
            Some("function_call_output") => {
                required_string(item, "call_id")?;
                if !item["output"].is_string() {
                    return Err("function_call_output must contain a string output".to_owned());
                }
                return Ok(());
            }
            Some("message") => {}
            _ => return Err("unsupported or missing Codex input type".to_owned()),
        }
    }
    let role = item["role"]
        .as_str()
        .ok_or("message role must be a string")?;
    if !matches!(role, "system" | "developer" | "user" | "assistant" | "tool") {
        return Err("unsupported message role".to_owned());
    }
    if protocol == CacheProtocol::Anthropic && !matches!(role, "user" | "assistant") {
        return Err("Anthropic messages must use user or assistant roles".to_owned());
    }
    let content = &item["content"];
    if content.is_string()
        || content.as_array().is_some_and(|blocks| {
            blocks
                .iter()
                .all(|block| block.is_object() && block.get("type").is_some_and(Value::is_string))
        })
        || (protocol == CacheProtocol::OpenAi
            && role == "assistant"
            && content.is_null()
            && item["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty() && calls.iter().all(Value::is_object)))
    {
        return Ok(());
    }
    Err("message requires string/block content or assistant tool calls".to_owned())
}

fn message_texts(item: &Value) -> Vec<&str> {
    match &item["content"] {
        Value::String(text) => vec![text],
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| {
                matches!(
                    block["type"].as_str(),
                    Some("text" | "input_text" | "output_text")
                )
            })
            .filter_map(|block| block["text"].as_str())
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PROTOCOLS: [CacheProtocol; 3] = [
        CacheProtocol::OpenAi,
        CacheProtocol::Codex,
        CacheProtocol::Anthropic,
    ];

    fn message(protocol: CacheProtocol, role: &str, text: &str) -> Value {
        match protocol {
            CacheProtocol::OpenAi => json!({"role": role, "content": text}),
            CacheProtocol::Codex => json!({"type": "message", "role": role,
                "content": [{"type": "input_text", "text": text}]}),
            CacheProtocol::Anthropic => json!({"role": role,
                "content": [{"type": "text", "text": text}]}),
        }
    }

    fn fixture(protocol: CacheProtocol) -> Value {
        let mut request = json!({"model": "test-model", "stream": false,
            "tools": [{"name": "a", "schema": {"required": ["first", "second"],
                "properties": {"cache_control": {"const": "keep"}}}}, {"name": "b"}],
            "temperature": 0, "reasoning_effort": "medium", "prompt_cache_key": "run-1"});
        request[protocol.history_key()] = json!([
            message(protocol, "user", "first request"),
            message(protocol, "assistant", "old result"),
        ]);
        match protocol {
            CacheProtocol::OpenAi => request["messages"]
                .as_array_mut()
                .unwrap()
                .insert(0, message(protocol, "system", "fixed instructions")),
            CacheProtocol::Codex => request["instructions"] = json!("fixed instructions"),
            CacheProtocol::Anthropic => {
                request["system"] = json!([
                    {"type": "text", "text": "fixed instructions", "cache_control": {"type": "ephemeral"}}
                ])
            }
        }
        request
    }

    fn appended(protocol: CacheProtocol, previous: &Value) -> Value {
        let mut current = previous.clone();
        current[protocol.history_key()]
            .as_array_mut()
            .unwrap()
            .push(message(protocol, "user", "continue"));
        current
    }

    fn compacted(protocol: CacheProtocol, previous: &Value) -> Value {
        let mut current = previous.clone();
        let history = current[protocol.history_key()].as_array_mut().unwrap();
        history.retain(|item| matches!(item["role"].as_str(), Some("system" | "developer")));
        history.push(message(
            protocol,
            "user",
            "[COMPACTION CHECKPOINT ckpt-1]\nsummary",
        ));
        current
    }

    #[test]
    fn append_and_retry_preserve_the_entire_prefix() {
        for protocol in PROTOCOLS {
            let previous = fixture(protocol);
            let mut current = appended(protocol, &previous);
            current["stream"] = json!(true);
            current["stream_options"] = json!({"include_usage": true});
            assert_append_only(protocol, &previous, &current).unwrap();
            assert_append_only(protocol, &previous, &previous).unwrap();
        }
    }

    #[test]
    fn input_settings_mutations_are_rejected_even_during_compaction() {
        for protocol in PROTOCOLS {
            let previous = fixture(protocol);
            for (key, replacement) in [
                ("model", json!("different-model")),
                ("prompt_cache_key", json!("run-2")),
                ("reasoning_effort", json!("high")),
                ("temperature", json!(1)),
                ("unknown_future_setting", json!(true)),
            ] {
                let mut current = appended(protocol, &previous);
                current[key] = replacement.clone();
                assert!(
                    assert_append_only(protocol, &previous, &current).is_err(),
                    "{key}"
                );
                let mut compact = compacted(protocol, &previous);
                compact[key] = replacement;
                assert!(
                    assert_compaction_transition(protocol, &previous, &compact, "ckpt-1").is_err()
                );
            }
            let mut current = appended(protocol, &previous);
            current["tools"].as_array_mut().unwrap().reverse();
            assert!(assert_append_only(protocol, &previous, &current).is_err());
            let mut current = appended(protocol, &previous);
            current["tools"][0]["schema"]["required"]
                .as_array_mut()
                .unwrap()
                .reverse();
            assert!(assert_append_only(protocol, &previous, &current).is_err());
            let mut current = appended(protocol, &previous);
            current["tools"][0]["schema"]["properties"]["cache_control"]["const"] =
                json!("changed");
            assert!(assert_append_only(protocol, &previous, &current).is_err());
        }
    }

    #[test]
    fn old_tool_output_and_unsampled_suffix_changes_are_rejected() {
        for protocol in PROTOCOLS {
            let mut previous = fixture(protocol);
            let long_output = format!("{}original suffix", "x".repeat(100_000));
            let tool_output = match protocol {
                CacheProtocol::OpenAi => {
                    json!({"role": "tool", "tool_call_id": "call-1", "content": long_output})
                }
                CacheProtocol::Codex => {
                    json!({"type": "function_call_output", "call_id": "call-1", "output": long_output})
                }
                CacheProtocol::Anthropic => {
                    json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call-1",
                    "content": [{"type": "text", "text": long_output}]}]})
                }
            };
            previous[protocol.history_key()]
                .as_array_mut()
                .unwrap()
                .push(tool_output);
            for replacement in [
                "[moved to file]".to_owned(),
                format!("{}changed suffix", "x".repeat(100_000)),
            ] {
                let mut current = appended(protocol, &previous);
                let index = previous[protocol.history_key()].as_array().unwrap().len() - 1;
                let item = &mut current[protocol.history_key()][index];
                match protocol {
                    CacheProtocol::OpenAi => item["content"] = json!(replacement),
                    CacheProtocol::Codex => item["output"] = json!(replacement),
                    CacheProtocol::Anthropic => {
                        item["content"][0]["content"][0]["text"] = json!(replacement)
                    }
                }
                assert!(assert_append_only(protocol, &previous, &current).is_err());
            }
        }
    }

    #[test]
    fn anthropic_only_normalizes_direct_message_breakpoint_placement() {
        let protocol = CacheProtocol::Anthropic;
        let mut previous = fixture(protocol);
        previous["messages"][1]["content"] = json!([{"type": "tool_use", "id": "call-1", "name": "test",
            "input": {"cache_control": "argument", "stream": false}, "cache_control": {"type": "ephemeral"}}]);
        let mut current = appended(protocol, &previous);
        current["messages"][1]["content"][0]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        current["messages"][2]["content"][0]["cache_control"] = json!({"type": "ephemeral"});
        assert_append_only(protocol, &previous, &current).unwrap();
        let mut changed = current.clone();
        changed["messages"][1]["content"][0]["input"]["cache_control"] = json!("changed");
        assert!(assert_append_only(protocol, &previous, &changed).is_err());
        let mut changed = current.clone();
        changed["messages"][1]["content"][0]["input"]["stream"] = json!(true);
        assert!(assert_append_only(protocol, &previous, &changed).is_err());
        let mut changed = current.clone();
        changed["system"][0]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        assert!(assert_append_only(protocol, &previous, &changed).is_err());
        current["messages"][1]["content"][0]["cache_control"] =
            json!({"type": "ephemeral", "ttl": "1h"});
        assert!(assert_append_only(protocol, &previous, &current).is_err());
    }

    #[test]
    fn malformed_or_missing_requests_cannot_pass_as_an_empty_prefix() {
        for protocol in PROTOCOLS {
            let valid = fixture(protocol);
            let mut missing = valid.clone();
            missing
                .as_object_mut()
                .unwrap()
                .remove(protocol.history_key());
            let mut wrong_type = valid.clone();
            wrong_type[protocol.history_key()] = json!({});
            let mut bad_item = valid.clone();
            bad_item[protocol.history_key()] = json!([{}]);
            let mut missing_model = valid.clone();
            missing_model.as_object_mut().unwrap().remove("model");
            for invalid in [
                Value::Null,
                json!([]),
                missing,
                wrong_type,
                bad_item,
                missing_model,
            ] {
                assert!(assert_append_only(protocol, &invalid, &valid).is_err());
                assert!(assert_append_only(protocol, &valid, &invalid).is_err());
                assert!(
                    assert_compaction_transition(protocol, &invalid, &valid, "ckpt-1").is_err()
                );
            }
        }
    }

    #[test]
    fn compaction_is_a_single_boundary_followed_by_a_new_append_only_baseline() {
        for protocol in PROTOCOLS {
            let previous = fixture(protocol);
            let current = compacted(protocol, &previous);
            assert!(assert_append_only(protocol, &previous, &current).is_err());
            assert_compaction_transition(protocol, &previous, &current, "ckpt-1").unwrap();
            let appended = appended(protocol, &current);
            assert_append_only(protocol, &current, &appended).unwrap();
            assert!(assert_compaction_transition(protocol, &current, &appended, "ckpt-1").is_err());
            assert!(
                assert_compaction_transition(protocol, &previous, &current, "wrong-id").is_err()
            );
            let mut duplicate = current.clone();
            duplicate[protocol.history_key()]
                .as_array_mut()
                .unwrap()
                .push(message(
                    protocol,
                    "user",
                    "[COMPACTION CHECKPOINT ckpt-1]\nother summary",
                ));
            assert!(
                assert_compaction_transition(protocol, &previous, &duplicate, "ckpt-1").is_err()
            );
            for (role, text) in [
                ("assistant", "[COMPACTION CHECKPOINT ckpt-1]\nsummary"),
                ("user", "[COMPACTION CHECKPOINT ckpt-1]\n"),
            ] {
                let mut invalid = current.clone();
                *invalid[protocol.history_key()]
                    .as_array_mut()
                    .unwrap()
                    .last_mut()
                    .unwrap() = message(protocol, role, text);
                assert!(
                    assert_compaction_transition(protocol, &previous, &invalid, "ckpt-1").is_err()
                );
            }
        }
    }

    #[test]
    fn ordinary_append_cannot_change_instructions() {
        for protocol in PROTOCOLS {
            let previous = fixture(protocol);
            let mut current = appended(protocol, &previous);
            match protocol {
                CacheProtocol::OpenAi => {
                    current["messages"][0]["content"] = json!("changed system")
                }
                CacheProtocol::Codex => current["instructions"] = json!("changed instructions"),
                CacheProtocol::Anthropic => current["system"][0]["text"] = json!("changed system"),
            }
            assert!(assert_append_only(protocol, &previous, &current).is_err());
        }
    }

    #[test]
    fn compaction_cannot_change_system_or_developer_instructions() {
        for protocol in PROTOCOLS {
            let mut previous = fixture(protocol);
            if protocol == CacheProtocol::OpenAi {
                previous["messages"]
                    .as_array_mut()
                    .unwrap()
                    .insert(1, message(protocol, "developer", "fixed developer"));
            }
            let mut current = compacted(protocol, &previous);
            match protocol {
                CacheProtocol::OpenAi => {
                    current["messages"][0]["content"] = json!("changed system")
                }
                CacheProtocol::Codex => current["instructions"] = json!("changed instructions"),
                CacheProtocol::Anthropic => current["system"][0]["text"] = json!("changed system"),
            }
            assert!(assert_compaction_transition(protocol, &previous, &current, "ckpt-1").is_err());
            if protocol == CacheProtocol::OpenAi {
                let mut current = compacted(protocol, &previous);
                current["messages"][1]["content"] = json!("changed developer");
                assert!(
                    assert_compaction_transition(protocol, &previous, &current, "ckpt-1").is_err()
                );
            }
        }
    }
}
