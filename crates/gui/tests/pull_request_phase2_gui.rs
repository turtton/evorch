//! ADR 0043 Phase 2, PR E: the GUI's auto fix operations and harness cards against the real
//! runtime. A click goes through the window, the chat permit, the command queue, the service and
//! coordinator, the bounded log source, the common outbox and the conversation; the card is
//! read back from the stored notice. Only the pull request facts, the logs and the model are
//! scripted. Every wait is for an event the runtime published.

use event_bus::EventBus;
use gui::model::commands::RecordingSink;
use gui::model::pull_requests::{PullRequestAction, auto_fix_controls};
use gui::model::transcript::TranscriptEntry;
use storage::pull_request_auto_fix::{PrNoticeMode, PrNoticePhase};

#[path = "support/short_private_directory.rs"]
mod short_private_directory;

#[path = "support/pull_request_world.rs"]
mod pull_request_world;

#[path = "support/pull_request_phase2_world.rs"]
mod phase2_world;
use phase2_world::{EXCERPT, THREAD, World, cards_of};

fn send() -> PullRequestAction {
    PullRequestAction::SendToAgent {
        thread_id: THREAD.into(),
    }
}

fn toggle() -> PullRequestAction {
    PullRequestAction::ToggleAutoFix {
        thread_id: THREAD.into(),
    }
}

fn chips(world: &World) -> Vec<String> {
    auto_fix_controls(
        world.window.pull_request_model().linked(THREAD).unwrap(),
        gui::model::pull_requests::now_ns(),
    )
    .unwrap()
    .chips
    .into_iter()
    .map(|chip| chip.text)
    .collect()
}

fn debug_of_entries(world: &World) -> String {
    format!("{:?}", world.window.transcript().entries())
}

// Given: a failing pull request with auto fix off / When: the person clicks "Send to agent",
// twice / Then: one notice is prepared, committed into the conversation once and shown as one
// harness card of its notice ID; the head reads as sent and cannot be sent again; nothing the
// agent reads (the wrapper) is drawn
#[tokio::test]
async fn a_click_on_send_becomes_one_notice_and_one_card_that_cannot_be_sent_twice() {
    let mut world = World::recorded().await;
    let failing = world.failing('a');
    world.link(failing).await;

    // Detection alone shows no card while auto fix is off.
    assert!(world.cards().is_empty());
    assert!(world.notices().is_empty());
    assert!(chips(&world).is_empty());

    world.push_logs('a');
    world.click(send());
    world.click(send());
    assert_eq!(
        world.issued_commands(),
        1,
        "a second click is not a second operation"
    );

    let id = world.delivered().await;
    world.idle().await;
    world.show();

    let notices = world.notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        (
            notices[0].notice_id.as_str(),
            notices[0].phase,
            notices[0].mode
        ),
        (id.as_str(), PrNoticePhase::Adopted, PrNoticeMode::Manual)
    );
    assert_eq!(world.cards(), [(id.clone(), true)]);
    let card = world
        .window
        .transcript()
        .entries()
        .iter()
        .find_map(|entry| match entry {
            TranscriptEntry::HarnessNotice { card, .. } => card.clone(),
            _ => None,
        })
        .unwrap();
    assert_eq!(card.checks, ["ci"]);
    assert_eq!(card.logs[0].excerpt, EXCERPT);
    assert_eq!(card.head_sha, "a".repeat(40));

    let shown = debug_of_entries(&world);
    for forbidden in ["[harness-notice", "<ci-log-data>", "data_not_instructions"] {
        assert!(
            !shown.contains(forbidden),
            "{forbidden:?} in the transcript"
        );
    }

    // Sent: the head reads as sent, the button is off, a click does nothing.
    assert_eq!(chips(&world), ["Sent"]);
    world.click(send());
    assert_eq!(world.issued_commands(), 1);
    world.shutdown();
}

// Given: the same delivery events arriving again (a replay, a second window) / Then: still one
// card for the notice ID
#[tokio::test]
async fn the_same_delivery_seen_again_adds_no_second_card() {
    let mut world = World::recorded().await;
    let failing = world.failing('a');
    world.link(failing).await;
    world.push_logs('a');
    world.click(send());
    let id = world.delivered().await;
    world.idle().await;
    let replay = world.seen.clone();
    world.show();
    assert_eq!(world.cards(), [(id.clone(), true)]);

    world.window.apply_events(replay);
    world.settle();
    assert_eq!(world.cards(), [(id, true)]);
    world.shutdown();
}

// Given: a second window over the same database that does not hold the thread / When: it shows
// the thread and the delivery events / Then: it shows the one card and the sent state, its
// controls are off and nothing it does reaches a runtime
#[tokio::test]
async fn a_window_that_does_not_hold_the_thread_shows_the_card_and_operates_nothing() {
    let mut world = World::recorded().await;
    let failing = world.failing('a');
    world.link(failing).await;
    world.push_logs('a');
    world.click(send());
    let id = world.delivered().await;
    world.idle().await;
    let events = world.history.clone();
    world.show();

    let other_process = std::sync::Arc::new(
        runtime::ownership::OwnerHost::open(
            world.owners.path(),
            Default::default(),
            std::sync::Arc::new(EventBus::new(16)),
        )
        .unwrap(),
    );
    let mut second = pull_request_world::window(
        &world.dir.path().join("phase2.db"),
        world.dir.path(),
        &other_process,
        Box::new(RecordingSink::default()),
        &[THREAD],
    );
    second.apply_events(events);
    for _ in 0..4 {
        second.pump_pull_requests();
    }

    assert_eq!(cards_of(&second), [(id, true)]);
    let view = second.pull_request_model().linked(THREAD).unwrap();
    assert!(!view.tracked_here);
    let controls = auto_fix_controls(view, gui::model::pull_requests::now_ns()).unwrap();
    assert!(!controls.toggle.enabled && !controls.send.enabled);
    second.apply_pull_request_action(toggle());
    second.apply_pull_request_action(send());
    assert!(second.issued().is_empty());
    world.shutdown();
}

