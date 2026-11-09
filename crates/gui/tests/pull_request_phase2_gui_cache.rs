//! ADR 0043 Phase 2, PR E, cache contract: the GUI's auto fix operations never change what the
//! next model request carries, and a send the person asked for adds exactly one notice at the
//! end of the conversation, which is not a new request of the user. The window, the command
//! queue, the service, the log source and the outbox are the real ones.
//!
//! Two views of the same flow: the bytes a mock OpenAI server received (with a cache computed
//! from the input) and the input the runtime attested to a recording model.

use mock_openai::cache_contract::{CacheProtocol, assert_append_only};
use serde_json::Value;

use gui::model::pull_requests::PullRequestAction;

#[path = "support/short_private_directory.rs"]
mod short_private_directory;

#[path = "support/pull_request_world.rs"]
mod pull_request_world;

#[path = "support/pull_request_phase2_world.rs"]
mod phase2_world;
use phase2_world::{THREAD, World, text};

const MARKER: &str = "[harness-notice kind=pr_checks_failed";

fn toggle() -> PullRequestAction {
    PullRequestAction::ToggleAutoFix {
        thread_id: THREAD.into(),
    }
}

fn send() -> PullRequestAction {
    PullRequestAction::SendToAgent {
        thread_id: THREAD.into(),
    }
}

fn messages(request: &Value) -> &Vec<Value> {
    request["messages"].as_array().unwrap()
}

// Given: a conversation after its first turn / When: the person works the bar (refresh, toggle on
// and off) and then sends the failure / Then: the operations call no model; the send adds the
// previous answer and one notice and nothing before them changes; a later human turn extends
// that prefix; system, tools, model and cache key never move; every request after the first
// reads from the cache
#[tokio::test]
async fn bar_operations_keep_the_wire_prefix_and_one_send_appends_one_notice() {
    let mut world = World::mocked().await;
    // New settings make the scheduler fetch again, and a fixture that answers once would run out.
    world.source.repeat_last();
    let failing = world.failing('a');
    world.link(failing).await;
    let first = world.requests();
    assert_eq!(first.len(), 1);

    // Operations on the bar are not input. The refresh finds the same facts.
    world.source.push_ok(
        &pull_request_world::key(phase2_world::NUMBER),
        world.failing('a'),
    );
    world.click(PullRequestAction::Refresh {
        thread_id: THREAD.into(),
    });
    world.click(toggle());
    world
        .until_pr(|change| {
            matches!(
                change,
                event_bus::PullRequestEvent::AutoFixChanged {
                    reason: event_bus::PullRequestAutoFixChange::Policy,
                    ..
                }
            )
        })
        .await;
    world.show();
    world.click(toggle());
    world
        .until_pr(|change| {
            matches!(
                change,
                event_bus::PullRequestEvent::AutoFixChanged {
                    reason: event_bus::PullRequestAutoFixChange::Policy,
                    ..
                }
            )
        })
        .await;
    world.show();
    // The global switch turned off and on announces itself to the bar and is not input either.
    for enabled in [false, true] {
        let mut settings = world.service.settings();
        settings.auto_fix_enabled = enabled;
        world.service.update_settings(settings).unwrap();
        world
            .until_pr(|change| {
                matches!(
                    change,
                    event_bus::PullRequestEvent::AutoFixChanged {
                        reason: event_bus::PullRequestAutoFixChange::Settings,
                        ..
                    }
                )
            })
            .await;
        world.show();
    }
    assert_eq!(
        world.requests().len(),
        1,
        "the bar's operations and the global switch called no model"
    );

    // The send: one notice, appended at the end.
    world
        .mock
        .as_ref()
        .unwrap()
        .enqueue(vec![text("handled the failure")]);
    world.push_logs('a');
    world.click(send());
    world.delivered().await;
    world.idle().await;
    let after_send = world.requests();
    assert_eq!(after_send.len(), 2);
    let (old, new) = (messages(&after_send[0]), messages(&after_send[1]));
    assert_eq!(
        &new[..old.len()],
        old.as_slice(),
        "nothing sent before changed"
    );
    assert_eq!(
        new.len(),
        old.len() + 2,
        "the previous answer and the notice, nothing else"
    );
    assert_eq!(new[old.len()]["role"], "assistant");
    let notice = new.last().unwrap()["content"].as_str().unwrap();
    assert_eq!(notice.matches(MARKER).count(), 1, "one wrapper, one notice");
    assert!(notice.contains(phase2_world::EXCERPT.lines().next().unwrap()));

    // A person's turn after it extends the prefix, and the card did not touch the input.
    world.show();
    assert_eq!(world.cards().len(), 1);
    world
        .mock
        .as_ref()
        .unwrap()
        .enqueue(vec![text("ordinary answer")]);
    world
        .runtime
        .send_message(world.run.parse().unwrap(), "ordinary human followup".into())
        .unwrap();
    world.idle().await;
    let requests = world.requests();
    assert_eq!(requests.len(), 3);
    let (old, new) = (messages(&requests[1]), messages(&requests[2]));
    assert_eq!(&new[..old.len()], old.as_slice());
    assert_eq!(new.len(), old.len() + 2);
    assert_eq!(new.last().unwrap()["content"], "ordinary human followup");

    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
        for field in ["tools", "prompt_cache_key", "model"] {
            assert_eq!(pair[0][field], pair[1][field], "{field} moved");
        }
    }
    assert_eq!(world.cached.len(), requests.len());
    assert_eq!(world.cached[0], 0);
    assert!(
        world.cached.iter().skip(1).all(|tokens| *tokens > 0),
        "input-derived cache hits: {:?}",
        world.cached
    );
    world.shutdown();
}

// Given: the same flow with a recording model / Then: the request after the send is attested as
// not coming from a person (a notice is no new request of the user), and the person's later
// turn is the only other one that counts as human
#[tokio::test]
async fn a_send_makes_no_new_user_request_and_only_the_allowed_notice_is_appended() {
    let mut world = World::recorded().await;
    let failing = world.failing('a');
    world.link(failing).await;
    let recorder = world.recorder.clone().unwrap();
    assert_eq!(recorder.requests.lock().unwrap().len(), 1);

    world.push_logs('a');
    world.click(send());
    world.click(send());
    world.delivered().await;
    world.idle().await;

    let requests = recorder.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "one send, one request");
    let (before, after) = (&requests[0], &requests[1]);
    let human = |turn: &Option<providers::RequestTurnContext>| {
        turn.as_ref()
            .is_some_and(|turn| turn.origin.as_deref() == Some("human"))
    };
    assert!(human(&before.1), "the first turn is the person's");
    assert!(
        !human(&after.1),
        "the notice is not a user request: {:?}",
        after.1
    );
    assert_ne!(
        before.1.as_ref().map(|turn| turn.prompt_id.clone()),
        after.1.as_ref().map(|turn| turn.prompt_id.clone()),
    );
    let old = serde_json::to_value(&before.0).unwrap();
    let new = serde_json::to_value(&after.0).unwrap();
    let (old, new) = (old.as_array().unwrap(), new.as_array().unwrap());
    assert_eq!(&new[..old.len()], old.as_slice());
    assert_eq!(
        new.len(),
        old.len() + 2,
        "the reply and the notice, nothing else"
    );
    let appended = serde_json::to_string(&new[old.len() + 1]).unwrap();
    assert_eq!(appended.matches(MARKER).count(), 1);
    world.shutdown();
}
