use super::*;

#[tokio::test]
async fn unsupported_client_uses_existing_prompt_without_schema() {
    // Given: a provider without structured-output support and a prompt-only answer.
    let (model, requests) = routed_model(Ok(response()), "local-model", None);
    let messages = vec![Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "Return JSON".into(),
        }],
    }];
    // When: a schema-constrained completion is requested.
    model
        .complete_structured(
            &AgentInvocationContext::default(),
            Role::Worker,
            &messages,
            &providers::JsonSchema {
                name: "verdict".into(),
                schema: serde_json::json!({"type":"object"}),
            },
        )
        .await
        .unwrap();
    // Then: one legacy attempt preserves the prompt and never exposes tools.
    let recorded = requests.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].output_schema, None);
    assert_eq!(recorded[0].messages, messages);
    assert!(recorded[0].tools.is_empty());
}
