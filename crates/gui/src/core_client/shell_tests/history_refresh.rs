//! First history pages belong to a projection, not to a one-off selection event.
use super::*;
use egui_kittest::kittest::NodeT;

fn first_pages(actions: &[Action], id: &str) -> usize {
    actions
        .iter()
        .filter(|action| {
            matches!(action,
        Action::History(thread, None, false) if thread.as_str() == id)
        })
        .count()
}

fn new_session(screen: &Screen) {
    let mut snapshot = populated().cache;
    snapshot.history.clear();
    snapshot.history_not_asked("thr-1");
    snapshot.older.clear();
    screen.shared.lock().unwrap().begin_session(
        snapshot,
        Capability::IMPLEMENTED.to_vec(),
        &[],
        Vec::new(),
    );
}

#[test]
fn reconnect_and_resync_read_the_selected_history_once_and_restore_the_conversation() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.shared.lock().unwrap().drafts.insert(
        "thr-1".into(),
        crate::core_client::Draft {
            text: "keep this unsent draft".into(),
            revision: 1,
        },
    );
    screen.settle();
    for _ in 0..2 {
        screen.shared.lock().unwrap().connected = false;
        new_session(&screen); // Both reconnect and in-session resync replace the cut.
        // A committed event delivered around the snapshot must not suppress
        // reading the earlier messages that the projection never contains.
        screen.shared.lock().unwrap().cache.history.insert(
            "thr-1".into(),
            vec![message(2, "assistant", "New live event")],
        );
        let asked = screen.settle();
        assert_eq!(first_pages(&asked, "thr-1"), 1);
        assert_eq!(first_pages(&screen.settle(), "thr-1"), 0);
        screen.shared.lock().unwrap().cache.history_page(
            "thr-1",
            Page {
                items: vec![message(
                    1,
                    "assistant",
                    "Recovered conversation from the core",
                )],
                next_cursor: None,
                snapshot_revision: Revision(1),
            },
            false,
        );
        screen.settle();
        screen
            .harness
            .get_by_label("Recovered conversation from the core");
        assert_eq!(
            screen.harness.state().selected.as_ref().unwrap().as_str(),
            "thr-1"
        );
        assert_eq!(
            screen.shared.lock().unwrap().drafts["thr-1"].text,
            "keep this unsent draft"
        );
    }
}

#[test]
fn a_first_page_asked_for_while_offline_is_read_after_the_new_projection() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    let tx = screen.harness.state().actions.clone();
    {
        let mut shared = screen.shared.lock().unwrap();
        shared.connected = false;
        crate::core_client::enqueue(
            &tx,
            &mut shared,
            Action::History(ThreadId::new("thr-1").unwrap(), None, false),
        );
    }
    assert_eq!(first_pages(&screen.settle(), "thr-1"), 0);
    new_session(&screen);
    assert_eq!(first_pages(&screen.settle(), "thr-1"), 1);
    assert_eq!(first_pages(&screen.settle(), "thr-1"), 0);
}

#[test]
fn a_full_queue_does_not_lose_the_selected_first_page_read() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    new_session(&screen);
    let tx = screen.harness.state().actions.clone();
    while tx.try_send(Action::Reconnect).is_ok() {}
    screen.harness.run_steps(1); // The automatic first page meets the full queue.
    assert!(screen.shared.lock().unwrap().cache.want_history("thr-1"));
    while screen.actions.try_recv().is_ok() {}
    assert_eq!(first_pages(&screen.settle(), "thr-1"), 1);
    assert_eq!(first_pages(&screen.settle(), "thr-1"), 0);
}

#[test]
fn a_failed_first_page_offers_retry_and_a_new_projection_retries_it_once() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    new_session(&screen);
    assert_eq!(first_pages(&screen.settle(), "thr-1"), 1);
    screen.shared.lock().unwrap().cache.history_failed("thr-1");
    assert!(
        screen
            .shared
            .lock()
            .unwrap()
            .cache
            .history_read_failed("thr-1")
    );
    assert_eq!(
        first_pages(&screen.settle(), "thr-1"),
        0,
        "a failure is not a hot retry loop"
    );

    // The failure replaces the empty-conversation text, and Retry history is the
    // only way back besides a new projection.
    screen
        .harness
        .get_by_label("Could not read this conversation's history");
    assert!(
        screen
            .harness
            .query_by_label("Start the conversation")
            .is_none()
    );
    assert!(screen.harness.query_by_label("Refresh history").is_none());
    screen.harness.get_by_label("Retry history").click();
    assert_eq!(first_pages(&screen.settle(), "thr-1"), 1);
    screen.harness.run_steps(2);
    // The retry is out: it is still told from an empty conversation, and it cannot be
    // sent twice.
    {
        let shared = screen.shared.lock().unwrap();
        assert!(shared.cache.history_read_failed("thr-1"));
        assert!(shared.cache.history_loading("thr-1"));
    }
    screen
        .harness
        .get_by_label("Could not read this conversation's history");
    assert!(
        screen
            .harness
            .query_by_label("Start the conversation")
            .is_none()
    );
    assert!(
        screen
            .harness
            .get_by_label("Retry history")
            .accesskit_node()
            .is_disabled()
    );
    screen.harness.get_by_label("Retry history").click();
    assert_eq!(first_pages(&screen.settle(), "thr-1"), 0);

    // A retry that fails again can be tried once more.
    screen.shared.lock().unwrap().cache.history_failed("thr-1");
    screen.harness.run_steps(2);
    assert!(
        !screen
            .harness
            .get_by_label("Retry history")
            .accesskit_node()
            .is_disabled()
    );
    screen.harness.get_by_label("Retry history").click();
    assert_eq!(first_pages(&screen.settle(), "thr-1"), 1);

    screen.shared.lock().unwrap().cache.history_page(
        "thr-1",
        Page {
            items: vec![message(
                1,
                "assistant",
                "History recovered after a failed read",
            )],
            next_cursor: None,
            snapshot_revision: Revision(1),
        },
        false,
    );
    screen.settle();
    screen
        .harness
        .get_by_label("History recovered after a failed read");
    assert!(screen.harness.query_by_label("Retry history").is_none());
    {
        let shared = screen.shared.lock().unwrap();
        assert!(!shared.cache.history_read_failed("thr-1"));
        assert!(!shared.cache.history_loading("thr-1"));
    }

    screen.shared.lock().unwrap().cache.history_failed("thr-1");
    new_session(&screen);
    assert!(
        !screen
            .shared
            .lock()
            .unwrap()
            .cache
            .history_read_failed("thr-1")
    );
    assert_eq!(first_pages(&screen.settle(), "thr-1"), 1);
    assert_eq!(first_pages(&screen.settle(), "thr-1"), 0);
}
