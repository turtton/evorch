//! ADR 0043 Phase 1 (PR6): the escalation review's facts across a Link and an Unlink, and the
//! regression gate for a disabled `[pull_requests]`.
//!
//! Each step waits for what the runtime publishes (a run's end, a request the scripted model
//! received, an event) and never for a duration. Nothing here sleeps.
//!
//! - The review's payload carries the facts while the thread is linked and none after Unlink,
//!   and an approval that was being decided while the link ended is not kept.
//! - With `enabled = false` nothing is fetched, no fact is given to a review, no event is
//!   published and no candidate is offered, while the agent's provider requests stay the bytes
//!   they are without tracking. The Phase 2 features (a notice, a held message, a wake) do not
//!   exist: no model input carries a harness notice and no run starts that nobody asked for.

#[path = "support/mod.rs"]
mod support;

#[path = "support/pull_request_review_world.rs"]
mod review_world;

use std::sync::Arc;

use agents::Role;
use config::PullRequestsConfig;
use event_bus::{
    AgentRunPhase, Event, EventBus, EventKind, EventReceiver, LifecycleEvent, PullRequestEvent,
};
use providers::FinishReason;
use review_world::{
    DELEGATION, DENY, REQUEST, THREAD, answer, failing_answer, merge_call, now_ns,
    provider_run_with, reviews, world, world_with,
};
use runtime::RunConfig;
use runtime::pull_requests::{PullRequestError, ReviewLookup};
use sandbox::DirectSandbox;
use serde_json::json;
use support::{ScriptedModel, text_response, tool_response};
use tools::ToolExecutor;

const APPROVE: &str = r#"{"approve":true,"reason":"requested","risk_level":"medium","authorization_level":"high","procedure_step":"none"}"#;
const SHELL_REVIEW: &str = "Review a shell sandbox permission request.";

/// Everything published before a marker that is published now has been received once the marker
/// comes back, so what the receiver returns is the whole history up to this point. The marker is
/// matched by its source, its code and a token that is unique to this call.
async fn events_up_to_a_marker(bus: &EventBus, receiver: &mut EventReceiver) -> Vec<Event> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let token = format!(
        "marker-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    bus.emit(Event::new(event_bus::DiagnosticEvent {
        source: "pull-request-e2e".into(),
        severity: event_bus::DiagnosticSeverity::Info,
        code: "marker".into(),
        detail: token.clone(),
        run_id: None,
        thread_id: None,
        call_id: None,
    }));
    let mut events = Vec::new();
    loop {
        let event = receiver.recv().await.unwrap();
        if let EventKind::Diagnostic(diagnostic) = &event.kind
            && diagnostic.source == "pull-request-e2e"
            && diagnostic.code == "marker"
            && diagnostic.detail == token
        {
            return events;
        }
        events.push(event);
    }
}

fn executor(bus: &Arc<EventBus>) -> ToolExecutor {
    ToolExecutor::with_standard_tools(Arc::clone(bus), Arc::new(DirectSandbox::new_unchecked()))
}

