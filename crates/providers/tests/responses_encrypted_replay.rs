//! Real provider HTTP/SSE paths; no external API calls.
#[allow(dead_code)]
#[path = "support/codex.rs"]
mod codex_support;
#[allow(dead_code)]
#[path = "support/codex_contract.rs"]
mod contract_support;

use futures_util::StreamExt;
use providers::provider::codex::{CodexClient, CodexConfig};
use providers::provider::openai::{OpenAiClient, OpenAiConfig};
use providers::{
    ChatRequest, ContentBlock, Message, ProviderAuth, ProviderClient, ProviderError,
    ResponsesReplayContext, ResponsesSpeed, Role, StreamEvent,
};
use serde_json::{Value, json};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn input() -> ChatRequest {
    serde_json::from_value(
        json!({"model":"m","messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}),
    )
    .unwrap()
}
fn reasoning() -> Value {
    json!({"type":"reasoning","id":"rs1","summary":[],"encrypted_content":"final-B","status":"completed","future":{"日本語":"\n"}})
}
fn response() -> ResponseTemplate {
    let mut added = reasoning();
    added["encrypted_content"] = json!("partial-A");
    let values = [
        json!({"type":"response.output_item.added","output_index":0,"item":added}),
        json!({"type":"response.output_item.done","output_index":0,"item":reasoning()}),
        json!({"type":"response.output_text.delta","output_index":1,"delta":"answer"}),
        json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg1","content":[{"type":"output_text","text":"answer"}]}}),
        json!({"type":"response.completed","response":{"usage":{"input_tokens":9,"output_tokens":4}}}),
    ];
    ResponseTemplate::new(200).set_body_raw(
        values
            .iter()
            .map(|v| format!("data: {v}\n\n"))
            .collect::<String>(),
        "text/event-stream",
    )
}

#[tokio::test]
async fn b3_both_providers_send_stream_replay_and_compaction_share_scope_guards() {
    for codex in [false, true] {
        for streaming in [false, true] {
            for effort in [None, Some("high".to_owned())] {
                for tools in [false, true] {
                    let server = MockServer::start().await;
                    codex_support::mount_empty_catalog(&server).await;
                    let endpoint = if codex {
                        "/backend-api/codex/responses"
                    } else {
                        "/responses"
                    };
                    Mock::given(method("POST"))
                        .and(path(endpoint))
                        .respond_with(response())
                        .mount(&server)
                        .await;
                    let client: Box<dyn ProviderClient> = if codex {
                        Box::new(codex_support::client(
                            &server,
                            contract_support::seeded_store(),
                        ))
                    } else {
                        Box::new(
                            OpenAiClient::new(OpenAiConfig {
                                base_url: server.uri(),
                                ..Default::default()
                            })
                            .unwrap()
                            .with_profile("work"),
                        )
                    };
                    let auth = ProviderAuth::new("test-key");
                    let mut request = input();
                    request.reasoning_effort = effort.clone();
                    if tools {
                        request.tools = serde_json::from_value(json!([{"name":"read","description":"read","input_schema":{"type":"object"}}])).unwrap();
                    }
                    request.responses_replay = Some(ResponsesReplayContext {
                        route_model: "m+fast".into(),
                        speed: ResponsesSpeed::Fast,
                        expected_scope: None,
                    });
                    request.service_tier = Some(providers::ServiceTier::Priority);
                    let scope = client
                        .resolve_responses_scope(&auth, &request)
                        .await
                        .unwrap()
                        .unwrap();
                    request.responses_replay.as_mut().unwrap().expected_scope = Some(scope.clone());
                    let completed = if streaming {
                        let events = client
                            .stream(&auth, &request)
                            .await
                            .unwrap()
                            .collect::<Vec<_>>()
                            .await
                            .into_iter()
                            .collect::<Result<Vec<_>, _>>()
                            .unwrap();
                        assert!(
                            events
                                .iter()
                                .all(|e| !format!("{e:?}").contains("partial-A"))
                        );
                        let StreamEvent::Completed { response } =
                            events.into_iter().last().unwrap()
                        else {
                            panic!("completion")
                        };
                        response
                    } else {
                        client.send(&auth, &request).await.unwrap()
                    };
                    assert_eq!(completed.usage.input_tokens, 9);
                    assert_eq!(
                        completed.message.content[0],
                        ContentBlock::ResponsesReasoning {
                            item: reasoning(),
                            origin: scope.clone()
                        }
                    );
                    assert_eq!(
                        completed.message.content[1],
                        ContentBlock::Text {
                            text: "answer".into()
                        }
                    );
                    let restored: Message =
                        serde_json::from_value(serde_json::to_value(completed.message).unwrap())
                            .unwrap();
                    request.messages.push(restored);
                    client.send(&auth, &request).await.unwrap();
                    let requests = codex_support::response_requests(&server).await;
                    for captured in &requests {
                        let body: Value = serde_json::from_slice(&captured.body).unwrap();
                        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
                        assert_eq!(body["store"], false);
                        assert_eq!(body["model"], "m");
                        assert_eq!(body["service_tier"], "priority");
                        if codex {
                            assert_eq!(captured.headers["chatgpt-account-id"], "acc-123");
                        }
                    }
                    let wire: Value = serde_json::from_slice(&requests[1].body).unwrap();
                    assert_eq!(wire["input"][1], reasoning());
                    request
                        .responses_replay
                        .as_mut()
                        .unwrap()
                        .expected_scope
                        .as_mut()
                        .unwrap()
                        .account = providers::ResponsesAccount::Codex {
                        identity: "other-account".into(),
                    };
                    assert!(matches!(
                        client.send(&auth, &request).await,
                        Err(ProviderError::ResponsesScopeChanged { .. })
                    ));
                    if let Some(compactor) = client.compactor() {
                        assert!(matches!(
                            compactor.compact(&request).await,
                            Err(ProviderError::ResponsesScopeChanged { .. })
                        ));
                    }
                    assert_eq!(codex_support::response_requests(&server).await.len(), 2);
                }
            }
        }
    }
}

#[tokio::test]
async fn b3_compaction_keeps_eligible_reasoning_but_preserves_its_old_include_contract() {
    let server = MockServer::start().await;
    codex_support::mount_empty_catalog(&server).await;
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_raw(
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"checkpoint\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":9,\"output_tokens\":4}}}\n\n", "text/event-stream"))
        .expect(1).mount(&server).await;
    let client = codex_support::client(&server, contract_support::seeded_store());
    let mut request = input();
    let scope = client
        .resolve_responses_scope(&ProviderAuth::new(""), &request)
        .await
        .unwrap()
        .unwrap();
    request.responses_replay = Some(ResponsesReplayContext {
        route_model: scope.route_model.clone(),
        speed: scope.speed,
        expected_scope: Some(scope.clone()),
    });
    request.messages.push(Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ResponsesReasoning {
            item: reasoning(),
            origin: scope,
        }],
    });
    let result = client.compactor().unwrap().compact(&request).await.unwrap();
    assert_eq!(result.encrypted_content, "checkpoint");
    let requests = codex_support::response_requests(&server).await;
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["include"], json!([]));
    assert_eq!(body["input"][1], reasoning());
    assert_eq!(body["input"][2]["type"], "compaction_trigger");
}

