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

#[tokio::test]
async fn the_invocations_attempt_log_reaches_the_provider_request_without_touching_the_wire() {
    let (model, requests) = routed_model(Ok(response()), "local-model", None);
    let messages = vec![Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: "Return JSON".into(),
        }],
    }];
    let schema = providers::JsonSchema {
        name: "verdict".into(),
        schema: serde_json::json!({"type":"object"}),
    };
    let log = providers::AttemptLog::default();
    let logged = AgentInvocationContext {
        attempts: Some(log.clone()),
        ..AgentInvocationContext::default()
    };
    model
        .complete_structured(&logged, Role::Worker, &messages, &schema)
        .await
        .unwrap();
    model
        .complete_structured(
            &AgentInvocationContext::default(),
            Role::Worker,
            &messages,
            &schema,
        )
        .await
        .unwrap();

    assert_eq!(
        log.snapshot().len(),
        1,
        "only the logged call reached the log"
    );
    // The log is observation metadata: the two requests are otherwise the same.
    let mut recorded = requests.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2);
    recorded[0].observation.as_mut().unwrap().attempts = None;
    assert_eq!(recorded[0], recorded[1]);
}
