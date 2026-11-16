//! Effective output settings must survive composition, transport and recovery.
use super::output_limits::{models_catalog, preset};
use super::*;
use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
use providers::provider::claude::{
    ClaudeClient, ClaudeTokenBundle, ClaudeTokenStore, InMemoryClaudeTokenStore,
};
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn fixture(id: &str, subscription: bool) -> (RoutedModel, Arc<Mutex<Vec<ChatRequest>>>) {
    let (mut model, requests) = output_limits::fixture(id).await;
    let profile = &mut model.providers.get_mut("local").unwrap().profile;
    profile.api_protocol = ApiProtocol::AnthropicMessages;
    profile.provider_type = if subscription {
        ProviderType::AnthropicSubscription
    } else {
        ProviderType::Anthropic
    };
    model
        .provider_configs
        .get_mut("local")
        .unwrap()
        .provider_type = if subscription {
        config::ProviderTypeConfig::AnthropicSubscription
    } else {
        config::ProviderTypeConfig::Anthropic
    };
    model.agents.worker.base.generation.temperature = None;
    (model, requests)
}

async fn send_wire(request: &ChatRequest, subscription: bool) -> Result<Value, ProviderError> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "role":"assistant", "content":[{"type":"text", "text":"ok"}],
            "stop_reason":"end_turn", "usage":{"input_tokens":10,"output_tokens":1}
        })))
        .mount(&server)
        .await;
    let config = AnthropicConfig {
        base_url: server.uri(),
        ..Default::default()
    };
    let client: Box<dyn ProviderClient> = if subscription {
        let store = Arc::new(InMemoryClaudeTokenStore::new());
        store
            .save(&ClaudeTokenBundle {
                access_token: "test-access".into(),
                refresh_token: "test-refresh".into(),
                expires_at: u64::MAX,
                account_id: Some("test-account".into()),
                email: None,
                org_id: None,
                org_name: None,
            })
            .unwrap();
        Box::new(ClaudeClient::new(config, store).unwrap())
    } else {
        Box::new(AnthropicClient::new(config).unwrap())
    };
    client
        .send(&ProviderAuth::new("test-api-key"), request)
        .await?;
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1);
    Ok(if subscription {
        mock_openai::claude_contract::claude_model_input(&received[0].body).unwrap()
    } else {
        serde_json::from_slice(&received[0].body).unwrap()
    })
}

#[tokio::test]
async fn precedence_and_wire_limits_match_on_api_key_and_oauth() {
    for subscription in [false, true] {
        for (id, explicit, preset_limit, catalog_limit, expected) in [
            (
                "claude-sonnet-4-5",
                Some(200_000),
                Some(48_000),
                Some(32_000),
                200_000,
            ),
            (
                "claude-sonnet-4-5",
                None,
                Some(48_000),
                Some(32_000),
                48_000,
            ),
            ("claude-sonnet-4-5", None, None, Some(32_000), 32_000),
            ("claude-sonnet-4-5", None, None, None, 64_000),
            ("unknown-model", None, None, None, 4096),
        ] {
            let (mut model, requests) = fixture(id, subscription).await;
            model.agents.worker.base.generation.max_tokens = explicit;
            if let Some(limit) = preset_limit {
                preset(&mut model, "local", id, limit);
            }
            if let Some(limit) = catalog_limit {
                model.publish_model_catalog(models_catalog(&[(id, limit)]).await);
            }
            complete(&model, "wire-limit").await.unwrap();
            let request = requests.lock().unwrap()[0].clone();
            assert_eq!(request.max_tokens, Some(expected));
            assert_eq!(
                model
                    .checkpoint_provider_input("wire-limit")
                    .unwrap()
                    .request
                    .max_tokens,
                Some(expected)
            );
            assert_eq!(
                send_wire(&request, subscription).await.unwrap()["max_tokens"],
                expected
            );
        }
    }
}

#[tokio::test]
async fn normal_compaction_structured_and_benchmark_keep_the_same_effective_limit() {
    for (id, expected) in [
        ("catalog-model", 128_000),
        ("claude-sonnet-4-5", 64_000),
        ("unknown", 4096),
    ] {
        let (model, requests) = fixture(id, false).await;
        let invocation = AgentInvocationContext {
            run_id: "same-paths".into(),
            ..Default::default()
        };
        model
            .complete(&invocation, Role::Worker, &[], &[])
            .await
            .unwrap();
        model
            .complete(
                &AgentInvocationContext {
                    purpose: event_bus::RequestPurpose::Compaction,
                    ..invocation.clone()
                },
                Role::Worker,
                &[],
                &[],
            )
            .await
            .unwrap();
        model
            .complete_structured(
                &invocation,
                Role::Worker,
                &[],
                &providers::JsonSchema {
                    name: "result".into(),
                    schema: json!({"type":"object"}),
                },
            )
            .await
            .unwrap();
        let model = Arc::new(model);
        let settings = model
            .benchmark_settings(&invocation, Role::Worker, &[])
            .await
            .unwrap();
        assert_eq!(
            settings.generation.max_tokens.map(u64::from),
            Some(expected)
        );
        let frozen = model.freeze_for_benchmark(settings).unwrap();
        frozen
            .complete(
                &invocation,
                Role::Worker,
                &[],
                &[ToolSpec {
                    name: "read".into(),
                    description: String::new(),
                    input_schema: json!({}),
                }],
            )
            .await
            .unwrap();
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), 4);
        assert!(
            recorded
                .iter()
                .all(|request| request.max_tokens == Some(expected))
        );
    }
}