#[tokio::test]
async fn b3_same_account_new_token_preserves_scope_different_account_does_not() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use providers::provider::codex::tokens::{CodexTokenStore, TokenBundle};
    let server = MockServer::start().await;
    codex_support::mount_empty_catalog(&server).await;
    let store = contract_support::seeded_store();
    let make_client = || {
        CodexClient::with_config(
            CodexConfig {
                base_url: server.uri(),
                ..Default::default()
            },
            store.clone(),
        )
        .unwrap()
    };
    let auth = ProviderAuth::new("");
    let request = input();
    let a = make_client()
        .resolve_responses_scope(&auth, &request)
        .await
        .unwrap()
        .unwrap();
    let mut bundle = store.load().unwrap().unwrap();
    bundle.access_token = "new-access".into();
    bundle.refresh_token = "new-refresh".into();
    store.save(&bundle).unwrap();
    let b = make_client()
        .resolve_responses_scope(&auth, &request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a, b);
    let payload = json!({"exp":4102444800_u64,"https://api.openai.com/auth":{"chatgpt_account_id":"account-B"}});
    store
        .save(&TokenBundle {
            id_token: format!(
                "e30.{}.signature",
                URL_SAFE_NO_PAD.encode(payload.to_string())
            ),
            ..bundle
        })
        .unwrap();
    let client = make_client();
    let c = client
        .resolve_responses_scope(&auth, &request)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(a, c);
    let mut request = request;
    request.responses_replay = Some(ResponsesReplayContext {
        route_model: a.route_model.clone(),
        speed: a.speed,
        expected_scope: Some(a),
    });
    assert!(
        matches!(client.send(&auth,&request).await,Err(ProviderError::ResponsesScopeChanged {actual}) if *actual == c)
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[test]
fn b3_secret_bearing_endpoint_components_cannot_alias_a_replay_scope() {
    for base in [
        "https://user:password@example.test",
        "https://example.test?account=one",
        "https://example.test#fragment",
    ] {
        assert!(
            OpenAiClient::new(OpenAiConfig {
                base_url: base.into(),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            CodexClient::with_config(
                CodexConfig {
                    base_url: base.into(),
                    ..Default::default()
                },
                contract_support::seeded_store()
            )
            .is_err()
        );
    }
}
