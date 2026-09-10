use providers::{ChatRequest, ContentBlock, Message, Role, wire};
use serde_json::json;

fn request() -> ChatRequest {
    ChatRequest {
        model: "vision".into(),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            }],
        }],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        observation: None,
    }
}

#[test]
fn image_only_input_serializes_as_openai_image_url() {
    // Given: an image-only user message.
    let request = request();
    // When: the OpenAI request is serialized.
    let value = serde_json::to_value(wire::openai::to_wire_request(&request, false)).unwrap();
    // Then: the image is retained as an inline data URL.
    assert_eq!(
        value["messages"][0]["content"],
        json!([
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8="}}
        ])
    );
}

#[test]
fn image_input_serializes_as_anthropic_base64_source() {
    // Given: an image-only user message.
    let request = request();
    // When: the Anthropic request is serialized.
    let value = serde_json::to_value(wire::anthropic::to_wire_request(&request, false)).unwrap();
    // Then: the source carries the MIME type and original base64 bytes.
    assert_eq!(
        value["messages"][0]["content"],
        json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
        ])
    );
}
