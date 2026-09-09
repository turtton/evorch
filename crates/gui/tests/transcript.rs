use event_bus::{Event, FaultEvent, ProviderEvent, ProviderFailureKind, SkillDiagnosticKind};
use gui::model::transcript::{TranscriptEntry, TranscriptModel};
use gui::model::transcript_registry::{TranscriptKey, TranscriptRegistry};

fn failed(failure: ProviderFailureKind, run_id: Option<&str>) -> Event {
    Event::new(ProviderEvent::RequestFailed {
        request_id: "request-1".into(),
        provider: "provider-a".into(),
        profile: None,
        protocol: "openai-chat-completions".into(),
        model: "model-a".into(),
        streaming: true,
        duration_ms: 42,
        failure,
        run_id: run_id.map(str::to_owned),
    })
}

#[test]
fn provider_request_failed_retry_attempt_becomes_notice() {
    // Given: potentially transient attempt failures, not confirmed retry decisions.
    for (failure, label) in [
        (ProviderFailureKind::RateLimited, "rate limited"),
        (ProviderFailureKind::Server, "server error"),
        (ProviderFailureKind::Transport, "transport error"),
        (ProviderFailureKind::Http { status: 500 }, "HTTP 500"),
        (ProviderFailureKind::Http { status: 599 }, "HTTP 599"),
    ] {
        let mut model = TranscriptModel::new();
        // When: an attempt failure is applied.
        model.apply(&failed(failure, Some("run-1")));
        // Then: the classification is visible as a notice.
        assert_eq!(
            model.entries(),
            &[TranscriptEntry::Notice {
                text: format!(
                    "Provider request failed: provider-a/model-a — {label} (42 ms, retrying)"
                ),
            }]
        );
    }
}

#[test]
fn provider_request_failed_final_attempt_becomes_error() {
    // Given: failures classified as definite rather than transient.
    for (failure, label) in [
        (ProviderFailureKind::Auth, "authentication failed"),
        (ProviderFailureKind::Timeout, "timed out"),
        (ProviderFailureKind::InvalidResponse, "invalid response"),
        (ProviderFailureKind::Quota, "quota exceeded"),
        (ProviderFailureKind::Other, "unknown error"),
        (ProviderFailureKind::Http { status: 401 }, "HTTP 401"),
        (ProviderFailureKind::Http { status: 499 }, "HTTP 499"),
        (ProviderFailureKind::Http { status: 600 }, "HTTP 600"),
    ] {
        let mut model = TranscriptModel::new();
        // When: an attempt failure is applied.
        model.apply(&failed(failure, None));
        // Then: the failure is an error without a retry suffix.
        assert_eq!(
            model.entries(),
            &[TranscriptEntry::Error {
                text: format!("Provider request failed: provider-a/model-a — {label} (42 ms)"),
            }]
        );
    }
}

#[test]
fn fault_skill_diagnostic_becomes_error() {
    // Given: every skill diagnostic kind.
    for kind in [
        SkillDiagnosticKind::DiscoveryError,
        SkillDiagnosticKind::ValidationError,
        SkillDiagnosticKind::Shadowed,
    ] {
        let mut registry = TranscriptRegistry::new();
        let event = Event::new(FaultEvent::SkillDiagnostic {
            kind,
            skill: "demo".into(),
            scope: "repo".into(),
            detail: "diagnostic detail".into(),
        });
        // When: the diagnostic is applied.
        registry.apply(&event);
        // Then: only the thread receives a visible error with identifying context.
        assert_eq!(registry.route(&event), vec![TranscriptKey::Thread]);
        assert!(
            matches!(registry.thread().entries(), [TranscriptEntry::Error { text }]
            if text.contains("demo") && text.contains("repo") && text.contains("diagnostic detail"))
        );
        assert_eq!(registry.run_ids().count(), 0);
    }
}

#[test]
fn fault_subscriber_lagged_becomes_notice() {
    // Given: a subscriber lost events.
    let mut registry = TranscriptRegistry::new();
    let event = Event::new(FaultEvent::SubscriberLagged {
        subscriber_id: 7,
        skipped: 12,
    });
    // When: the infrastructure fault is applied.
    registry.apply(&event);
    // Then: only the thread receives a notice identifying the loss.
    assert_eq!(registry.route(&event), vec![TranscriptKey::Thread]);
    assert_eq!(
        registry.thread().entries(),
        &[TranscriptEntry::Notice {
            text: "Subscriber 7 lagged: skipped 12 events".into(),
        }]
    );
    assert_eq!(registry.run_ids().count(), 0);
}

#[test]
fn registry_routes_request_failed_with_run_id_to_thread_and_run() {
    // Given: an attributed provider failure.
    let mut registry = TranscriptRegistry::new();
    let event = failed(ProviderFailureKind::Auth, Some("run-1"));
    // When: routing and applying the event.
    let routes = registry.route(&event);
    registry.apply(&event);
    // Then: both destinations receive exactly the same error.
    assert_eq!(
        routes,
        vec![TranscriptKey::Thread, TranscriptKey::Run("run-1".into())]
    );
    assert_eq!(
        registry.run("run-1").expect("run").entries(),
        registry.thread().entries()
    );
    assert!(matches!(
        registry.thread().entries(),
        [TranscriptEntry::Error { .. }]
    ));
}

#[test]
fn registry_routes_request_failed_without_run_id_to_thread_only() {
    // Given: a run-less provider failure.
    let mut registry = TranscriptRegistry::new();
    let event = failed(ProviderFailureKind::RateLimited, None);
    // When: routing and applying the event.
    let routes = registry.route(&event);
    registry.apply(&event);
    // Then: only the thread receives the notice.
    assert_eq!(routes, vec![TranscriptKey::Thread]);
    assert_eq!(registry.run_ids().count(), 0);
    assert!(matches!(
        registry.thread().entries(),
        [TranscriptEntry::Notice { .. }]
    ));
}