// Given: a screen showing head a / When: the pull request moved to head b and the person clicks
// "Send to agent" on the old screen / Then: nothing is sent for either head, the person is told
// to look again, the screen reloads, and a click on the new screen sends head b
#[tokio::test]
async fn a_click_on_an_old_screen_never_sends_the_new_head_silently() {
    let mut world = World::recorded().await;
    let failing = world.failing('a');
    world.link(failing).await;
    let moved = world.failing('b');
    world.move_to(moved).await;
    world.seen.clear();

    world.click(send());
    // The refusal comes back through the command queue; the runner's slow-test limit, not a
    // timeout here, ends a refusal that never comes.
    while pull_request_world::notices(&world.window).is_empty() {
        world.window.poll_command_sink();
        tokio::task::yield_now().await;
    }
    world.settle();

    assert!(
        world.notices().is_empty(),
        "nothing was reserved for either head"
    );
    let told = pull_request_world::notices(&world.window);
    assert!(
        told.iter().any(|text| text.contains("changed")),
        "the person is told the pull request changed: {told:?}"
    );
    let head = world
        .window
        .pull_request_model()
        .linked(THREAD)
        .and_then(|view| view.facts.as_ref())
        .map(|facts| facts.head_sha.clone());
    assert_eq!(head, Some("b".repeat(40)), "the screen was read again");

    world.push_logs('b');
    world.click(send());
    world.delivered().await;
    world.idle().await;
    world.show();
    assert_eq!(world.notices()[0].head_sha, "b".repeat(40));
    world.shutdown();
}

// Given: auto fix switched on from the bar with a failing head the thread's agent did not push /
// Then: nothing is sent and no card appears (a detected failure is not a card)
#[tokio::test]
async fn auto_fix_on_an_external_head_sends_nothing_and_shows_no_card() {
    let mut world = World::recorded().await;
    let failing = world.failing('a');
    world.link(failing).await;

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

    let state = world.service.auto_fix_state(THREAD).await.unwrap();
    assert!(state.enabled);
    assert_eq!(state.destination_thread_id.as_deref(), Some(THREAD));
    assert!(world.notices().is_empty());
    assert!(world.cards().is_empty());
    let shown = chips(&world);
    assert!(shown.contains(&"3 left".to_owned()), "{shown:?}");
    assert!(shown.contains(&"External commit".to_owned()), "{shown:?}");

    // Switching off keeps the state's destination and shows no change in the transcript.
    world.click(toggle());
    world
        .until_pr(|change| matches!(change, event_bus::PullRequestEvent::AutoFixChanged { .. }))
        .await;
    world.show();
    assert!(!world.service.auto_fix_state(THREAD).await.unwrap().enabled);
    world.shutdown();
}

// Given: another thread turned auto fix on for the same pull request / When: this thread's toggle
// is clicked / Then: the move is asked about first; a cancel changes nothing, and a confirm moves
// the destination to this thread through the real service
#[tokio::test]
async fn moving_auto_fix_between_threads_is_confirmed_and_can_be_cancelled() {
    let mut world = World::recorded().await;
    let failing = world.failing('a');
    world.link(failing).await;

    let other = world.owner.start("t2").unwrap();
    world.source.push_ok(
        &pull_request_world::key(phase2_world::NUMBER),
        world.failing('a'),
    );
    world
        .service
        .link(
            "t2",
            &format!(
                "https://github.com/turtton/evorch/pull/{}",
                phase2_world::NUMBER
            ),
            0,
            &other,
        )
        .await
        .unwrap();
    let state = world.service.auto_fix_state("t2").await.unwrap();
    world
        .service
        .set_auto_fix("t2", 1, state.policy_revision, true, &other)
        .await
        .unwrap();
    world.until_pr(|change| {
        matches!(change, event_bus::PullRequestEvent::AutoFixChanged { thread_id, .. } if thread_id == "t2")
    })
    .await;
    world.window.apply_events(std::mem::take(&mut world.seen));
    world.settle();
    // Another thread's change reads this thread again: its toggle now moves auto fix.
    world.click(PullRequestAction::Refresh {
        thread_id: THREAD.into(),
    });
    world.settle();

    world.click(toggle());
    let dialog = world
        .window
        .destination_dialog()
        .cloned()
        .expect("the move is asked about");
    assert_eq!(dialog.previous_thread_id, "t2");
    assert_eq!(world.issued_commands(), 1, "only the refresh was issued");
    world.click(PullRequestAction::CancelAutoFixDestination);
    let state = world.service.auto_fix_state(THREAD).await.unwrap();
    assert_eq!(state.destination_thread_id.as_deref(), Some("t2"));

    world.click(toggle());
    world.click(PullRequestAction::ConfirmAutoFixDestination {
        thread_id: THREAD.into(),
    });
    world.until_pr(|change| {
        matches!(change, event_bus::PullRequestEvent::AutoFixChanged { thread_id, reason: event_bus::PullRequestAutoFixChange::Policy, .. } if thread_id == THREAD)
    })
    .await;
    let state = world.service.auto_fix_state(THREAD).await.unwrap();
    assert!(state.enabled);
    assert_eq!(state.destination_thread_id.as_deref(), Some(THREAD));
    world.shutdown();
}
