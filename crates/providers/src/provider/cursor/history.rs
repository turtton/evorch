//! Deterministic canonical history and server-owned checkpoint side state.
use super::wire::{Proto, encode_value};
use crate::{ChatRequest, ContentBlock, Message, ProviderError, Role, ToolResultContent};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct Conversation {
    pub id: String,
    pub checkpoint: Proto,
    pub model_route: Option<super::models::ModelRoute>,
    pub blobs: HashMap<Vec<u8>, Vec<u8>>,
    blob_bytes: usize,
}
impl Conversation {
    pub fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            ..Self::default()
        }
    }
    pub fn store(&mut self, data: Vec<u8>) -> Result<Vec<u8>, ProviderError> {
        let id = Sha256::digest(&data).to_vec();
        self.set(id.clone(), data)?;
        Ok(id)
    }
    pub fn set(&mut self, id: Vec<u8>, data: Vec<u8>) -> Result<(), ProviderError> {
        let old = self.blobs.get(&id).map_or(0, Vec::len);
        let size = self
            .blob_bytes
            .saturating_sub(old)
            .saturating_add(data.len());
        if size > 64 * 1024 * 1024 {
            return Err(ProviderError::Request(
                "Cursor conversation blob budget exceeded".into(),
            ));
        }
        self.blobs.insert(id, data);
        self.blob_bytes = size;
        Ok(())
    }
}
pub(super) fn conversation_key(request: &ChatRequest) -> String {
    let identity = json!({"run": request.observation.as_ref().map(|o| (&o.run_id, o.purpose)), "model": request.model, "tools":request.tools,
        "system":request.messages.iter().filter(|m| m.role == Role::System).collect::<Vec<_>>(),
        "first":request.messages.iter().find(|m| m.role == Role::User), "effort":request.reasoning_effort, "temperature":request.temperature, "tier":request.service_tier});
    Sha256::digest(identity.to_string().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(super) fn system_texts(request: &ChatRequest) -> Vec<String> {
    let mut system: Vec<_> = request
        .messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(text)
        .filter(|s| !s.is_empty())
        .collect();
    if system.is_empty() {
        system.push("You are a helpful assistant.".into());
    }
    system
}
pub(super) fn request_context(request: &ChatRequest) -> Proto {
    let mut context = Proto::new();
    for (index, system) in system_texts(request).iter().enumerate() {
        let rule = Proto::new()
            .string(1, &format!("/evorch/system-prompt/{index}.mdc"))
            .string(2, system)
            .message(3, Proto::new().message(1, Proto::new()))
            .integer(4, 2);
        context = context.message(2, rule);
    }
    for tool in &request.tools {
        context = context.message(7, tool_definition(tool));
    }
    context
}
pub(super) fn tool_definition(tool: &crate::ToolSpec) -> Proto {
    Proto::new()
        .string(1, &tool.name)
        .string(2, &tool.description)
        .message(3, encode_value(&tool.input_schema))
        .string(4, "evorch")
        .string(5, &tool.name)
        .string(6, &tool.input_schema.to_string())
}
pub(super) fn build_run(
    request: &ChatRequest,
    state: &mut Conversation,
    route: &super::models::ModelRoute,
) -> Result<Proto, ProviderError> {
    let active = request.messages.last().filter(|m| {
        m.role == Role::User
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { .. } | ContentBlock::Image { .. }))
            && !m
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
    });
    let history_end = request.messages.len() - usize::from(active.is_some());
    let history = &request.messages[..history_end];
    let mut checkpoint = state.checkpoint.without(&[1, 2, 4, 8])?;
    for system in system_texts(request) {
        checkpoint = checkpoint.bytes(
            1,
            state.store(
                serde_json::to_vec(&json!({"role":"system", "content":system})).expect("JSON"),
            )?,
        );
    }
    let names: HashMap<_, _> = history
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();
    let results: HashMap<_, _> = history
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_call_id, .. } => Some((tool_call_id.as_str(), b)),
            _ => None,
        })
        .collect();
    let mut turn: Option<Proto> = None;
    for (index, message) in history
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role != Role::System)
    {
        let mut content = Vec::new();
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => content.push(json!({"type":"text", "text":text})),
                ContentBlock::Image { media_type, data } => content.push(json!({"type":"image", "image":format!("data:{media_type};base64,{data}"), "mediaType":media_type})),
                ContentBlock::ToolUse { id, name, input } => content.push(json!({"type":"tool-call", "toolCallId":wire_id(id), "toolName":name, "args":input})),
                ContentBlock::Compaction { .. } => return Err(ProviderError::Request("Cursor cannot replay a foreign compaction block".into())),
                _ => {},
            }
        }
        if !content.is_empty() {
            let role = if message.role == Role::Assistant {
                "assistant"
            } else {
                "user"
            };
            checkpoint = checkpoint.bytes(
                1,
                state.store(
                    serde_json::to_vec(&json!({"role":role, "content":content})).expect("JSON"),
                )?,
            );
        }
        for block in &message.content {
            if let ContentBlock::ToolResult {
                tool_call_id,
                is_error,
                ..
            } = block
            {
                let tool_name = names.get(tool_call_id.as_str()).ok_or_else(|| {
                    ProviderError::Request("Cursor history has an orphaned tool result".into())
                })?;
                let value = json!({"role":"tool", "id":wire_id(tool_call_id), "content":[{"type":"tool-result", "toolName":tool_name, "toolCallId":wire_id(tool_call_id), "result":result_text(block), "isError":is_error}]});
                checkpoint =
                    checkpoint.bytes(1, state.store(serde_json::to_vec(&value).expect("JSON"))?);
            }
        }
        if message.role == Role::User
            && message
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { .. } | ContentBlock::Image { .. }))
        {
            if let Some(previous) = turn.take() {
                checkpoint = checkpoint.bytes(8, state.store(Proto::new().message(1, previous).0)?);
            }
            turn = Some(Proto::new().bytes(1, state.store(user_message(message, index)?.0)?));
        } else if message.role == Role::Assistant {
            for block in &message.content {
                let step =
                    match block {
                        ContentBlock::Text { text } => {
                            Some(Proto::new().message(1, Proto::new().string(1, text)))
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            let mut call = Proto::new().message(1, mcp_args(id, name, input));
                            if let Some(result) = results.get(id.as_str()) {
                                call = call.message(2, mcp_result(result));
                            }
                            Some(Proto::new().message(
                                2,
                                Proto::new().message(15, call).string(57, &wire_id(id)),
                            ))
                        }
                        _ => None,
                    };
                if let Some(step) = step
                    && let Some(current) = turn.take()
                {
                    turn = Some(current.bytes(2, state.store(step.0)?));
                }
            }
        }
    }
    if let Some(turn) = turn {
        checkpoint = checkpoint.bytes(8, state.store(Proto::new().message(1, turn).0)?);
    }
    let action = match active {
        Some(user) => {
            Proto::new().message(1, Proto::new().message(1, user_message(user, history_end)?))
        }
        None => Proto::new().message(2, Proto::new()),
    };
    let details = Proto::new()
        .string(1, &route.details_id)
        .string(3, &route.details_id)
        .string(4, &route.details_id)
        .integer(7, u64::from(route.max_mode));
    let mut model = Proto::new()
        .string(1, &route.model_id)
        .integer(2, u64::from(route.max_mode));
    for (id, value) in &route.parameters {
        model = model.message(3, Proto::new().string(1, id).string(2, value));
    }
    Ok(Proto::new().message(
        1,
        Proto::new()
            .message(1, checkpoint)
            .message(2, action)
            .message(3, details)
            .message(9, model)
            .string(5, &state.id),
    ))
}
fn user_message(message: &Message, index: usize) -> Result<Proto, ProviderError> {
    let id = deterministic_id(&format!(
        "{index}:{}",
        serde_json::to_string(message).expect("JSON")
    ));
    let mut output = Proto::new().string(1, &text(message)).string(2, &id);
    let mut images = Proto::new();
    let mut has_images = false;
    for (index, block) in message.content.iter().enumerate() {
        if let ContentBlock::Image { media_type, data } = block {
            has_images = true;
            images = images.message(
                1,
                Proto::new()
                    .string(2, &deterministic_id(&format!("{id}:{index}")))
                    .string(7, media_type)
                    .bytes(
                        8,
                        STANDARD.decode(data).map_err(|_| {
                            ProviderError::Request("Invalid Cursor image base64".into())
                        })?,
                    ),
            );
        }
    }
    if has_images {
        output = output.message(3, images);
    }
    Ok(output)
}
fn deterministic_id(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    uuid::Uuid::from_bytes(digest[..16].try_into().expect("length")).to_string()
}
pub(super) fn wire_id(id: &str) -> String {
    if !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        id.into()
    } else {
        deterministic_id(id)
    }
}
pub(super) fn text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|b| {
            if let ContentBlock::Text { text } = b {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}
pub(super) fn result_text(block: &ContentBlock) -> String {
    if let ContentBlock::ToolResult { content, .. } = block {
        content
            .iter()
            .map(|b| match b {
                ToolResultContent::Text { text } => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        String::new()
    }
}
fn mcp_args(id: &str, name: &str, input: &Value) -> Proto {
    let mut args = Proto::new()
        .string(1, name)
        .string(3, &wire_id(id))
        .string(4, "evorch")
        .string(5, name);
    if let Some(input) = input.as_object() {
        for (key, value) in input {
            args = args.message(
                2,
                Proto::new().string(1, key).message(2, encode_value(value)),
            );
        }
    }
    args
}
fn mcp_result(block: &ContentBlock) -> Proto {
    if matches!(block, ContentBlock::ToolResult { is_error: true, .. }) {
        Proto::new().message(2, Proto::new().string(1, &result_text(block)))
    } else {
        mcp_text_result(&result_text(block))
    }
}
pub(super) fn mcp_text_result(text: &str) -> Proto {
    Proto::new().message(
        1,
        Proto::new().message(1, Proto::new().message(1, Proto::new().string(1, text))),
    )
}
