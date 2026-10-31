//! A Codex `401` mid-run is recovered by one refresh and a resend of the identical
//! wire body, so the provider prompt-cache prefix survives the rejection.

use std::sync::Arc;

use event_bus::{AgentRunPhase, EventKind, ProviderEvent, ProviderFailureKind};
use mock_openai::cache_contract::assert_append_only;
use mock_openai::codex_auth::{FAR_FUTURE_EXP, token_bundle};
use runtime::{Role, RunConfig};
use serde_json::Value;

use super::{BulkRead, Wire, harness_with_wire, read_response, text_response, through_phase};

// Given: a Codex run whose second request is rejected with 401 and a refresh endpoint
//        issuing a new access token
// When: the run continues after the rejection
// Then: one refresh happens, the rejected body is resent unchanged with the new Bearer,
//       the rejection is recorded as an Auth failure and a separate attempt, the resend
//       and the following turn stay append-only, and cache reads keep covering the
//       previously processed input
#[tokio::test]
async fn codex_401_recovery_resends_the_identical_body_and_keeps_the_cached_prefix() {
    let mut harness = harness_with_wire(
        Wire::Codex,
        vec![read_response(0), read_response(1), text_response("done")],
        1_000_000,
        Arc::new(BulkRead),
        Vec::new(),
        None,
        0.5,
        |_| {},
    );
    harness.mock.set_oauth_token_response(token_bundle(
        "access-new",
        "refresh-new",
        "acc",
        FAR_FUTURE_EXP,
    ));
    harness.mock.reject_responses_unauthorized(1, 1);

    let run = harness.runtime.delegate_background(
        Role::Worker,
        "Read both inputs and finish".into(),
        RunConfig::default(),
    );
    let events = through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );

    let recorded = harness.mock.recorded_requests();
    let responses: Vec<_> = recorded
        .iter()
        .filter(|request| request.path == Wire::Codex.path())
        .collect();
    assert_eq!(
        responses
            .iter()
            .map(|request| request.authorization.as_deref().unwrap_or_default())
            .collect::<Vec<_>>(),
        [
            "Bearer offline-access",
            "Bearer offline-access",
            "Bearer access-new",
            "Bearer access-new"
        ],
        "request 2 is rejected, then resent and followed with the refreshed token"
    );
    assert_eq!(
        recorded
            .iter()
            .filter(|request| request.path.ends_with("/oauth/token"))
            .count(),
        1,
        "exactly one refresh"
    );
    assert_eq!(responses[1].body, responses[2].body);
    let bodies: Vec<&Value> = [0, 2, 3].iter().map(|&i| &responses[i].body).collect();
    let protocol = Wire::Codex.protocol();
    for pair in bodies.windows(2) {
        assert_append_only(protocol, pair[0], pair[1])
            .unwrap_or_else(|error| panic!("the resend destroyed the cached prefix: {error}"));
    }

    let mut attempts = Vec::new();
    for event in &events {
        match &event.kind {
            EventKind::Provider(ProviderEvent::RequestStarted { request_id, .. }) => {
                attempts.push((request_id.clone(), None, None));
            }
            EventKind::Provider(ProviderEvent::RequestFailed {
                request_id,
                failure,
                ..
            }) => {
                let attempt = attempts.last_mut().expect("failure follows its start");
                assert_eq!(&attempt.0, request_id);
                attempt.1 = Some(*failure);
            }
            EventKind::Provider(ProviderEvent::RequestCompleted {
                request_id,
                input_tokens,
                cache_read_tokens,
                ..
            }) => {
                let attempt = attempts.last_mut().expect("completion follows its start");
                assert_eq!(&attempt.0, request_id);
                attempt.2 = Some((*input_tokens, *cache_read_tokens));
            }
            EventKind::Diagnostic(diagnostic) => {
                assert_ne!(diagnostic.code, "CacheRegression", "{diagnostic:?}");
            }
            _ => {}
        }
    }
    assert_eq!(attempts.len(), 4, "the rejection is its own attempt");
    let ids: std::collections::HashSet<_> = attempts.iter().map(|attempt| &attempt.0).collect();
    assert_eq!(ids.len(), 4, "every attempt has a distinct request_id");
    assert_eq!(attempts[1].1, Some(ProviderFailureKind::Auth));
    assert!(attempts[1].2.is_none());
    let completed: Vec<_> = [0, 2, 3]
        .iter()
        .map(|&i| attempts[i].2.expect("accepted attempts complete"))
        .collect();
    assert_eq!(completed[0].1, 0, "the first request is cold");
    for pair in completed.windows(2) {
        assert!(
            pair[1].1 >= pair[0].0,
            "cached={} must cover the previously processed input={}",
            pair[1].1,
            pair[0].0
        );
    }
    assert_eq!(harness.mock.remaining_scripts(), 0);
}
