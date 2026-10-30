use super::*;
use providers::{
    ImageGeneration, ImageGenerationError, ImageGenerationRequest, ImageGenerationResponse,
};
type Requests = Arc<Mutex<Vec<(String, ImageGenerationRequest)>>>;
struct ImageClient {
    result: Result<(), ImageGenerationError>,
    requests: Requests,
}
#[async_trait]
impl ProviderClient for ImageClient {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_use: false,
            reasoning: false,
        }
    }
    fn image_generation(&self) -> Option<&dyn ImageGeneration> {
        Some(self)
    }
    async fn send(&self, _: &ProviderAuth, _: &ChatRequest) -> Result<ChatResponse, ProviderError> {
        panic!("image generation must not use chat")
    }
    async fn stream(
        &self,
        _: &ProviderAuth,
        _: &ChatRequest,
    ) -> Result<DeltaStream, ProviderError> {
        panic!("image generation must not use chat")
    }
}
#[async_trait]
impl ImageGeneration for ImageClient {
    fn provider_name(&self) -> &'static str {
        "test"
    }
    async fn generate(
        &self,
        auth: &ProviderAuth,
        request: &ImageGenerationRequest,
    ) -> Result<ImageGenerationResponse, ImageGenerationError> {
        self.requests
            .lock()
            .unwrap()
            .push((auth.api_key.clone(), request.clone()));
        self.result.clone()?;
        Ok(ImageGenerationResponse {
            png: vec![],
            width: 1,
            height: 1,
            background: None,
            usage: Usage::default(),
            request_id: None,
        })
    }
}
fn fixture(errors: Vec<Result<(), ImageGenerationError>>) -> (RoutedModel, Vec<Requests>) {
    let (mut model, _) = super::fallback::fixture(vec![None; errors.len()]);
    model.agents.roles.image_generator.models = vec![config::ChainEntryConfig::route("worker")];
    let recordings = errors
        .into_iter()
        .enumerate()
        .map(|(i, result)| {
            let profile = format!("profile-{i}");
            let requests = Arc::new(Mutex::new(Vec::new()));
            model.providers.get_mut(&profile).unwrap().client = Arc::new(ImageClient {
                result,
                requests: requests.clone(),
            });
            model.provider_configs.insert(
                profile,
                config::ProviderProfileConfig {
                    models: vec![config::ModelEntryConfig {
                        kind: Some(config::ModelKind::ImageGeneration),
                        ..config::ModelEntryConfig::enabled(format!("model-{i}"))
                    }],
                    ..Default::default()
                },
            );
            requests
        })
        .collect();
    (model, recordings)
}