#[tokio::test]
async fn thinking_remains_bounded_without_clamping_explicit_output_limits() {
    for subscription in [false, true] {
        for (id, explicit, effort, max, thinking) in [
            (
                "claude-sonnet-4-5",
                None,
                "high",
                64_000,
                json!({"type":"enabled","budget_tokens":16384}),
            ),
            (
                "claude-sonnet-4-5",
                Some(1025),
                "high",
                1025,
                json!({"type":"enabled","budget_tokens":1024}),
            ),
            (
                "claude-sonnet-4-6",
                None,
                "high",
                128_000,
                json!({"type":"adaptive","display":"summarized"}),
            ),
        ] {
            let (mut model, requests) = fixture(id, subscription).await;
            model.agents.worker.base.generation.max_tokens = explicit;
            model.agents.worker.base.models =
                vec![config::ChainEntryConfig::with_effort("worker", effort)];
            complete(&model, "thinking").await.unwrap();
            let request = requests.lock().unwrap()[0].clone();
            let body = send_wire(&request, subscription).await.unwrap();
            assert_eq!(body["max_tokens"], max);
            assert_eq!(body["thinking"], thinking);
        }
        let (mut model, requests) = fixture("claude-sonnet-4-5", subscription).await;
        model.agents.worker.base.models =
            vec![config::ChainEntryConfig::with_effort("worker", "high")];
        for limit in [0, 1024] {
            model.agents.worker.base.generation.max_tokens = Some(limit);
            complete(&model, "invalid-thinking").await.unwrap();
            let request = requests.lock().unwrap().last().unwrap().clone();
            assert_eq!(request.max_tokens, Some(u64::from(limit)));
            let result = send_wire(&request, subscription).await;
            if subscription {
                assert!(matches!(result, Err(ProviderError::Request(_))));
            } else {
                assert!(result.unwrap().get("thinking").is_none());
            }
        }
    }
}

#[tokio::test]
async fn known_and_unknown_defaults_stay_pinned_across_catalog_refresh() {
    for (id, initial) in [("claude-sonnet-4-5", 64_000), ("unknown", 4096)] {
        let (model, requests) = fixture(id, false).await;
        let old = AgentInvocationContext {
            run_id: "old".into(),
            ..Default::default()
        };
        model.admit(&old, Role::Worker).await.unwrap();
        model.publish_model_catalog(models_catalog(&[(id, 32_000)]).await);
        model.complete(&old, Role::Worker, &[], &[]).await.unwrap();
        complete(&model, "old").await.unwrap();
        complete(&model, "new").await.unwrap();
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.max_tokens)
                .collect::<Vec<_>>(),
            [Some(initial), Some(initial), Some(32_000)]
        );
    }
}

#[tokio::test]
async fn recovery_preserves_legacy_none_and_new_saved_limits_and_thinking() {
    for subscription in [false, true] {
        for legacy in [false, true] {
            let id = "claude-sonnet-4-5";
            let (mut original, _) = fixture(id, subscription).await;
            original.agents.worker.base.models =
                vec![config::ChainEntryConfig::with_effort("worker", "high")];
            original.publish_model_catalog(models_catalog(&[(id, 12_000)]).await);
            complete(&original, "recover").await.unwrap();
            let mut saved = original.checkpoint_provider_input("recover").unwrap();
            if legacy {
                saved.request.max_tokens = None;
            }
            let before = send_wire(&saved.request, subscription).await.unwrap();
            let pins = original.route_pins("recover");
            let (mut current, requests) = fixture(id, subscription).await;
            current.agents = original.agents.clone();
            current.metadata_catalog = tokio::sync::OnceCell::new();
            if !legacy {
                current.publish_model_catalog(models_catalog(&[(id, 88_000)]).await);
            }
            let invocation = AgentInvocationContext {
                run_id: "recover".into(),
                ..Default::default()
            };
            current
                .validate_checkpoint_compatibility(&invocation, Role::Worker, &pins, Some(&saved))
                .unwrap();
            assert!(current.metadata_catalog.get().is_none());
            assert!(current.recovery_output_limits.lock().unwrap().is_empty());
            assert_eq!(
                current.output_limit_state().is_pinned_to_current("recover"),
                None
            );
            current.adopt_recovery_checkpoint("recover", &pins, Some(&saved));
            complete(&current, "recover").await.unwrap();
            let request = requests.lock().unwrap()[0].clone();
            assert_eq!(request.max_tokens, Some(if legacy { 4096 } else { 12_000 }));
            let after = send_wire(&request, subscription).await.unwrap();
            assert_eq!(before["max_tokens"], after["max_tokens"]);
            assert_eq!(before["thinking"], after["thinking"]);
            mock_openai::cache_contract::assert_append_only(
                mock_openai::cache_contract::CacheProtocol::Anthropic,
                &before,
                &after,
            )
            .unwrap();
            assert_eq!(
                current
                    .checkpoint_provider_input("recover")
                    .unwrap()
                    .request
                    .max_tokens,
                request.max_tokens
            );
            current.agents.worker.base.generation.max_tokens = Some(322);
            assert!(
                current
                    .validate_checkpoint_compatibility(
                        &invocation,
                        Role::Worker,
                        &pins,
                        Some(&saved)
                    )
                    .is_err()
            );
        }
    }
}
