use std::{sync::Arc, time::Duration};

use futures_util::StreamExt;
use providers::provider::codex::{CodexClient, CodexConfig, tokens::InMemoryTokenStore};
use providers::provider::openai_compatible::OpenAiCompatibleClient;
use providers::sse::SseFrame;
use providers::wire::openai::OpenAiStreamInterpreter;
use providers::{
    ChatRequest, ContentBlock, ProviderAuth, ProviderClient, ProviderError, StreamEvent,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn request() -> ChatRequest {
    ChatRequest {
        responses_replay: None,
        model: "cli-proxy-api/kimi-k3".into(),
        messages: Vec::new(),
        tools: Vec::new(),
        temperature: None,
        max_tokens: None,
        reasoning_effort: None,
        service_tier: None,
        output_schema: None,
        observation: None,
    }
}

#[test]
fn kimi_reasoning_content_delta_maps_to_reasoning_event() {
    // Given: Kimi's full thinking, distinct from the answer.
    let frame = SseFrame {
        event: None,
        data: r#"{"choices":[{"delta":{"reasoning_content":"Full thought", "content":"Answer"}}]}"#
            .into(),
    };
    // When
    let events = OpenAiStreamInterpreter::new().interpret(&frame).unwrap();
    // Then
    assert!(
        matches!(events.as_slice(), [StreamEvent::ReasoningDelta { text }, StreamEvent::TextDelta { text: answer }] if text == "Full thought" && answer == "Answer")
    );
}

#[test]
fn reasoning_alias_maps_to_reasoning_event() {
    // Given
    let frame = SseFrame {
        data: r#"{"choices":[{"delta":{"reasoning":"Full thought"}}]}"#.into(),
        event: None,
    };
    // When
    let events = OpenAiStreamInterpreter::new().interpret(&frame).unwrap();
    // Then
    assert!(
        matches!(events.as_slice(), [StreamEvent::ReasoningDelta { text }] if text == "Full thought")
    );
}

#[tokio::test]
async fn kimi_full_thinking_survives_chat_completions_stream() {
    // Given
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Full \"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thought\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"Answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        )).expect(1).mount(&server).await;
    let client =
        OpenAiCompatibleClient::new(server.uri(), "kimi", Duration::from_secs(5), None).unwrap();
    // When
    let events = client
        .stream(&ProviderAuth::new("test"), &request())
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    // Then: completion keeps the full thinking for history replay.
    assert!(
        matches!(events.last(), Some(StreamEvent::Completed { response }) if response.message.content.contains(&ContentBlock::Reasoning { text: "Full thought".into() }))
    );
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(body.get("reasoning").is_none());
}

#[tokio::test]
async fn kimi_is_rejected_before_codex_summary_request() {
    // Given: a Kimi model mistakenly assigned to a Codex provider.
    let client =
        CodexClient::with_config(CodexConfig::default(), Arc::new(InMemoryTokenStore::new()))
            .unwrap();
    // When
    let error = client
        .stream(&ProviderAuth::new("unused"), &request())
        .await
        .err()
        .unwrap();
    // Then: actionable failure before OAuth/network, not a summary request.
    assert!(
        matches!(error, ProviderError::Request(detail) if detail.contains("openai-compatible") && detail.contains("kimi-subscription"))
    );
}

#[tokio::test]
async fn b3_kimi_tool_continuation_replays_plain_thinking_but_not_responses_state() {
    use providers::{FinishReason, Message, Role};
    use serde_json::json;
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning\":\"think\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer\",\"tool_calls\":[{\"index\":0,\"id\":\"call1\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n"), "text/event-stream"))
        .expect(2).mount(&server).await;
    let client =
        OpenAiCompatibleClient::new(server.uri(), "kimi", Duration::from_secs(5), None).unwrap();
    let auth = ProviderAuth::new("test");
    let mut input = request();
    let response = client
        .stream(&auth, &input)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .pop()
        .unwrap();
    let StreamEvent::Completed { response } = response else {
        panic!("completion")
    };
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
    assert_eq!(response.usage.input_tokens, 12);
    assert_eq!(
        response.message.content[0],
        ContentBlock::Reasoning {
            text: "think".into()
        }
    );
    assert_eq!(
        response.message.content[1],
        ContentBlock::Text {
            text: "answer".into()
        }
    );
    let mut assistant = response.message;
    let origin = providers::ResolvedResponsesScope::resolve(
        &input,
        providers::ResponsesDialect::OpenAi,
        None,
        "https://example.test/responses",
        providers::ResponsesAccount::Unavailable,
    );
    assistant.content.push(ContentBlock::ResponsesReasoning {item:json!({"type":"reasoning","id":"rs1","summary":[],"encrypted_content":"secret-cipher"}),origin});
    input.messages.push(assistant);
    input.messages.push(Message {role:Role::User,content:vec![serde_json::from_value(json!({"type":"tool_result","tool_call_id":"call1","is_error":false,"content":[{"type":"text","text":"result"}]})).unwrap()]});
    client
        .stream(&auth, &input)
        .await
        .unwrap()
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    let wire: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(wire["messages"][0]["reasoning_content"], "think");
    assert_eq!(wire["messages"][0]["tool_calls"][0]["id"], "call1");
    assert_eq!(wire["messages"][1]["tool_call_id"], "call1");
    assert!(!wire.to_string().contains("secret-cipher"));
    assert!(wire.get("include").is_none());
}