#[tokio::test]
async fn ac5_ac7_image_chain_fallback_includes_404_and_shares_auth_quota_cooldown() {
    for error in [
        ProviderError::Http {
            status: 404,
            body: "secret".into(),
        },
        ProviderError::Http {
            status: 405,
            body: String::new(),
        },
        ProviderError::Http {
            status: 401,
            body: String::new(),
        },
        ProviderError::Http {
            status: 402,
            body: String::new(),
        },
        ProviderError::RateLimited {
            retry_after: Some(std::time::Duration::from_secs(1)),
        },
        ProviderError::Timeout,
        ProviderError::Transport {
            message: "offline".into(),
        },
        ProviderError::Request("connection failed".into()),
        ProviderError::Http {
            status: 503,
            body: String::new(),
        },
    ] {
        let (model, records) = fixture(vec![
            Err(ImageGenerationError::Provider(error.clone())),
            Ok(()),
        ]);
        let invocation = AgentInvocationContext {
            attempts: None,
            run_id: "run-image".into(),
            ..Default::default()
        };
        let generated = model
            .generate_image(
                &invocation,
                ImageGenerationRequest {
                    prompt: "material".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(generated.profile, "profile-1");
        assert_eq!(records[0].lock().unwrap().len(), 1);
        assert_eq!(records[1].lock().unwrap().len(), 1);
        {
            let record = records[1].lock().unwrap();
            assert_eq!(record[0].0, "secret-1");
            assert_eq!(record[0].1.model, "model-1");
            assert_eq!(
                record[0].1.observation.as_ref().unwrap().purpose,
                event_bus::RequestPurpose::ImageGeneration
            );
            assert_eq!(
                record[0].1.observation.as_ref().unwrap().run_id,
                "run-image"
            );
            assert!(record[0].1.usage_sink.is_none());
        }
        model
            .generate_image(&invocation, ImageGenerationRequest::default())
            .await
            .unwrap();
        assert_eq!(
            records[0].lock().unwrap().len(),
            1,
            "subsequent images skip the cooling candidate"
        );
        assert_eq!(records[1].lock().unwrap().len(), 2);
        let route = routing::ResolvedRoute {
            profile: "profile-0".into(),
            model_id: "model-0".into(),
        };
        let shared = matches!(
            &error,
            ProviderError::Http {
                status: 401 | 402,
                ..
            } | ProviderError::RateLimited { .. }
        );
        assert_eq!(
            model.router.is_member_available(&route),
            !shared,
            "image-only failures must not block the same chat route"
        );
        if !shared {
            let now = std::time::Instant::now();
            assert!(model.image_health.is_cooling_down(
                "profile-0",
                "model-0",
                now + std::time::Duration::from_secs(59)
            ));
            assert!(!model.image_health.is_cooling_down(
                "profile-0",
                "model-0",
                now + std::time::Duration::from_secs(60)
            ));
        }
        if matches!(
            error,
            ProviderError::Http {
                status: 401 | 402,
                ..
            } | ProviderError::RateLimited { .. }
        ) {
            assert!(model.health.is_cooling_down(
                "profile-0",
                "model-0",
                std::time::Instant::now()
            ));
        }
    }
}
#[tokio::test]
async fn ac5_rejected_stops_without_fallback_or_cooldown() {
    let (model, records) = fixture(vec![
        Err(ImageGenerationError::Rejected("unsafe content".into())),
        Ok(()),
    ]);
    let error = model
        .generate_image(
            &AgentInvocationContext::default(),
            ImageGenerationRequest::default(),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error, "unsafe content");
    assert_eq!(records[0].lock().unwrap().len(), 1);
    assert!(records[1].lock().unwrap().is_empty());
    assert!(
        !model
            .image_health
            .is_cooling_down("profile-0", "model-0", std::time::Instant::now())
    );
    assert!(
        !model
            .health
            .is_cooling_down("profile-0", "model-0", std::time::Instant::now())
    );
}
#[tokio::test]
async fn ac5_image_quota_retry_after_reaches_shared_health() {
    let (model, records) = fixture(vec![
        Err(ImageGenerationError::Quota {
            retry_after: Some(std::time::Duration::from_secs(17)),
        }),
        Ok(()),
    ]);
    model
        .generate_image(
            &AgentInvocationContext::default(),
            ImageGenerationRequest::default(),
        )
        .await
        .unwrap();
    let now = std::time::Instant::now();
    assert!(model.health.is_cooling_down(
        "profile-0",
        "another-model",
        now + std::time::Duration::from_secs(16)
    ));
    assert!(!model.health.is_cooling_down(
        "profile-0",
        "another-model",
        now + std::time::Duration::from_secs(17)
    ));
    model
        .generate_image(
            &AgentInvocationContext::default(),
            ImageGenerationRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(records[0].lock().unwrap().len(), 1);
}

#[tokio::test]
async fn ac7_image_chain_checks_kind_and_capability_before_sending() {
    let (mut model, records) = fixture(vec![Ok(()), Ok(())]);
    model.provider_configs.get_mut("profile-1").unwrap().models[0].kind =
        Some(config::ModelKind::Chat);
    assert!(
        model
            .generate_image(
                &AgentInvocationContext::default(),
                ImageGenerationRequest::default()
            )
            .await
            .is_err()
    );
    assert!(records.iter().all(|r| r.lock().unwrap().is_empty()));
    model.provider_configs.get_mut("profile-1").unwrap().models[0].kind =
        Some(config::ModelKind::ImageGeneration);
    model.providers.get_mut("profile-1").unwrap().client = Arc::new(StubClient {
        result: Ok(response()),
        requests: Arc::default(),
    });
    assert!(
        model
            .generate_image(
                &AgentInvocationContext::default(),
                ImageGenerationRequest::default()
            )
            .await
            .is_err()
    );
    assert!(records.iter().all(|r| r.lock().unwrap().is_empty()));
}
#[tokio::test]
async fn ac5_codex_404_falls_back_to_openai_http_endpoint() {
    use base64::Engine;
    use providers::provider::codex::{
        CodexClient, CodexConfig,
        tokens::{CodexTokenStore, InMemoryTokenStore, TokenBundle},
    };
    use providers::provider::openai::{OpenAiClient, OpenAiConfig};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/images/generations"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(1, 1)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    Mock::given(method("POST")).and(path("/v1/images/generations")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"data":[{"b64_json":base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())}]}))).expect(1).mount(&server).await;
    let tokens = Arc::new(InMemoryTokenStore::new());
    tokens.save(&TokenBundle {access_token:"session".into(),refresh_token:"refresh".into(),id_token:format!("e30.{}.sig",base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::json!({"exp":u64::MAX,"https://api.openai.com/auth":{"chatgpt_account_id":"account"}}).to_string()))}).unwrap();
    let (mut model, _) = fixture(vec![Ok(()), Ok(())]);
    model.providers.get_mut("profile-0").unwrap().client = Arc::new(
        CodexClient::with_config(
            CodexConfig {
                base_url: format!("{}/backend-api/codex", server.uri()),
                ..Default::default()
            },
            tokens,
        )
        .unwrap(),
    );
    model.providers.get_mut("profile-1").unwrap().client = Arc::new(
        OpenAiClient::new(OpenAiConfig {
            base_url: format!("{}/v1", server.uri()),
            ..Default::default()
        })
        .unwrap(),
    );
    let generated = model
        .generate_image(
            &AgentInvocationContext::default(),
            ImageGenerationRequest {
                prompt: "material".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(generated.profile, "profile-1");
    assert_eq!(generated.response.width, 1);
}

#[tokio::test]
async fn ac6_image_snapshot_keeps_chain_across_live_reload() {
    let (model, records) = fixture(vec![Ok(())]);
    let switchable = SwitchableModel::new(Arc::new(model));
    let snapshot = switchable.invocation_snapshot().unwrap();
    let (mut disabled, _) = fixture(vec![Ok(())]);
    disabled.agents.roles.image_generator.models.clear();
    switchable.replace(Arc::new(disabled));
    assert!(!switchable.image_generation_enabled());
    assert!(snapshot.image_generation_enabled());
    snapshot
        .generate_image(
            &AgentInvocationContext::default(),
            ImageGenerationRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(records[0].lock().unwrap().len(), 1);
}
