use std::sync::Mutex;
use std::time::Duration;

use event_bus::{CacheComparison, EventKind, ProviderEvent, RequestPurpose};
use futures_util::FutureExt;
use providers::{ChatRequest, ImageBackground, InlineImage, ProviderAuth, Usage};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, ResponseTemplate};

use super::*;

#[tokio::test]
async fn ac1_only_codex_and_openai_expose_image_generation() {
    use providers::provider::anthropic::{AnthropicClient, AnthropicConfig};
    use providers::provider::openai_compatible::OpenAiCompatibleClient;
    let server = MockServer::start().await;
    for codex in [true, false] {
        let client = client(&server, codex, None, "");
        assert_eq!(
            client.image_generation().unwrap().provider_name(),
            if codex { "openai-codex" } else { "openai" }
        );
    }
    // A compatible provider labelled openai still must not inherit the capability.
    assert!(
        OpenAiCompatibleClient::new(server.uri(), "openai", Duration::from_secs(1), None)
            .unwrap()
            .image_generation()
            .is_none()
    );
    assert!(
        AnthropicClient::new(AnthropicConfig::default())
            .unwrap()
            .image_generation()
            .is_none()
    );
}

async fn requests(codex: bool) {
    for edit in [false, true] {
        let server = MockServer::start().await;
        let bus = Arc::new(EventBus::new(32));
        let mut receiver = bus.subscribe();
        let client = client(&server, codex, Some(bus), "");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let mut request = request();
        request.usage_sink = Some(Arc::new(move |usage| observed.lock().unwrap().push(usage)));
        let mut expected = json!({"prompt":"draw a tree","model":"gpt-image-2"});
        if codex {
            expected["quality"] = json!("auto");
            expected["size"] = json!("auto");
        } else {
            expected["output_format"] = json!("png");
        }
        if edit {
            request.reference_images = (0..5)
                .map(|_| InlineImage {
                    media_type: "image/png".into(),
                    data: base64::engine::general_purpose::STANDARD.encode(png()),
                })
                .collect();
            request.background = Some(ImageBackground::Transparent);
            request.size = Some("1024x1024".into());
            request.quality = Some("high".into());
            expected["background"] = json!("transparent");
            expected["size"] = json!("1024x1024");
            expected["quality"] = json!("high");
            expected["images"] = json!((0..5).map(|_| json!({"image_url":format!("data:image/png;base64,{}",base64::engine::general_purpose::STANDARD.encode(png()))})).collect::<Vec<_>>());
        }
        let mut mock = Mock::given(method("POST"))
            .and(path(image_path(codex, edit)))
            .and(header(
                "authorization",
                if codex {
                    "Bearer secret-session"
                } else {
                    "Bearer secret-key"
                },
            ))
            .and(header("accept", "application/json"))
            .and(header("content-type", "application/json"))
            .and(body_json(expected));
        if codex {
            mock = mock
                .and(header("chatgpt-account-id", "selected-account"))
                .and(header("originator", "codex_cli_rs"))
                .and(header("version", "0.200.0"))
                .and(header("user-agent", "codex_cli_rs/0.200.0"))
                .and(header("OpenAI-Beta", "responses=experimental"));
        }
        mock.respond_with(
            ResponseTemplate::new(200)
                .set_body_json(success())
                .insert_header("x-codex-imagegen-request-id", "img-1"),
        )
        .expect(1)
        .mount(&server)
        .await;
        let response = client
            .image_generation()
            .unwrap()
            .generate(&ProviderAuth::new("secret-key"), &request)
            .await
            .unwrap();
        assert_eq!(response.png, png());
        assert_eq!((response.width, response.height), (2, 3));
        assert_eq!(response.request_id.as_deref(), Some("img-1"));
        assert_eq!(response.background, Some(ImageBackground::Transparent));
        let usage = Usage {
            input_tokens: 42,
            output_tokens: 7,
            ..Default::default()
        };
        assert_eq!(response.usage, usage);
        assert_eq!(*calls.lock().unwrap(), [usage]);
        let (mut usage_events, mut terminals, mut starts) = (0, 0, 0);
        while let Some(event) = receiver.recv().now_or_never() {
            match event.unwrap().kind {
                EventKind::Usage(_) => usage_events += 1,
                EventKind::Provider(ProviderEvent::RequestStarted {
                    purpose, run_id, ..
                }) => {
                    starts += 1;
                    assert_eq!(purpose, Some(RequestPurpose::ImageGeneration));
                    assert_eq!(run_id.as_deref(), Some("image-run"));
                }
                EventKind::Provider(ProviderEvent::RequestCompleted {
                    purpose,
                    run_id,
                    profile,
                    input_tokens,
                    output_tokens,
                    ..
                }) => {
                    terminals += 1;
                    assert_eq!(purpose, Some(RequestPurpose::ImageGeneration));
                    assert_eq!(run_id.as_deref(), Some("image-run"));
                    assert_eq!(profile.as_deref(), Some("selected-profile"));
                    assert_eq!((input_tokens, output_tokens), (42, 7));
                }
                EventKind::Provider(
                    ProviderEvent::RequestFailed { .. } | ProviderEvent::CacheReuseObserved { .. },
                ) => panic!("image call must succeed without affecting cache"),
                _ => {}
            }
        }
        assert_eq!((usage_events, terminals, starts), (1, 1, 1));
        let requests = server.received_requests().await.unwrap();
        if codex {
            assert!(!requests[0].headers["session-id"].is_empty());
            assert_eq!(
                requests[0].headers["thread-id"],
                requests[0].headers["x-client-request-id"]
            );
        } else {
            assert!(!requests[0].headers.contains_key("chatgpt-account-id"));
        }
    }
}

