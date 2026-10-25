//! In-memory chat completion scenarios rendered as SSE frames or JSON.

use serde_json::{Value, json};

const CREATED: u64 = 1_700_000_000;

#[derive(Debug, Clone)]
enum ResponseKind {
    Text,
    Compaction {
        encrypted_content: String,
    },
    CustomToolCall {
        index: usize,
        id: String,
        name: String,
    },
    ToolCall {
        index: usize,
        id: String,
        name: String,
    },
}

/// An owned, deterministic response script with no transport or clock dependency.
#[derive(Debug, Clone)]
pub struct ScriptedResponse {
    pub(crate) delay: std::time::Duration,
    pub(crate) gate: Option<crate::server::ResponseGate>,
    id: String,
    model: String,
    fragments: Vec<String>,
    kind: ResponseKind,
    prompt_tokens: u64,
    completion_tokens: u32,
    cached_tokens: Option<u64>,
}

impl ScriptedResponse {
    /// Waits for an explicit test signal before writing this response.
    #[must_use]
    pub fn with_gate(mut self, gate: crate::server::ResponseGate) -> Self {
        self.gate = Some(gate);
        self
    }

    pub const fn with_delay(mut self, delay: std::time::Duration) -> Self {
        self.delay = delay;
        self
    }
    /// Builds a text script. Usage defaults to zero; fragments are copied in order.
    pub fn text_stream(
        id: &str,
        model: &str,
        fragments: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Self {
        Self {
            id: id.to_owned(),
            delay: std::time::Duration::ZERO,
            gate: None,
            model: model.to_owned(),
            fragments: fragments
                .into_iter()
                .map(|fragment| fragment.as_ref().to_owned())
                .collect(),
            kind: ResponseKind::Text,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: None,
        }
    }

    /// Builds one indexed tool call with verbatim JSON argument fragments.
    ///
    /// The positional parameters intentionally mirror the scenario fixture API.
    pub fn tool_call(
        id: &str,
        model: &str,
        index: usize,
        call_id: &str,
        name: &str,
        arguments: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Self {
        Self {
            kind: ResponseKind::ToolCall {
                index,
                id: call_id.to_owned(),
                name: name.to_owned(),
            },
            ..Self::text_stream(id, model, arguments)
        }
    }

    /// Builds a freeform call from raw input fragments (not JSON).
    ///
    /// `call_id` is the canonical tool-use ID; `index` is the Responses output
    /// index / Chat tool-call index. Responses emits `output_item.added`, one
    /// `custom_tool_call_input.delta` per fragment, `custom_tool_call_input.done`,
    /// then `output_item.done` with the complete input. Input events identify the
    /// item by `item_id` / `output_index`, without a `call_id`.
    /// Chat Completions (SSE and non-streaming JSON) instead wraps the concatenated
    /// input in a function call's `{"input": "..."}` arguments.
    ///
    /// To exercise apply_patch, pass `apply_patch::tool_contract::NAME` and the raw
    /// patch fragments, and put a ToolSpec with the matching canonical schema in
    /// the provider request. No JSON escaping of fragments is needed.
    pub fn custom_tool_call(
        id: &str,
        model: &str,
        index: usize,
        call_id: &str,
        name: &str,
        input: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Self {
        Self {
            kind: ResponseKind::CustomToolCall {
                index,
                id: call_id.to_owned(),
                name: name.to_owned(),
            },
            ..Self::text_stream(id, model, input)
        }
    }

    /// Builds a Codex official compaction reply carrying an opaque `encrypted_content`.
    ///
    /// Responses emits one `compaction` output item (`output_item.added`, then
    /// `output_item.done`) before `response.completed`, as the Codex backend answers
    /// a request ending with a `compaction_trigger` item. Chat Completions has no
    /// compaction item, so it renders an empty assistant text instead.
    pub fn compaction(id: &str, model: &str, encrypted_content: &str) -> Self {
        Self {
            kind: ResponseKind::Compaction {
                encrypted_content: encrypted_content.to_owned(),
            },
            ..Self::text_stream(id, model, std::iter::empty::<&str>())
        }
    }

    fn as_chat_function(&self) -> Option<Self> {
        let ResponseKind::CustomToolCall { index, id, name } = &self.kind else {
            return None;
        };
        Some(Self {
            kind: ResponseKind::ToolCall {
                index: *index,
                id: id.clone(),
                name: name.clone(),
            },
            fragments: vec![json!({"input": self.fragments.concat()}).to_string()],
            ..self.clone()
        })
    }

    /// Overrides token counts in both encodings; total is their lossless sum.
    #[must_use]
    pub const fn with_usage(mut self, prompt_tokens: u32, completion_tokens: u32) -> Self {
        self.prompt_tokens = prompt_tokens as u64;
        self.completion_tokens = completion_tokens;
        self
    }

    pub(crate) const fn with_prompt_cache_usage(
        mut self,
        prompt_tokens: u64,
        cached_tokens: u64,
    ) -> Self {
        self.prompt_tokens = prompt_tokens;
        self.cached_tokens = Some(cached_tokens);
        self
    }

    /// Renders complete SSE frames, including usage and the final `[DONE]` frame.
    /// Empty scripts emit one empty delta carrying the role and finish reason.
    pub fn frames(&self) -> Vec<String> {
        if let Some(function) = self.as_chat_function() {
            return function.frames();
        }
        let count = self.fragments.len().max(1);
        let mut frames = Vec::with_capacity(count + 2);
        for position in 0..count {
            let fragment = self.fragments.get(position).map_or("", String::as_str);
            let mut delta = match &self.kind {
                ResponseKind::CustomToolCall { .. } => unreachable!("custom call normalized above"),
                ResponseKind::Text | ResponseKind::Compaction { .. } => {
                    json!({"content": fragment})
                }
                ResponseKind::ToolCall { index, id, name } => {
                    let call = if position == 0 {
                        json!({
                            "index": index, "id": id, "type": "function",
                            "function": {"name": name, "arguments": fragment}
                        })
                    } else {
                        json!({"index": index, "function": {"arguments": fragment}})
                    };
                    json!({"tool_calls": [call]})
                }
            };
            if position == 0 {
                delta["role"] = json!("assistant");
            }
            let finish_reason = (position + 1 == count).then(|| self.finish_reason());
            let mut chunk = self.envelope("chat.completion.chunk");
            chunk["choices"] = json!([{
                "index": 0, "delta": delta, "finish_reason": finish_reason
            }]);
            frames.push(format!("data: {chunk}\n\n"));
        }
        let mut usage = self.envelope("chat.completion.chunk");
        usage["choices"] = json!([]);
        usage["usage"] = self.usage();
        frames.push(format!("data: {usage}\n\n"));
        frames.push("data: [DONE]\n\n".to_owned());
        frames
    }

    /// Concatenates the complete SSE frames into a response body.
    pub fn body(&self) -> String {
        self.frames().concat()
    }

    /// Renders the same script as Responses API SSE, ending with `response.completed`.
    ///
    /// Text fragments become `response.output_text.delta`. A tool call becomes one
    /// `function_call` output item whose arguments arrive as
    /// `response.function_call_arguments.delta` fragments before `output_item.done`.
    pub fn responses_frames(&self) -> Vec<String> {
        let frame = |value: Value| {
            format!(
                "event: {}\ndata: {value}\n\n",
                value["type"].as_str().unwrap_or_default()
            )
        };
        let mut frames = vec![frame(json!({
            "type": "response.created",
            "response": {"id": self.id, "object": "response", "status": "in_progress",
                "model": self.model, "output": []},
        }))];
        match &self.kind {
            ResponseKind::Text => {
                for fragment in &self.fragments {
                    frames.push(frame(json!({
                        "type": "response.output_text.delta", "item_id": "msg_0",
                        "output_index": 0, "content_index": 0, "delta": fragment,
                    })));
                }
            }
            ResponseKind::Compaction { encrypted_content } => {
                let item = json!({"type": "compaction", "id": format!("cmp_{}", self.id),
                    "encrypted_content": encrypted_content});
                for event in ["response.output_item.added", "response.output_item.done"] {
                    frames.push(frame(json!({
                        "type": event, "output_index": 0, "item": item,
                    })));
                }
            }
            ResponseKind::CustomToolCall { index, id, name } => {
                let item_id = format!("ctc_{id}");
                let item = |input: String| {
                    json!({"type": "custom_tool_call", "id": item_id, "call_id": id,
                        "name": name, "input": input})
                };
                frames.push(frame(json!({
                    "type": "response.output_item.added", "output_index": index,
                    "item": item(String::new()),
                })));
                for fragment in &self.fragments {
                    frames.push(frame(json!({
                        "type": "response.custom_tool_call_input.delta",
                        "item_id": item_id, "output_index": index, "delta": fragment,
                    })));
                }
                frames.push(frame(json!({
                    "type": "response.custom_tool_call_input.done",
                    "item_id": item_id, "output_index": index, "input": self.fragments.concat(),
                })));
                frames.push(frame(json!({
                    "type": "response.output_item.done", "output_index": index,
                    "item": item(self.fragments.concat()),
                })));
            }
            ResponseKind::ToolCall { index, id, name } => {
                let item_id = format!("fc_{id}");
                let item = |arguments: String| {
                    json!({"type": "function_call", "id": item_id, "call_id": id,
                        "name": name, "arguments": arguments})
                };
                frames.push(frame(json!({
                    "type": "response.output_item.added", "output_index": index,
                    "item": item(String::new()),
                })));
                for fragment in &self.fragments {
                    frames.push(frame(json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": item_id, "output_index": index, "delta": fragment,
                    })));
                }
                frames.push(frame(json!({
                    "type": "response.output_item.done", "output_index": index,
                    "item": item(self.fragments.concat()),
                })));
            }
        }
        let mut usage = json!({
            "input_tokens": self.prompt_tokens,
            "output_tokens": self.completion_tokens,
            "total_tokens": self.prompt_tokens + u64::from(self.completion_tokens),
        });
        if let Some(cached_tokens) = self.cached_tokens {
            usage["input_tokens_details"] = json!({"cached_tokens": cached_tokens});
        }
        frames.push(frame(json!({
            "type": "response.completed",
            "response": {"id": self.id, "object": "response", "status": "completed",
                "model": self.model, "usage": usage},
        })));
        frames
    }

    /// Renders a non-streaming completion with all fragments reassembled.
    pub fn to_non_streaming_json(&self) -> Value {
        if let Some(function) = self.as_chat_function() {
            return function.to_non_streaming_json();
        }
        let content = self.fragments.concat();
        let message = match &self.kind {
            ResponseKind::CustomToolCall { .. } => unreachable!("custom call normalized above"),
            ResponseKind::Text | ResponseKind::Compaction { .. } => {
                json!({"role": "assistant", "content": content})
            }
            ResponseKind::ToolCall { id, name, .. } => json!({
                "role": "assistant", "content": null,
                "tool_calls": [{
                    "id": id, "type": "function",
                    "function": {"name": name, "arguments": content}
                }]
            }),
        };
        let mut response = self.envelope("chat.completion");
        response["choices"] = json!([{
            "index": 0, "message": message, "finish_reason": self.finish_reason()
        }]);
        response["usage"] = self.usage();
        response
    }

    const fn finish_reason(&self) -> &'static str {
        match &self.kind {
            ResponseKind::Text | ResponseKind::Compaction { .. } => "stop",
            ResponseKind::ToolCall { .. } | ResponseKind::CustomToolCall { .. } => "tool_calls",
        }
    }

    fn envelope(&self, object: &str) -> Value {
        json!({"id": self.id, "object": object, "created": CREATED, "model": self.model})
    }

    fn usage(&self) -> Value {
        let mut usage = json!({
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.prompt_tokens + u64::from(self.completion_tokens)
        });
        if let Some(cached_tokens) = self.cached_tokens {
            usage["prompt_tokens_details"] = json!({"cached_tokens": cached_tokens});
        }
        usage
    }
}
