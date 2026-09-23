use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Constant-size metadata, even for a large conversation or image input.
#[derive(Clone)]
pub(super) struct RequestPrefix {
    settings: [u8; 32],
    messages: [u8; 32],
    message_count: usize,
}

pub(super) struct WireInput {
    settings: [u8; 32],
    messages: Vec<Value>,
}

fn digest(value: &impl Serialize) -> Option<[u8; 32]> {
    Some(Sha256::digest(serde_json::to_vec(value).ok()?).into())
}

impl WireInput {
    pub(super) fn new(request: &impl Serialize, protocol: &str) -> Option<Self> {
        let mut value = serde_json::to_value(request).ok()?;
        let settings = value.as_object_mut()?;
        let input = settings
            .remove("messages")
            .or_else(|| settings.remove("input"))?;
        let Value::Array(mut messages) = input else {
            return None;
        };
        // Transport choices do not change the model input.
        settings.remove("stream");
        settings.remove("stream_options");
        if protocol == "anthropic-messages" {
            // Anthropic moves the last explicit breakpoint when appending input.
            // Ignore only content-block metadata, never similarly named keys in
            // tool arguments, result text, schemas, or other request settings.
            for message in &mut messages {
                if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                    for block in content {
                        if let Some(block) = block.as_object_mut() {
                            block.remove("cache_control");
                        }
                    }
                }
            }
        }
        Some(Self {
            settings: digest(settings)?,
            messages,
        })
    }

    pub(super) fn prefix(&self) -> Option<RequestPrefix> {
        Some(RequestPrefix {
            settings: self.settings,
            messages: digest(&self.messages)?,
            message_count: self.messages.len(),
        })
    }

    pub(super) fn extends(&self, previous: &RequestPrefix) -> bool {
        self.settings == previous.settings
            && self
                .messages
                .get(..previous.message_count)
                .and_then(|messages| digest(&messages))
                == Some(previous.messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ChatRequest, ContentBlock, Message, Role, ToolResultContent};
    use crate::wire::anthropic::to_wire_request;
    use serde_json::json;

    #[test]
    fn anthropic_moving_breakpoint_preserves_prefix_but_argument_changes_do_not() {
        let mut request: ChatRequest = serde_json::from_value(json!({
            "model":"test", "messages":[{
                "role":"user", "content":[{"type":"text", "text":"Use example"}]
            }, {
                "role":"assistant", "content":[{
                    "type":"tool_use", "id":"call-1", "name":"example",
                    "input":{"cache_control":"argument value"}
                }]
            }]
        }))
        .unwrap();
        let original = to_wire_request(&request, false);
        let prefix = WireInput::new(&original, "anthropic-messages")
            .unwrap()
            .prefix()
            .unwrap();
        request.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_call_id: "call-1".into(),
                content: vec![ToolResultContent::Text {
                    text: "result".into(),
                }],
                is_error: false,
            }],
        });
        let appended = to_wire_request(&request, false);
        let original_json = serde_json::to_value(&original).unwrap();
        let appended_json = serde_json::to_value(&appended).unwrap();
        assert!(original_json["messages"][1]["content"][0]["cache_control"].is_object());
        assert!(appended_json["messages"][1]["content"][0]["cache_control"].is_null());
        assert!(appended_json["messages"][2]["content"][0]["cache_control"].is_object());
        assert!(
            WireInput::new(&appended, "anthropic-messages")
                .unwrap()
                .extends(&prefix)
        );
        let ContentBlock::ToolUse { input, .. } = &mut request.messages[1].content[0] else {
            panic!("tool call must remain in the request");
        };
        input["cache_control"] = json!("changed argument");
        assert!(
            !WireInput::new(&to_wire_request(&request, false), "anthropic-messages")
                .unwrap()
                .extends(&prefix)
        );
    }

    #[test]
    fn streaming_transport_can_change_without_replacing_the_input() {
        let original = json!({"model":"test", "messages":[{"role":"user", "content":"prefix"}], "stream":false});
        let prefix = WireInput::new(&original, "openai-chat-completions")
            .unwrap()
            .prefix()
            .unwrap();
        let mut streaming = original;
        streaming["stream"] = json!(true);
        streaming["stream_options"] = json!({"include_usage":true});
        assert!(
            WireInput::new(&streaming, "openai-chat-completions")
                .unwrap()
                .extends(&prefix)
        );
    }
}
