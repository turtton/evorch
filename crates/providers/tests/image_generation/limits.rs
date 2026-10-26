use std::time::Duration;

use event_bus::{EventKind, ProviderEvent};
use futures_util::FutureExt;
use providers::{ImageGenerationError, ProviderAuth, ProviderError};
use wiremock::matchers::method;
use wiremock::{Mock, ResponseTemplate};

use super::*;

#[tokio::test]
async fn ac4_response_over_48_mib_is_rejected_without_usage() {
    for codex in [true, false] {
        let error = super::errors::fails_without_usage(
            codex,
            ResponseTemplate::new(200).set_body_bytes(vec![b' '; 48 * 1024 * 1024 + 1]),
        )
        .await;
        assert!(
            matches!(error,ImageGenerationError::Provider(ProviderError::InvalidJson {detail}) if detail.contains("48 MiB"))
        );
    }
}

#[tokio::test]
async fn ac4_five_minute_deadline_and_cancellation_do_not_record_usage() {
    for codex in [true, false] {
        for cancel in [true, false] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(success())
                        .set_delay(Duration::from_secs(301)),
                )
                .expect(1)
                .mount(&server)
                .await;
            let bus = Arc::new(EventBus::new(32));
            let mut receiver = bus.subscribe();
            let client = client(&server, codex, Some(bus), "");
            let handle = tokio::spawn(async move {
                client
                    .image_generation()
                    .unwrap()
                    .generate(&ProviderAuth::new("secret-key"), &request())
                    .await
            });
            // Wait until the real socket has delivered the request before moving time.
            tokio::time::timeout(Duration::from_secs(5), async {
                while server.received_requests().await.unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if cancel {
                handle.abort();
                assert!(handle.await.unwrap_err().is_cancelled());
            } else {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(299)).await;
                assert!(
                    !handle.is_finished(),
                    "must allow more than the ordinary chat timeout"
                );
                tokio::time::advance(Duration::from_secs(1)).await;
                assert!(matches!(
                    handle.await.unwrap(),
                    Err(ImageGenerationError::Provider(ProviderError::Timeout))
                ));
                tokio::time::resume();
            }
            let mut failures = 0;
            while let Some(event) = receiver.recv().now_or_never() {
                match event.unwrap().kind {
                    EventKind::Usage(_)
                    | EventKind::Provider(ProviderEvent::RequestCompleted { .. }) => {
                        panic!("timeout/cancellation must not record usage")
                    }
                    EventKind::Provider(ProviderEvent::RequestFailed { .. }) => failures += 1,
                    _ => {}
                }
            }
            assert_eq!(failures, 1);
        }
    }
}
