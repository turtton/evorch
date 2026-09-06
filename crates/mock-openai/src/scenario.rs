//! In-memory chat completion scenarios rendered as SSE frames or JSON.

use serde_json::{Value, json};

const CREATED: u64 = 1_700_000_000;

#[derive(Debug, Clone)]
enum ResponseKind {
    Text,
    ToolCall {
        index: usize,
        id: String,
        name: String,
    },
}

/// An owned, deterministic response script with no transport or clock dependency.
#[derive(Debug, Clone)]
pub struct ScriptedResponse {
    id: String,
    model: String,
    fragments: Vec<String>,
    kind: ResponseKind,
    prompt_tokens: u32,
    completion_tokens: u32,
}

impl ScriptedResponse {
    /// Builds a text script. Usage defaults to zero; fragments are copied in order.
    pub fn text_stream(
        id: &str,
        model: &str,
        fragments: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Self {
        Self {
            id: id.to_owned(),
            model: model.to_owned(),
            fragments: fragments
                .into_iter()
                .map(|fragment| fragment.as_ref().to_owned())
                .collect(),
            kind: ResponseKind::Text,
            prompt_tokens: 0,
            completion_tokens: 0,
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

    /// Overrides token counts in both encodings; total is their lossless sum.
    #[must_use]
    pub const fn with_usage(mut self, prompt_tokens: u32, completion_tokens: u32) -> Self {
        self.prompt_tokens = prompt_tokens;
        self.completion_tokens = completion_tokens;
        self
    }

    /// Renders complete SSE frames, including usage and the final `[DONE]` frame.
    /// Empty scripts emit one empty delta carrying the role and finish reason.
    pub fn frames(&self) -> Vec<String> {
        let count = self.fragments.len().max(1);
        let mut frames = Vec::with_capacity(count + 2);
        for position in 0..count {
            let fragment = self.fragments.get(position).map_or("", String::as_str);
            let mut delta = match &self.kind {
                ResponseKind::Text => json!({"content": fragment}),
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

    /// Renders a non-streaming completion with all fragments reassembled.
    pub fn to_non_streaming_json(&self) -> Value {
        let content = self.fragments.concat();
        let message = match &self.kind {
            ResponseKind::Text => json!({"role": "assistant", "content": content}),
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
            ResponseKind::Text => "stop",
            ResponseKind::ToolCall { .. } => "tool_calls",
        }
    }

    fn envelope(&self, object: &str) -> Value {
        json!({"id": self.id, "object": object, "created": CREATED, "model": self.model})
    }

    fn usage(&self) -> Value {
        json!({
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": u64::from(self.prompt_tokens) + u64::from(self.completion_tokens)
        })
    }
}
