use providers::{ContentBlock, Message, Role};

#[test]
fn image_block_round_trips_without_becoming_text() {
    let input = serde_json::json!({
        "role": "user",
        "content": [{"type": "image", "media_type": "image/png", "data": "aGVsbG8="}]
    });
    let message: Message = serde_json::from_value(input.clone()).expect("image block");
    assert_eq!(message.role, Role::User);
    assert!(!matches!(message.content[0], ContentBlock::Text { .. }));
    assert_eq!(serde_json::to_value(message).expect("serialize"), input);
}

#[test]
fn image_payload_reaches_each_wire_protocol() {
    let request = providers::ChatRequest {
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
    };
    let openai = serde_json::to_value(providers::wire::openai::to_wire_request(&request, false))
        .expect("openai");
    assert_eq!(
        openai["messages"][0]["content"][0]["image_url"]["url"],
        "data:image/png;base64,aGVsbG8="
    );
    let anthropic =
        serde_json::to_value(providers::wire::anthropic::to_wire_request(&request, false))
            .expect("anthropic");
    assert_eq!(
        anthropic["messages"][0]["content"][0]["source"]["data"],
        "aGVsbG8="
    );
    let codex =
        serde_json::to_value(providers::wire::codex::to_wire_request(&request)).expect("codex");
    assert_eq!(
        codex["input"][0]["content"][0]["image_url"],
        "data:image/png;base64,aGVsbG8="
    );
}
