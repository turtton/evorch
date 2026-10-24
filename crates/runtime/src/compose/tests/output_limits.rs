use super::*;

async fn metadata_catalog() -> catalog::ModelCatalog {
    let directory = tempfile::tempdir().unwrap();
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        directory.path().join("models-dev.json"),
        serde_json::json!({"fetched_at": fetched_at, "api": {
            "anthropic": {"models": {
                "catalog-model": {"id": "catalog-model", "limit": {"output": 128000}},
                "large-model": {"id": "large-model", "limit": {"output": 4294967296_u64}}
            }}
        }})
        .to_string(),
    )
    .unwrap();
    catalog::ModelCatalog::load_or_refresh(directory.path())
        .await
        .unwrap()
}

pub(super) async fn fixture(model_id: &str) -> (RoutedModel, Arc<Mutex<Vec<ChatRequest>>>) {
    let (mut model, requests) = routed_model(Ok(response()), model_id, None);
    model.agents.worker.base.generation.max_tokens = None;
    model.provider_configs.insert(
        "local".into(),
        config::ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::Anthropic,
            models: vec![config::ModelEntryConfig::enabled(model_id)],
            ..Default::default()
        },
    );
    model
        .metadata_catalog
        .set(Some(metadata_catalog().await))
        .ok()
        .unwrap();
    (model, requests)
}

fn preset(model: &mut RoutedModel, profile: &str, entry: &str, limit: u64) {
    let name = format!("{profile}/{entry}");
    model.model_presets.insert(
        name.clone(),
        config::ModelPresetConfig {
            max_output_tokens: Some(limit),
            ..Default::default()
        },
    );
    model
        .provider_configs
        .get_mut(profile)
        .unwrap()
        .models
        .iter_mut()
        .find(|model| model.id == entry)
        .unwrap()
        .preset = Some(name);
}

#[tokio::test]
async fn explicit_then_preset_then_catalog_resolve_positive_output_limits() {
    for (explicit, preset_limit, expected) in [
        (Some(777), Some(64_000), Some(777)),
        (None, Some(64_000), Some(64_000)),
        (None, Some(0), Some(128_000)),
        (None, None, Some(128_000)),
    ] {
        let (mut model, requests) = fixture("catalog-model").await;
        model.agents.worker.base.generation.max_tokens = explicit;
        if let Some(limit) = preset_limit {
            preset(&mut model, "local", "catalog-model", limit);
        }
        complete(&model, "precedence").await.unwrap();
        assert_eq!(requests.lock().unwrap()[0].max_tokens, expected);
    }
    let (model, requests) = fixture("unknown-model").await;
    complete(&model, "unknown").await.unwrap();
    assert_eq!(requests.lock().unwrap()[0].max_tokens, None);
}

#[tokio::test]
async fn fast_variant_uses_base_catalog_and_exact_preset_before_base_preset() {
    for (exact, expected) in [(false, 128_000), (true, 48_000)] {
        let (mut model, requests) = fixture("catalog-model+fast").await;
        if exact {
            preset(&mut model, "local", "catalog-model+fast", 48_000);
        }
        complete(&model, "fast").await.unwrap();
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded[0].model, "catalog-model");
        assert_eq!(recorded[0].max_tokens, Some(expected));
    }
    let (mut model, requests) = fixture("catalog-model+fast").await;
    model.provider_configs.get_mut("local").unwrap().models =
        vec![config::ModelEntryConfig::enabled("catalog-model")];
    preset(&mut model, "local", "catalog-model", 32_000);
    complete(&model, "base-preset").await.unwrap();
    assert_eq!(requests.lock().unwrap()[0].max_tokens, Some(32_000));
}

#[tokio::test]
async fn automatic_limit_is_only_added_for_protocols_that_send_it() {
    for protocol in [ApiProtocol::OpenAiCodexResponses, ApiProtocol::CursorAgent] {
        let (mut model, requests) = fixture("catalog-model").await;
        model
            .providers
            .get_mut("local")
            .unwrap()
            .profile
            .api_protocol = protocol;
        complete(&model, "unsupported-wire").await.unwrap();
        assert_eq!(requests.lock().unwrap()[0].max_tokens, None);
    }
}

#[tokio::test]
async fn openai_responses_receives_automatic_limit_but_codex_does_not() {
    for (provider_type, config_type, protocol, expected) in [
        (
            ProviderType::OpenAi,
            config::ProviderTypeConfig::OpenAi,
            ApiProtocol::OpenAiResponses,
            Some(128_000),
        ),
        (
            ProviderType::OpenAiCodex,
            config::ProviderTypeConfig::OpenAiCodex,
            ApiProtocol::OpenAiCodexResponses,
            None,
        ),
    ] {
        // Given: a catalog-limited model served by the OpenAI API or by Codex.
        let (mut model, requests) = fixture("catalog-model").await;
        let profile = &mut model.providers.get_mut("local").unwrap().profile;
        profile.provider_type = provider_type;
        profile.api_protocol = protocol;
        model
            .provider_configs
            .get_mut("local")
            .unwrap()
            .provider_type = config_type;
        // When: the request is built without an explicit limit.
        complete(&model, "responses-wire").await.unwrap();
        // Then: only the OpenAI Responses request carries the catalog limit.
        assert_eq!(
            requests.lock().unwrap()[0].max_tokens,
            expected,
            "{provider_type:?} / {protocol:?}"
        );
    }
}

