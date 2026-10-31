//! A Codex run keeps one identity on the wire: `session-id`, `thread-id`,
//! `x-client-request-id` and `prompt_cache_key` are the same run-derived UUID in every
//! request (ordinary turns, official compaction and a 401 resend), the legacy
//! `OpenAI-Beta` header is not sent, and the prompt cache keeps being reused under the key.

use std::sync::Arc;

use event_bus::{AgentRunPhase, CompactionReason};
use mock_openai::RecordedRequest;
use mock_openai::cache_contract::assert_append_only;
use mock_openai::codex_auth::{FAR_FUTURE_EXP, token_bundle};
use runtime::{Role, RunConfig};
use serde_json::json;

use super::{
    BulkRead, Wire, compaction_restarts_cache_on_harness, harness_with_wire,
    is_compaction_trigger_request, read_response, text_response, through_phase,
};

/// The identity every request of one run must share, read off the first request.
fn run_identity(requests: &[RecordedRequest]) -> String {
    let id = requests[0]
        .codex_ids
        .session_id
        .clone()
        .expect("the first request carries a session-id");
    // A UUID in the 8-4-4-4-12 form whose version nibble is 5 (name-based, SHA-1).
    assert_eq!(id.len(), 36, "{id}");
    assert_eq!(id.as_bytes()[14], b'5', "{id}");
    id
}

fn assert_one_identity(requests: &[RecordedRequest]) {
    let identity = run_identity(requests);
    for (index, request) in requests.iter().enumerate() {
        let ids = &request.codex_ids;
        for (name, value) in [
            ("session-id", &ids.session_id),
            ("thread-id", &ids.thread_id),
            ("x-client-request-id", &ids.client_request_id),
        ] {
            assert_eq!(value.as_deref(), Some(identity.as_str()), "{index} {name}");
        }
        assert_eq!(request.body["prompt_cache_key"], identity, "{index}");
        assert_eq!(request.openai_beta, None, "{index}");
    }
}

fn responses(harness: &super::Harness) -> Vec<RecordedRequest> {
    harness
        .mock
        .recorded_requests()
        .into_iter()
        .filter(|request| request.path == Wire::Codex.path())
        .collect()
}

// Given: a Codex run whose history is compacted with the official compaction call
// When: the run continues past the boundary (manual, then automatic)
// Then: every request, the compaction call included, carries the same run identity and
//       the ordinary `include`; ordinary turns stay append-only with their settings
//       (cache key included) and the mock cache keeps covering the previous input
#[tokio::test]
async fn codex_compaction_keeps_the_run_identity_include_and_cache_affinity() {
    for reason in [CompactionReason::Manual, CompactionReason::Automatic] {
        let harness = compaction_restarts_cache_on_harness(Wire::Codex, reason).await;
        let requests = responses(&harness);
        assert_one_identity(&requests);
        let (compactions, ordinary): (Vec<_>, Vec<_>) = requests
            .iter()
            .partition(|request| is_compaction_trigger_request(&request.body));
        assert_eq!(compactions.len(), 1, "{reason:?}");
        for request in &requests {
            assert_eq!(
                request.body["include"],
                json!(["reasoning.encrypted_content"])
            );
        }
        // The compaction request differs from the request it extends only by its history.
        let mut settings = compactions[0].body.clone();
        let mut previous = ordinary[0].body.clone();
        for body in [&mut settings, &mut previous] {
            body.as_object_mut().unwrap().remove("input");
        }
        assert_eq!(settings, previous, "{reason:?}");
        // The key is not a cache boundary: the first ordinary pair is still prefix-stable.
        assert_append_only(Wire::Codex.protocol(), &ordinary[0].body, &ordinary[1].body)
            .unwrap_or_else(|error| panic!("{reason:?}: {error}"));
    }
}

// Given: a Codex run whose second request is rejected with 401
// When: the credentials are refreshed and the body is resent
// Then: the rejected attempt, its resend and the following turn carry the same identity
#[tokio::test]
async fn codex_401_resend_keeps_the_run_identity() {
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
    through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
    assert_eq!(
        harness.runtime.wait(run).await.unwrap(),
        AgentRunPhase::Done
    );
    let requests = responses(&harness);
    assert_eq!(requests.len(), 4, "one rejection, one resend");
    assert_one_identity(&requests);
    assert_eq!(requests[1].body, requests[2].body);
}

// Given: two Codex runs on one runtime and mock
// When: both send requests
// Then: each run keeps its own identity, so the key separates runs (the cache is per run)
#[tokio::test]
async fn codex_runs_have_distinct_identities() {
    let mut harness = harness_with_wire(
        Wire::Codex,
        vec![text_response("one"), text_response("two")],
        1_000_000,
        Arc::new(BulkRead),
        Vec::new(),
        None,
        0.5,
        |_| {},
    );
    let mut identities = Vec::new();
    for goal in ["first run", "second run"] {
        let run =
            harness
                .runtime
                .delegate_background(Role::Worker, goal.into(), RunConfig::default());
        through_phase(&mut harness.receiver, run, AgentRunPhase::Done).await;
        harness.runtime.wait(run).await.unwrap();
        identities.push(responses(&harness).last().unwrap().clone());
    }
    assert_one_identity(&identities[..1]);
    assert_one_identity(&identities[1..]);
    assert_ne!(identities[0].codex_ids, identities[1].codex_ids);
}
