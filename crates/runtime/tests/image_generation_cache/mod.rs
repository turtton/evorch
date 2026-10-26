//! Images append to tool history without changing stable schemas or prior inputs.
use super::*;
use base64::Engine;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn ac12_ac16_generated_preview_preserves_wire_prefix_and_input_derived_cache() {
    for wire in [Wire::ChatCompletions, Wire::Responses] {
        let images = MockServer::start().await;
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(16, 8)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        Mock::given(method("POST")).and(path("/v1/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"b64_json":base64::engine::general_purpose::STANDARD.encode(png.into_inner())}],"usage":{"input_tokens":9,"output_tokens":3}})))
            .expect(1).mount(&images).await;
        let generate = ScriptedResponse::tool_call(
            "image",
            MODEL,
            0,
            "image-call",
            "generate_image",
            [json!({"prompt":"material","output_path":"material.png"}).to_string()],
        );
        let mut harness = harness_with_wire(
            wire,
            vec![
                generate,
                read_response(0),
                read_response(1),
                text_response("done"),
            ],
            1_000_000,
            Arc::new(BulkRead),
            Vec::new(),
            None,
            0.9,
            |config| {
                config.providers.insert(
                    "images".into(),
                    config::ProviderProfileConfig {
                        provider_type: config::ProviderTypeConfig::OpenAi,
                        api_protocol: config::ApiProtocolConfig::OpenAiResponses,
                        base_url: format!("{}/v1", images.uri()),
                        credential: config::CredentialRefConfig::Env {
                            var: KEY_ENV.into(),
                        },
                        models: vec![config::ModelEntryConfig::enabled("gpt-image-2")],
                        default_model: "gpt-image-2".into(),
                        ..Default::default()
                    },
                );
                config.routing.routes.insert(
                    "images".into(),
                    vec![config::RouteCandidateConfig {
                        profile: "images".into(),
                        model: Some("gpt-image-2".into()),
                        auto: false,
                    }],
                );
                config.agents.roles.image_generator.models =
                    vec![config::ChainEntryConfig::route("images")];
            },
        );
        harness
            .runtime
            .set_default_cwd(harness._directory.path().into())
            .unwrap();
        let run = harness.runtime.delegate_background(
            Role::Worker,
            "Create a material, inspect it, and continue.".into(),
            RunConfig {
                conversation: true,
                category: Some("conversation".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            harness.runtime.wait(run).await.unwrap(),
            AgentRunPhase::Done
        );
        let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
        let requests: Vec<_> = harness
            .mock
            .recorded_requests()
            .into_iter()
            .filter(|r| r.path == wire.path())
            .map(|r| r.body)
            .collect();
        assert_eq!(requests.len(), 4);
        for pair in requests.windows(2) {
            assert_append_only(wire.protocol(), &pair[0], &pair[1]).unwrap();
        }
        let serialized = requests[1].to_string();
        assert!(
            serialized.contains("data:image/png;base64,"),
            "preview must survive wire conversion"
        );
        assert!(harness._directory.path().join("material.png").is_file());
        let completed: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Provider(ProviderEvent::RequestCompleted {
                    input_tokens,
                    cache_read_tokens,
                    purpose: Some(event_bus::RequestPurpose::Agent),
                    ..
                }) => Some((*input_tokens, *cache_read_tokens)),
                _ => None,
            })
            .collect();
        assert_eq!(completed.len(), 4);
        for pair in completed.windows(2) {
            assert!(
                pair[1].1 >= pair[0].0,
                "input-derived cache lost prior prefix: {pair:?}"
            );
        }
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    &e.kind,
                    EventKind::Provider(ProviderEvent::RequestCompleted {
                        purpose: Some(event_bus::RequestPurpose::ImageGeneration),
                        ..
                    })
                ))
                .count(),
            1
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(&e.kind,EventKind::Diagnostic(d) if d.code=="CacheRegression"))
        );
    }
}