#[tokio::test]
async fn fallback_resolves_each_profile_and_model_output_limit() {
    let (mut model, requests) = super::fallback::fixture(vec![Some(ProviderError::Timeout), None]);
    model.agents.worker.base.generation.max_tokens = None;
    for (profile, model_id, limit) in [
        ("profile-0", "model-0", 16_000),
        ("profile-1", "model-1", 64_000),
    ] {
        model.provider_configs.insert(
            profile.into(),
            config::ProviderProfileConfig {
                models: vec![config::ModelEntryConfig::enabled(model_id)],
                ..Default::default()
            },
        );
        preset(&mut model, profile, model_id, limit);
    }
    complete(&model, "fallback-output").await.unwrap();
    assert_eq!(requests[0].lock().unwrap()[0].max_tokens, Some(16_000));
    assert_eq!(requests[1].lock().unwrap()[0].max_tokens, Some(64_000));
}

#[tokio::test]
async fn preferences_keep_identical_model_ids_in_different_profiles_separate() {
    let (mut model, requests) = fixture("catalog-model").await;
    let original = &model.providers["local"];
    let mut other = ComposedProvider {
        profile: original.profile.clone(),
        client: Arc::clone(&original.client),
        auth: ProviderAuth::new("other-secret"),
    };
    other.profile.name = "other".into();
    model.providers.insert("other".into(), other);
    model
        .provider_configs
        .insert("other".into(), model.provider_configs["local"].clone());
    model.verification = tokio::sync::OnceCell::new_with(Some(BTreeMap::from([
        ("local".into(), Ok(Vec::new())),
        ("other".into(), Ok(Vec::new())),
    ])));
    preset(&mut model, "local", "catalog-model", 12_000);
    preset(&mut model, "other", "catalog-model", 24_000);
    for profile in ["local", "other"] {
        model
            .complete(
                &AgentInvocationContext {
                    model_preference: Some(crate::ModelPreference {
                        profile: profile.into(),
                        model: Some("catalog-model".into()),
                        reasoning_effort: None,
                    }),
                    ..Default::default()
                },
                Role::Worker,
                &[],
                &[],
            )
            .await
            .unwrap();
    }
    let recorded = requests.lock().unwrap();
    assert_eq!(recorded[0].max_tokens, Some(12_000));
    assert_eq!(recorded[1].max_tokens, Some(24_000));
}

#[tokio::test]
async fn category_explicit_output_limit_overrides_model_metadata() {
    let (mut model, requests) = fixture("catalog-model").await;
    model.agents.worker.categories.insert(
        "quick".into(),
        config::CategoryBindingConfig {
            generation: GenerationOverridesConfig {
                max_tokens: Some(1234),
                ..Default::default()
            },
            ..Default::default()
        },
    );
    model
        .complete(
            &AgentInvocationContext {
                category: Some("quick".into()),
                ..Default::default()
            },
            Role::Worker,
            &[],
            &[],
        )
        .await
        .unwrap();
    assert_eq!(requests.lock().unwrap()[0].max_tokens, Some(1234));
}

#[tokio::test]
async fn large_metadata_limit_is_preserved_by_requests_and_rejected_by_benchmark_capture() {
    let (model, requests) = fixture("large-model").await;
    complete(&model, "large-output").await.unwrap();
    assert_eq!(requests.lock().unwrap()[0].max_tokens, Some(4_294_967_296));
    assert!(
        model
            .benchmark_settings(&AgentInvocationContext::default(), Role::Worker, &[])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn metadata_limit_reaches_streaming_structured_and_summary_requests() {
    let (model, requests) = fixture("catalog-model").await;
    model
        .complete_streaming(
            &AgentInvocationContext::default(),
            Role::Worker,
            &[],
            &[],
            &EventBus::new(16),
        )
        .await
        .unwrap();
    model
        .complete_structured(
            &AgentInvocationContext::default(),
            Role::Worker,
            &[],
            &providers::JsonSchema {
                name: "result".into(),
                schema: serde_json::json!({"type": "object"}),
            },
        )
        .await
        .unwrap();
    model
        .complete(
            &AgentInvocationContext {
                purpose: event_bus::RequestPurpose::Compaction,
                ..Default::default()
            },
            Role::Worker,
            &[],
            &[],
        )
        .await
        .unwrap();
    let recorded = requests.lock().unwrap();
    assert_eq!(recorded.len(), 3);
    assert!(
        recorded
            .iter()
            .all(|request| request.max_tokens == Some(128_000))
    );
}

#[tokio::test]
async fn benchmark_freezes_metadata_or_anthropic_default_across_candidate_selection() {
    for (baseline, expected) in [("catalog-model", 128_000), ("unknown-model", 4096)] {
        let (mut model, requests) = fixture(baseline).await;
        model
            .providers
            .get_mut("local")
            .unwrap()
            .profile
            .api_protocol = ApiProtocol::AnthropicMessages;
        model
            .providers
            .get_mut("local")
            .unwrap()
            .profile
            .models
            .push("candidate".into());
        model
            .provider_configs
            .get_mut("local")
            .unwrap()
            .models
            .push(config::ModelEntryConfig::enabled("candidate"));
        preset(&mut model, "local", "candidate", 256_000);
        let model = Arc::new(model);
        let mut captured = model
            .benchmark_settings(&AgentInvocationContext::default(), Role::Worker, &[])
            .await
            .unwrap();
        assert_eq!(captured.generation.max_tokens, Some(expected));
        captured.preference.model = Some("candidate".into());
        let frozen = model.freeze_for_benchmark(captured).unwrap();
        frozen
            .complete(
                &AgentInvocationContext::default(),
                Role::Worker,
                &[],
                &[ToolSpec {
                    name: "read".into(),
                    description: String::new(),
                    input_schema: serde_json::json!({}),
                }],
            )
            .await
            .unwrap();
        assert_eq!(
            requests.lock().unwrap()[0].max_tokens,
            Some(u64::from(expected))
        );
    }
}
