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

/// One Responses API `reasoning` output item, streamed before `response.completed`.
///
/// Responses SSE renders it at its own `output_index` as `response.output_item.added`
/// (a provisional item whose `encrypted_content` differs from the final value, or
/// omits it), the summary part/delta/done events, then `response.output_item.done`
/// with the confirmed [`Self::done_item`]. Only the done item may be replayed.
/// Chat Completions has no reasoning item, so it renders nothing for it.
#[derive(Debug, Clone)]
pub struct ReasoningItem {
    output_index: usize,
    id: String,
    summary: Vec<String>,
    added_encrypted_content: Option<String>,
    encrypted_content: String,
    extra: serde_json::Map<String, Value>,
}

impl ReasoningItem {
    /// A summary-less item whose `output_item.added` carries the provisional
    /// `provisional-{id}` ciphertext and whose done item carries `encrypted_content`.
    pub fn new(output_index: usize, id: &str, encrypted_content: &str) -> Self {
        Self {
            output_index,
            id: id.to_owned(),
            summary: Vec::new(),
            added_encrypted_content: Some(format!("provisional-{id}")),
            encrypted_content: encrypted_content.to_owned(),
            extra: serde_json::Map::new(),
        }
    }

    /// Streams one `summary_text` part as these `reasoning_summary_text.delta` fragments.
    #[must_use]
    pub fn with_summary(mut self, fragments: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        self.summary = fragments
            .into_iter()
            .map(|fragment| fragment.as_ref().to_owned())
            .collect();
        self
    }

    /// Uses `partial` as the provisional `encrypted_content` of `output_item.added`.
    #[must_use]
    pub fn with_added_encrypted_content(mut self, partial: &str) -> Self {
        self.added_encrypted_content = Some(partial.to_owned());
        self
    }

    /// Omits `encrypted_content` from `output_item.added`.
    #[must_use]
    pub fn without_added_encrypted_content(mut self) -> Self {
        self.added_encrypted_content = None;
        self
    }

    /// Adds a field (for example `status` or an unknown field) to the done item only.
    #[must_use]
    pub fn with_extra(mut self, key: &str, value: Value) -> Self {
        self.extra.insert(key.to_owned(), value);
        self
    }

    /// The Responses output index of this item.
    pub const fn output_index(&self) -> usize {
        self.output_index
    }

    /// The confirmed item exactly as `response.output_item.done` sends it.
    pub fn done_item(&self) -> Value {
        let summary = if self.summary.is_empty() {
            json!([])
        } else {
            json!([{"type": "summary_text", "text": self.summary.concat()}])
        };
        let mut item = json!({"type": "reasoning", "id": self.id, "summary": summary,
            "encrypted_content": self.encrypted_content});
        for (key, value) in &self.extra {
            item[key] = value.clone();
        }
        item
    }

    fn responses_frames(&self) -> Vec<String> {
        let index = self.output_index;
        let mut added = json!({"type": "reasoning", "id": self.id, "summary": []});
        if let Some(partial) = &self.added_encrypted_content {
            added["encrypted_content"] = json!(partial);
        }
        let mut frames = vec![sse_frame(json!({
            "type": "response.output_item.added", "output_index": index, "item": added,
        }))];
        if !self.summary.is_empty() {
            let part = |text: &str| json!({"type": "summary_text", "text": text});
            let event = |kind: &str| {
                json!({"type": kind, "item_id": self.id, "output_index": index,
                    "summary_index": 0})
            };
            let mut started = event("response.reasoning_summary_part.added");
            started["part"] = part("");
            frames.push(sse_frame(started));
            for fragment in &self.summary {
                let mut delta = event("response.reasoning_summary_text.delta");
                delta["delta"] = json!(fragment);
                frames.push(sse_frame(delta));
            }
            let text = self.summary.concat();
            let mut done = event("response.reasoning_summary_text.done");
            done["text"] = json!(text);
            frames.push(sse_frame(done));
            let mut finished = event("response.reasoning_summary_part.done");
            finished["part"] = part(&text);
            frames.push(sse_frame(finished));
        }
        frames.push(sse_frame(json!({
            "type": "response.output_item.done", "output_index": index,
            "item": self.done_item(),
        })));
        frames
    }
}

fn sse_frame(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap_or_default()
    )
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
    reasoning: Vec<ReasoningItem>,
    text_output_index: usize,
    prompt_tokens: u64,
    completion_tokens: u32,
    cached_tokens: Option<u64>,
    usage_omitted: bool,
}