#[tokio::test]
async fn ac2_codex_generation_and_edits_wire_decode_and_usage_once() {
    requests(true).await;
}
#[tokio::test]
async fn ac3_openai_generation_and_edits_wire_decode_and_usage_once() {
    requests(false).await;
}

#[tokio::test]
async fn ac2_codex_normalizes_all_responses_base_url_shapes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(image_path(true, false)))
        .respond_with(ResponseTemplate::new(200).set_body_json(success()))
        .expect(5)
        .mount(&server)
        .await;
    for suffix in [
        "",
        "/",
        "/backend-api/",
        "/backend-api/codex/",
        "/backend-api/codex/responses/",
    ] {
        client(&server, true, None, suffix)
            .image_generation()
            .unwrap()
            .generate(&ProviderAuth::new("unused"), &request())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn ac4_image_generation_preserves_chat_cache_baseline() {
    for codex in [true, false] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(image_path(codex, false)))
            .respond_with(ResponseTemplate::new(200).set_body_json(success()))
            .expect(1)
            .mount(&server)
            .await;
        let responses = if codex {
            "/backend-api/codex/responses"
        } else {
            "/v1/responses"
        };
        Mock::given(method("POST")).and(path(responses)).respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {}\n\n",json!({"type":"response.completed","response":{"usage":{"input_tokens":100,"output_tokens":1,"input_tokens_details":{"cached_tokens":80}}}})),"text/event-stream")).expect(2).mount(&server).await;
        let bus = Arc::new(EventBus::new(32));
        let mut receiver = bus.subscribe();
        let client = client(&server, codex, Some(bus), "");
        let request = request();
        let chat = ChatRequest {
            responses_replay: None,
            model: "chat-model".into(),
            observation: request.observation.clone(),
            messages: vec![providers::Message {
                role: providers::Role::User,
                content: vec![providers::ContentBlock::Text {
                    text: "ordinary chat".into(),
                }],
            }],
            tools: vec![],
            temperature: None,
            max_tokens: None,
            reasoning_effort: None,
            service_tier: None,
            output_schema: None,
        };
        let auth = ProviderAuth::new("secret-key");
        client.send(&auth, &chat).await.unwrap();
        client
            .image_generation()
            .unwrap()
            .generate(&auth, &request)
            .await
            .unwrap();
        client.send(&auth, &chat).await.unwrap();
        let mut comparisons = Vec::new();
        while let Some(event) = receiver.recv().now_or_never() {
            if let EventKind::Provider(ProviderEvent::CacheReuseObserved { comparison, .. }) =
                event.unwrap().kind
            {
                comparisons.push(comparison);
            }
        }
        assert_eq!(comparisons.len(), 2);
        assert!(matches!(
            comparisons[1],
            CacheComparison::Compared {
                previous_cache_tokens: 80,
                ..
            }
        ));
        let requests = server.received_requests().await.unwrap();
        let chat_bodies = requests
            .iter()
            .filter(|r| r.url.path().ends_with("/responses"))
            .map(|r| r.body_json::<Value>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(chat_bodies.len(), 2);
        assert_eq!(
            chat_bodies[0], chat_bodies[1],
            "image generation must not rewrite the wire prefix or cache affinity"
        );
    }
}