// Scenario 7a. Given: a linked thread whose agent asks for an escalated shell command twice /
// When: the thread is unlinked between the two reviews / Then: the first review's payload has
// the facts, the second has no `harness_facts` at all, and the agent's input never carries a
// harness notice
#[tokio::test]
async fn a_shell_review_carries_the_facts_only_while_the_thread_is_linked() {
    let model = Arc::new(ScriptedModel::new([
        Ok(text_response(DENY, FinishReason::Stop)),
        Ok(text_response(DENY, FinishReason::Stop)),
    ]));
    model
        .add_keyed(REQUEST, [Ok(text_response("ready", FinishReason::Stop))])
        .await;
    model
        .add_keyed(
            DELEGATION,
            [
                Ok(merge_call("merge")),
                Ok(text_response("stopped", FinishReason::Stop)),
                Ok(merge_call("merge")),
                Ok(text_response("stopped", FinishReason::Stop)),
            ],
        )
        .await;
    let bus = Arc::new(EventBus::new(4096));
    let world = world(model.clone(), executor(&bus), bus);
    world.link(answer('a', now_ns())).await;
    let root = world
        .runtime
        .delegate_chat(
            THREAD,
            Role::Orchestrator,
            REQUEST.into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(world.runtime.wait(root).await, Ok(AgentRunPhase::Done));

    let first = world
        .runtime
        .delegate_background_as_child(root, Role::Worker, DELEGATION, RunConfig::default())
        .expect("registered child");
    assert_eq!(world.runtime.wait(first).await, Ok(AgentRunPhase::Done));

    // When: the thread is unlinked, and the agent asks again.
    let revision = world
        .storage
        .handle()
        .pull_request_thread_state(THREAD)
        .unwrap()
        .lifecycle_revision;
    world
        .service
        .unlink(THREAD, revision, &world.permit())
        .await
        .unwrap();
    let second = world
        .runtime
        .delegate_background_as_child(root, Role::Worker, DELEGATION, RunConfig::default())
        .expect("registered child");
    assert_eq!(world.runtime.wait(second).await, Ok(AgentRunPhase::Done));

    // Then: only the first review was given the facts.
    let reviews = reviews(&model, SHELL_REVIEW).await;
    assert_eq!(reviews.len(), 2);
    assert_eq!(
        reviews[0].1["context"]["harness_facts"]["pull_request"]["head_sha"],
        "a".repeat(40)
    );
    assert!(
        reviews[1].1["context"].get("harness_facts").is_none(),
        "{}",
        reviews[1].1["context"]
    );
    let sent = format!("{:?}", model.observed().await);
    assert!(
        !sent.contains("harness-notice"),
        "Phase 2's notice does not exist in Phase 1"
    );
}

// Scenario 7b. Given: an escalated shell command whose review approves / When: the thread is
// unlinked while that review is out / Then: the approval is not kept, so the command never runs
#[tokio::test]
async fn an_approval_decided_while_the_link_ended_does_not_run_the_command() {
    let workspace = tempfile::tempdir().unwrap();
    let marker = workspace.path().join("ran");
    let release = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [
            Ok(tool_response(
                "touch",
                "shell",
                json!({
                    "command": format!("touch {}", marker.display()),
                    "sandbox_access": "unsandboxed",
                    "justification": "record that the command ran",
                }),
            )),
            Ok(text_response(APPROVE, FinishReason::Stop)),
            Ok(text_response("finished", FinishReason::Stop)),
        ],
        Arc::clone(&release),
    ));
    let bus = Arc::new(EventBus::new(4096));
    let world = world(model.clone(), executor(&bus), bus);
    world.link(answer('a', now_ns())).await;

    let run = world
        .runtime
        .delegate_chat(
            THREAD,
            Role::Worker,
            "Record that this ran.".into(),
            RunConfig::default(),
        )
        .unwrap();
    let (phase, ()) = tokio::join!(world.runtime.wait(run), async {
        model.wait_for_request(0).await;
        release.notify_one();
        let review = model.wait_for_request(1).await;
        assert!(
            format!("{review:?}").contains("harness_facts"),
            "the review in flight was given the facts"
        );
        let revision = world
            .storage
            .handle()
            .pull_request_thread_state(THREAD)
            .unwrap()
            .lifecycle_revision;
        world
            .service
            .unlink(THREAD, revision, &world.permit())
            .await
            .unwrap();
        release.notify_one();
        model.wait_for_request(2).await;
        release.notify_one();
    });

    assert_eq!(phase, Ok(AgentRunPhase::Done));
    assert!(
        !marker.exists(),
        "the command ran on an approval that the unlink outdated"
    );
    let followup = format!("{:?}", model.observed().await[2]);
    assert!(
        followup.contains("review context changed during review"),
        "{followup}"
    );
}

const ECHO_REQUEST: &str = "Print the pull request address.";
const MERGE_REQUEST: &str = "Merge pull request 7 now.";

/// What one pass over the agent, the observer and the review left behind.
struct Flow {
    fetches: usize,
    candidates: usize,
    pull_request_events: usize,
    review_facts: bool,
    lookup: ReviewLookup,
    model_requests: usize,
    notice: bool,
    linking: Result<(), PullRequestError>,
}

