use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use event_bus::{EventKind, ProviderEvent, RequestPurpose};
use futures_util::FutureExt;
use providers::{ImageGenerationError, InlineImage, ProviderAuth, ProviderError};
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use super::*;

pub(super) async fn fails_without_usage(
    codex: bool,
    template: ResponseTemplate,
) -> ImageGenerationError {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(image_path(codex, false)))
        .respond_with(template)
        .expect(1)
        .mount(&server)
        .await;
    let bus = Arc::new(EventBus::new(32));
    let mut receiver = bus.subscribe();
    let client = client(&server, codex, Some(bus), "");
    let mut request = request();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    request.usage_sink = Some(Arc::new(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
    }));
    let error = client
        .image_generation()
        .unwrap()
        .generate(&ProviderAuth::new("secret-key"), &request)
        .await
        .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let mut terminals = 0;
    while let Some(event) = receiver.recv().now_or_never() {
        let event = event.unwrap();
        let json = serde_json::to_string(&event).unwrap();
        for secret in [
            "secret-key",
            "secret-session",
            "data:image",
            "echoed-prompt",
        ] {
            assert!(!json.contains(secret), "{json}");
        }
        match event.kind {
            EventKind::Usage(_) | EventKind::Provider(ProviderEvent::RequestCompleted { .. }) => {
                panic!("failed attempt must not record usage")
            }
            EventKind::Provider(ProviderEvent::RequestFailed { purpose, .. }) => {
                terminals += 1;
                assert_eq!(purpose, Some(RequestPurpose::ImageGeneration));
            }
            _ => {}
        }
    }
    assert_eq!(terminals, 1);
    let display = format!("{error:?} {error}");
    for secret in [
        "secret-key",
        "secret-session",
        "data:image",
        "echoed-prompt",
    ] {
        assert!(!display.contains(secret), "{display}");
    }
    error
}

#[tokio::test]
async fn ac5_http_errors_classify_auth_quota_rate_limit_unsupported_transient_and_rejection() {
    for codex in [true, false] {
        for (status, code) in [
            (401, ""),
            (403, ""),
            (402, ""),
            (429, "rate_limit_exceeded"),
            (429, "insufficient_quota"),
            (400, "quota_exceeded"),
            (403, "usage_limit_reached"),
            (404, ""),
            (405, ""),
            (500, ""),
            (503, ""),
            (400, "moderation_blocked"),
            (400, "invalid_value"),
            (422, ""),
        ] {
            let error=fails_without_usage(codex,ResponseTemplate::new(status)
                .insert_header("Retry-After","17")
                .set_body_json(json!({"error":{"code":code,"message":"secret-key secret-session data:image echoed-prompt"}}))).await;
            match (status, code, error) {
                (
                    429,
                    "rate_limit_exceeded",
                    ImageGenerationError::Provider(ProviderError::RateLimited { retry_after }),
                ) => assert_eq!(retry_after, Some(Duration::from_secs(17))),
                (402, _, ImageGenerationError::Quota { retry_after })
                | (
                    _,
                    "insufficient_quota" | "quota_exceeded" | "usage_limit_reached",
                    ImageGenerationError::Quota { retry_after },
                ) => assert_eq!(retry_after, Some(Duration::from_secs(17))),
                (400 | 422, _, ImageGenerationError::Rejected(_)) => {}
                (
                    expected,
                    _,
                    ImageGenerationError::Provider(ProviderError::Http { status, .. }),
                ) => assert_eq!(expected, status),
                (_, _, error) => panic!("unexpected error {error:?}"),
            }
        }
    }
}

#[tokio::test]
async fn ac4_invalid_responses_do_not_emit_usage() {
    for codex in [true, false] {
        let mut responses = vec![
            json!({"data":[]}),
            json!({"data":[{"b64_json":"!!!!"}]}),
            json!({"data":[{"b64_json":base64::engine::general_purpose::STANDARD.encode("not a PNG")}]}),
            json!({"data":[{"b64_json":base64::engine::general_purpose::STANDARD.encode(&png()[..24])}]}),
            json!({"data":[{},{}]}),
        ];
        let mut malformed_usage = success();
        malformed_usage["usage"]["input_tokens"] = json!(-1);
        responses.push(malformed_usage);
        let mut extra_image = success();
        let first_image = extra_image["data"][0].clone();
        extra_image["data"]
            .as_array_mut()
            .unwrap()
            .push(first_image);
        responses.push(extra_image);
        for body in responses {
            let error =
                fails_without_usage(codex, ResponseTemplate::new(200).set_body_json(body)).await;
            assert!(
                matches!(
                    error,
                    ImageGenerationError::Provider(ProviderError::InvalidJson { .. })
                ),
                "{error:?}"
            );
        }
        let error = fails_without_usage(
            codex,
            ResponseTemplate::new(200).set_body_string("invalid JSON secret-key"),
        )
        .await;
        assert!(matches!(
            error,
            ImageGenerationError::Provider(ProviderError::InvalidJson { .. })
        ));
    }
}

#[tokio::test]
async fn ac1_rejects_six_references_and_invalid_inline_input_before_sending() {
    for codex in [true, false] {
        let server = MockServer::start().await;
        let client = client(&server, codex, None, "");
        for case in [
            "six",
            "invalid_base64",
            "invalid_media",
            "empty_prompt",
            "empty_model",
        ] {
            let mut request = request();
            match case {
                "six" => {
                    request.reference_images = (0..6)
                        .map(|_| InlineImage {
                            media_type: "image/png".into(),
                            data: "eA==".into(),
                        })
                        .collect()
                }
                "invalid_base64" => {
                    request.reference_images = vec![InlineImage {
                        media_type: "image/png".into(),
                        data: "!!!!".into(),
                    }]
                }
                "invalid_media" => {
                    request.reference_images = vec![InlineImage {
                        media_type: "text/plain".into(),
                        data: "eA==".into(),
                    }]
                }
                "empty_prompt" => request.prompt = " ".into(),
                "empty_model" => request.model.clear(),
                _ => unreachable!(),
            }
            assert!(matches!(
                client
                    .image_generation()
                    .unwrap()
                    .generate(&ProviderAuth::new("secret-key"), &request)
                    .await,
                Err(ImageGenerationError::Rejected(_))
            ));
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn ac4_missing_optional_usage_background_and_context_are_supported() {
    for codex in [true, false] {
        let server = MockServer::start().await;
        let mut body = success();
        body.as_object_mut().unwrap().remove("usage");
        body.as_object_mut().unwrap().remove("background");
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(body)
                    .insert_header("x-request-id", "public-id"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let bus = Arc::new(EventBus::new(16));
        let mut receiver = bus.subscribe();
        let client = client(&server, codex, Some(bus), "");
        let mut request = request();
        request.observation = None;
        let response = client
            .image_generation()
            .unwrap()
            .generate(&ProviderAuth::new("secret-key"), &request)
            .await
            .unwrap();
        assert_eq!(response.usage, providers::Usage::default());
        assert_eq!(response.background, None);
        assert_eq!(response.request_id.as_deref(), Some("public-id"));
        while let Some(event) = receiver.recv().now_or_never() {
            if let EventKind::Provider(ProviderEvent::RequestCompleted {
                run_id, purpose, ..
            }) = event.unwrap().kind
            {
                assert_eq!(run_id, None);
                assert_eq!(purpose, Some(RequestPurpose::ImageGeneration));
            }
        }
    }
}