impl ScriptedResponse {
    /// Adds a Responses reasoning output item; call repeatedly for several items.
    ///
    /// Give each item an output index distinct from the other items and from the
    /// text ([`Self::with_text_output_index`]) or tool call index.
    #[must_use]
    pub fn with_reasoning(mut self, item: ReasoningItem) -> Self {
        self.reasoning.push(item);
        self
    }

    /// Streams Responses text deltas at `output_index` (default 0).
    #[must_use]
    pub const fn with_text_output_index(mut self, output_index: usize) -> Self {
        self.text_output_index = output_index;
        self
    }

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
            reasoning: Vec::new(),
            text_output_index: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: None,
            usage_omitted: false,
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

    /// Omits `usage` from the Responses `response.completed` event, as the Codex
    /// backend may. The prompt-cache fixture still observes the request, so later
    /// requests' cached tokens stay derived from their input. Chat Completions
    /// rendering is unchanged.
    #[must_use]
    pub const fn without_usage(mut self) -> Self {
        self.usage_omitted = true;
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
    ///
    /// Reasoning items ([`Self::with_reasoning`]) and the text or call stream in
    /// output-index order, each completing before the next begins.
    pub fn responses_frames(&self) -> Vec<String> {
        let frame = sse_frame;
        let mut frames = vec![frame(json!({
            "type": "response.created",
            "response": {"id": self.id, "object": "response", "status": "in_progress",
                "model": self.model, "output": []},
        }))];
        let mut items: Vec<_> = self
            .reasoning
            .iter()
            .map(|item| (item.output_index, item.responses_frames()))
            .collect();
        let mut main = Vec::new();
        let main_index = match &self.kind {
            ResponseKind::Text => {
                let index = self.text_output_index;
                for fragment in &self.fragments {
                    main.push(frame(json!({
                        "type": "response.output_text.delta", "item_id": format!("msg_{index}"),
                        "output_index": index, "content_index": 0, "delta": fragment,
                    })));
                }
                index
            }
            ResponseKind::Compaction { encrypted_content } => {
                let item = json!({"type": "compaction", "id": format!("cmp_{}", self.id),
                    "encrypted_content": encrypted_content});
                for event in ["response.output_item.added", "response.output_item.done"] {
                    main.push(frame(json!({
                        "type": event, "output_index": 0, "item": item,
                    })));
                }
                0
            }
            ResponseKind::CustomToolCall { index, id, name } => {
                let item_id = format!("ctc_{id}");
                let item = |input: String| {
                    json!({"type": "custom_tool_call", "id": item_id, "call_id": id,
                        "name": name, "input": input})
                };
                main.push(frame(json!({
                    "type": "response.output_item.added", "output_index": index,
                    "item": item(String::new()),
                })));
                for fragment in &self.fragments {
                    main.push(frame(json!({
                        "type": "response.custom_tool_call_input.delta",
                        "item_id": item_id, "output_index": index, "delta": fragment,
                    })));
                }
                main.push(frame(json!({
                    "type": "response.custom_tool_call_input.done",
                    "item_id": item_id, "output_index": index, "input": self.fragments.concat(),
                })));
                main.push(frame(json!({
                    "type": "response.output_item.done", "output_index": index,
                    "item": item(self.fragments.concat()),
                })));
                *index
            }
            ResponseKind::ToolCall { index, id, name } => {
                let item_id = format!("fc_{id}");
                let item = |arguments: String| {
                    json!({"type": "function_call", "id": item_id, "call_id": id,
                        "name": name, "arguments": arguments})
                };
                main.push(frame(json!({
                    "type": "response.output_item.added", "output_index": index,
                    "item": item(String::new()),
                })));
                for fragment in &self.fragments {
                    main.push(frame(json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": item_id, "output_index": index, "delta": fragment,
                    })));
                }
                main.push(frame(json!({
                    "type": "response.output_item.done", "output_index": index,
                    "item": item(self.fragments.concat()),
                })));
                *index
            }
        };
        items.push((main_index, main));
        // Stable: equal indices keep reasoning first, but scripts should not collide.
        items.sort_by_key(|(index, _)| *index);
        frames.extend(items.into_iter().flat_map(|(_, item)| item));
        let mut usage = json!({
            "input_tokens": self.prompt_tokens,
            "output_tokens": self.completion_tokens,
            "total_tokens": self.prompt_tokens + u64::from(self.completion_tokens),
        });
        if let Some(cached_tokens) = self.cached_tokens {
            usage["input_tokens_details"] = json!({"cached_tokens": cached_tokens});
        }
        let mut response = json!({"id": self.id, "object": "response", "status": "completed",
            "model": self.model});
        if !self.usage_omitted {
            response["usage"] = usage;
        }
        frames.push(frame(
            json!({"type": "response.completed", "response": response}),
        ));
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