/// One agent prints a pull request address through the real shell, then another asks for an
/// escalated `gh pr merge` that the review denies.
async fn flow(settings: PullRequestsConfig) -> Flow {
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        DENY,
        FinishReason::Stop,
    ))]));
    model
        .add_keyed(
            ECHO_REQUEST,
            [
                Ok(tool_response(
                    "echo",
                    "shell",
                    json!({"command": "echo https://github.com/turtton/evorch/pull/7"}),
                )),
                Ok(text_response("printed", FinishReason::Stop)),
            ],
        )
        .await;
    model
        .add_keyed(
            MERGE_REQUEST,
            [
                Ok(merge_call("merge")),
                Ok(text_response("stopped", FinishReason::Stop)),
            ],
        )
        .await;
    let enabled = settings.enabled;
    let bus = Arc::new(EventBus::new(4096));
    let world = world_with(model.clone(), executor(&bus), bus, settings);
    // Observing output is an owned mutation, just like Link. Keep identical
    // authority for the enabled/disabled sides of the cache comparison.
    let _permit = world.permit();
    let mut pull_request_events = 0;

    let echo = world
        .runtime
        .delegate_chat(
            THREAD,
            Role::Worker,
            ECHO_REQUEST.into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(world.runtime.wait(echo).await, Ok(AgentRunPhase::Done));
    if enabled {
        // The observer reads the tool output after the run: wait for what it offers.
        let mut events = world.events.lock().await;
        loop {
            let event = events.recv().await.unwrap();
            if let EventKind::PullRequest(change) = event.kind {
                pull_request_events += 1;
                if matches!(change, PullRequestEvent::CandidateChanged { .. }) {
                    break;
                }
            }
        }
    }
    let linking = world
        .service
        .link(THREAD, "turtton/evorch#7", 0, &world.permit())
        .await
        .map(|_| ());
    if enabled {
        // Linking only to see a disabled service refuse it; undo it for the enabled run.
        let revision = world
            .storage
            .handle()
            .pull_request_thread_state(THREAD)
            .unwrap()
            .lifecycle_revision;
        world
            .service
            .unlink(THREAD, revision, &world.permit())
            .await
            .unwrap();
    }

    let merge = world
        .runtime
        .delegate_chat(
            "thread-pr-merge",
            Role::Worker,
            MERGE_REQUEST.into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(world.runtime.wait(merge).await, Ok(AgentRunPhase::Done));
    let lookup = world
        .service
        .snapshot_for_review(&merge.to_string())
        .await
        .unwrap();

    {
        let mut events = world.events.lock().await;
        pull_request_events += events_up_to_a_marker(&world.bus, &mut events)
            .await
            .iter()
            .filter(|event| matches!(event.kind, EventKind::PullRequest(_)))
            .count();
    }

    let review_payloads = reviews(&model, SHELL_REVIEW).await;
    assert_eq!(review_payloads.len(), 1, "the merge reached the reviewer");
    Flow {
        fetches: world.source.fetches(),
        candidates: storage::Database::open_read_only(&world.db_path())
            .unwrap()
            .pull_request_candidates(THREAD, None)
            .unwrap()
            .len(),
        pull_request_events,
        review_facts: review_payloads[0].1["context"]
            .get("harness_facts")
            .is_some(),
        lookup,
        model_requests: model.observed().await.len(),
        notice: format!("{:?}", model.observed().await).contains("harness-notice"),
        linking,
    }
}

// Scenario 8c. Given: a tracked, linked thread whose agent has finished, so a notice would have
// a finished recipient to be held for and to wake / When: CI fails and the service has handled it
// (the transition is stored and its notification claimed) / Then: nothing is delivered to any
// run, nothing is held, no run is woken or started, no automatic wake is spent, and the agent's
// model input never carried a harness notice. Notices and wakes are Phase 2.
#[tokio::test]
async fn a_ci_failure_on_a_linked_thread_sends_no_notice_holds_nothing_and_wakes_nothing() {
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(REQUEST, [Ok(text_response("ready", FinishReason::Stop))])
        .await;
    let bus = Arc::new(EventBus::new(4096));
    let world = world(model.clone(), executor(&bus), bus);
    world.link(answer('a', now_ns())).await;
    let root = world
        .runtime
        .delegate_chat(
            THREAD,
            Role::Orchestrator,
            REQUEST.into(),
            RunConfig::default(),
        )
        .unwrap();
    assert_eq!(world.runtime.wait(root).await, Ok(AgentRunPhase::Done));
    let mut audit = world.bus.subscribe();
    let (held, wakes) = (
        world.runtime.held_message_count(),
        world.runtime.auto_wake_total(),
    );

    // When: the check fails, and the change is published with its notification claimed.
    world.update_facts(failing_answer('b', now_ns())).await;
    let mut seen = Vec::new();
    loop {
        let event = audit.recv().await.unwrap();
        let handled = matches!(
            &event.kind,
            EventKind::PullRequest(PullRequestEvent::Changed { notifications, .. })
                if notifications.iter().any(|kind| kind == "checks_failed")
        );
        seen.push(event);
        if handled {
            break;
        }
    }
    seen.extend(events_up_to_a_marker(&world.bus, &mut audit).await);

    // Then: the failure was handled once, and nothing reached an agent.
    let failures = seen
        .iter()
        .filter(|event| {
            matches!(
                &event.kind,
                EventKind::PullRequest(PullRequestEvent::Changed { notifications, .. })
                    if notifications.iter().any(|kind| kind == "checks_failed")
            )
        })
        .count();
    assert_eq!(failures, 1);
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event.kind, EventKind::AgentMessage(_))),
        "no message was delivered to any run, held or not"
    );
    assert!(
        !seen.iter().any(|event| matches!(
            &event.kind,
            EventKind::Lifecycle(
                LifecycleEvent::AgentRunStarted { .. }
                    | LifecycleEvent::AgentRunStateChanged { .. }
            )
        )),
        "no run was started or woken"
    );
    assert_eq!(world.runtime.held_message_count(), held);
    assert_eq!(held, 0);
    assert_eq!(world.runtime.auto_wake_total(), wakes);
    assert_eq!(wakes, 0);
    let sent = model.observed().await;
    assert_eq!(sent.len(), 1, "no request was made after the failure");
    assert!(!format!("{sent:?}").contains("harness-notice"));
}

// Scenario 8a. Given: `[pull_requests] enabled = false` / When: an agent prints a pull request
// address and another asks for an escalated merge / Then: nothing is fetched, offered,
// published or given to the review, the link is refused, and no run or notice follows. The
// same flow with tracking on offers the candidate, so the zeros are not vacuous.
#[tokio::test]
async fn a_disabled_service_fetches_offers_publishes_and_supplies_nothing() {
    let off = flow(PullRequestsConfig::default()).await;
    assert_eq!(off.fetches, 0);
    assert_eq!(off.candidates, 0);
    assert_eq!(off.pull_request_events, 0);
    assert!(!off.review_facts);
    assert!(matches!(off.lookup, ReviewLookup::Disabled));
    assert!(matches!(off.linking, Err(PullRequestError::Disabled)));
    assert!(!off.notice);

    let on = flow(PullRequestsConfig {
        enabled: true,
        ..PullRequestsConfig::default()
    })
    .await;
    assert_eq!(on.candidates, 1, "tracking on offers the printed address");
    assert!(on.pull_request_events > 0);
    assert!(!on.review_facts, "an unlinked thread has no facts");
    assert!(matches!(on.lookup, ReviewLookup::NotLinked));
    assert!(!on.notice);

    // Both ran the same two agent runs and one review, and nothing else was asked of a model
    // (no notice-driven run, no wake).
    assert_eq!(off.model_requests, 5);
    assert_eq!(on.model_requests, off.model_requests);
}

// Scenario 8b. Given: the same agent run with tracking off and with tracking on but unlinked /
// Then: the provider sees the same bytes (system, tools, settings, affinity key and every sent
// prefix), the prefix only grows, the input-derived mock cache reuses all of it, and no request
// carries a harness notice
#[tokio::test]
async fn a_disabled_service_leaves_the_agents_wire_prefix_and_cache_untouched() {
    use mock_openai::cache_contract::{CacheProtocol, assert_append_only};

    let (tracking, _, tracking_head) = provider_run_with(
        None,
        PullRequestsConfig {
            enabled: true,
            ..PullRequestsConfig::default()
        },
    )
    .await;
    let (off, usage, off_head) = provider_run_with(None, PullRequestsConfig::default()).await;

    assert_eq!(tracking_head, "");
    assert_eq!(off_head, "");
    assert_eq!(off.len(), 3);
    assert_eq!(
        serde_json::to_string(&off).unwrap(),
        serde_json::to_string(&tracking).unwrap()
    );
    assert!(
        !serde_json::to_string(&off)
            .unwrap()
            .contains("harness-notice")
    );
    for pair in off.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    for pair in usage.windows(2) {
        assert!(pair[0].0 > 0);
        assert_eq!(
            pair[1].1, pair[0].0,
            "the input-derived mock must reuse the full previous prefix"
        );
    }
}
