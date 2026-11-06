use workspace_ui::{LineageKind, ProjectId, SidebarState, ThreadId, UiSettings};

use super::WorkbenchState;
use crate::fixture::DemoSource;
use crate::model::commands::{PullRequestDerivation, RecordingSink, WorkbenchCommand};

fn state(directory: &std::path::Path) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", directory)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("parent"), project, "Parent")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("parent")).unwrap();
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_command_sink(Box::new(RecordingSink::default()))
}

fn inherits(state: &WorkbenchState<DemoSource>) -> Vec<(String, String, PullRequestDerivation)> {
    state
        .issued()
        .iter()
        .filter_map(|command| match command {
            WorkbenchCommand::InheritPullRequest {
                parent_thread_id,
                child_thread_id,
                kind,
            } => Some((parent_thread_id.clone(), child_thread_id.clone(), *kind)),
            _ => None,
        })
        .collect()
}

// Given: a thread that never ran / When: it is forked and rewound / Then: each derived thread is
// announced to the pull request service with how it was derived, where it is created
#[test]
fn a_fork_and_a_rewind_announce_their_inheritance_before_any_run() {
    let directory = tempfile::tempdir().unwrap();
    let mut state = state(directory.path());

    let fork = state.fork_thread(ThreadId::new("parent")).unwrap();
    let rewind = state
        .branch_thread(&ThreadId::new("parent"), None, LineageKind::Rewind)
        .unwrap();

    assert_eq!(
        inherits(&state),
        [
            (
                "parent".to_owned(),
                fork.to_string(),
                PullRequestDerivation::Fork
            ),
            (
                "parent".to_owned(),
                rewind.to_string(),
                PullRequestDerivation::Rewind
            ),
        ]
    );
}

// Given: a conversation another process branched / When: this window imports it / Then: the
// import is display only and announces nothing
#[test]
fn an_imported_branch_announces_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let mut state = state(directory.path());
    let fork = state.fork_thread(ThreadId::new("parent")).unwrap();
    let before = inherits(&state).len();

    state.seed_imported_branch(&fork.to_string());

    assert_eq!(inherits(&state).len(), before);
}

// Given: a thread this window attached read-only / When: any pull request operation is asked for,
// Ignore included / Then: none reaches the sink, and the person is told why
#[test]
fn a_thread_attached_read_only_refuses_every_operation() {
    use crate::model::pull_requests::PullRequestAction;
    let directory = tempfile::tempdir().unwrap();
    let mut state = state(directory.path());
    state.readonly_threads.insert("parent".to_owned());

    for action in [
        PullRequestAction::Refresh {
            thread_id: "parent".into(),
        },
        PullRequestAction::Confirm {
            thread_id: "parent".into(),
            candidate_id: 1,
        },
        PullRequestAction::Dismiss {
            thread_id: "parent".into(),
            candidate_id: 1,
        },
    ] {
        state.apply_pull_request_action(action);
    }

    assert!(
        state.issued().is_empty(),
        "an operation got past the read-only attach: {:?}",
        state.issued()
    );
    let refused = state
        .transcript()
        .entries()
        .iter()
        .filter(|entry| {
            matches!(entry, crate::model::transcript::TranscriptEntry::Notice { text }
                if text == "Read-only attach: explicitly Start or Claim before mutating.")
        })
        .count();
    assert_eq!(refused, 3);
}

// --- auto fix operations (ADR 0043 Phase 2, PR E) ---

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use event_bus::{
    Event, LifecycleEvent, PullRequestAutoFixChange, PullRequestEvent, PullRequestRef,
};
use storage::pull_request_auto_fix::{PrAutoFixState, PrNoticeMode, PrNoticePhase};

use super::pull_requests::PullRequestReader;
use crate::model::commands::LoopEvent;
use crate::model::harness_card::HarnessCard;
use crate::model::notifications::NotificationKind;
use crate::model::pull_requests::PullRequestAction;
use crate::model::pull_requests::ThreadPullRequest;
use crate::model::pull_requests::tests::{SHA_A, auto_state, failing, notice, on_here, with_auto};

const THREAD: &str = "parent";

#[derive(Default)]
struct Reader {
    view: Mutex<Option<ThreadPullRequest>>,
    cards: Mutex<BTreeMap<String, HarnessCard>>,
    card_reads: Mutex<Vec<String>>,
}

impl PullRequestReader for Reader {
    fn load(&self, _: &str) -> Result<ThreadPullRequest, String> {
        self.view
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "none".into())
    }

    fn linked_threads(&self) -> Result<Vec<String>, String> {
        Ok(vec![THREAD.into()])
    }

    fn harness_card(&self, notice_id: &str) -> Result<Option<HarnessCard>, String> {
        self.card_reads.lock().unwrap().push(notice_id.into());
        Ok(self.cards.lock().unwrap().get(notice_id).cloned())
    }
}

fn shown(
    state: PrAutoFixState,
    shape: impl FnOnce(&mut crate::model::pull_requests::AutoFixView),
) -> ThreadPullRequest {
    let mut shown = with_auto(failing(), state, shape);
    shown.thread_id = THREAD.into();
    if let Some(link) = &mut shown.link {
        link.thread_id = THREAD.into();
    }
    shown
}

fn window(
    directory: &std::path::Path,
    shown: ThreadPullRequest,
) -> (WorkbenchState<DemoSource>, Arc<Reader>) {
    let reader = Arc::new(Reader::default());
    *reader.view.lock().unwrap() = Some(shown);
    let mut window = state(directory)
        .with_pull_requests(reader.clone(), None)
        .with_pull_request_spawner(Arc::new(|job| job()));
    for _ in 0..3 {
        window.pump_pull_requests();
    }
    (window, reader)
}

fn toggle() -> PullRequestAction {
    PullRequestAction::ToggleAutoFix {
        thread_id: THREAD.into(),
    }
}

fn adopted(automatic: bool, limit_epoch: Option<u64>) -> Event {
    Event::new(PullRequestEvent::AutoFixChanged {
        thread_id: THREAD.into(),
        link_revision: 1,
        transition_seq: 1,
        pull_request: PullRequestRef {
            provider: "github".into(),
            host: "github.com".into(),
            repository: "turtton/evorch".into(),
            number: 12,
        },
        reason: PullRequestAutoFixChange::Adopted {
            automatic,
            limit_epoch,
        },
    })
}

fn limit_notifications(window: &WorkbenchState<DemoSource>) -> usize {
    window
        .notifications()
        .items()
        .filter(|item| matches!(item.kind, NotificationKind::PullRequestAutoFixLimit { .. }))
        .count()
}

// Given: a failing pull request with auto fix off / When: the toggle is clicked twice before the
// runtime answered / Then: one operation named the revisions the person saw is issued; once the
// runtime answered, the same state can be operated again
#[test]
fn the_toggle_issues_one_operation_per_state_the_person_saw() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, _) = window(directory.path(), shown(auto_state(|_| {}), |_| {}));

    window.apply_pull_request_action(toggle());
    window.apply_pull_request_action(toggle());

    let set = |enabled| WorkbenchCommand::SetPullRequestAutoFix {
        thread_id: THREAD.into(),
        link_revision: 1,
        policy_revision: 4,
        enabled,
    };
    assert_eq!(window.issued(), [set(true)]);
    window.apply_events([adopted_policy()]);
    window.apply_pull_request_action(toggle());
    assert_eq!(window.issued(), [set(true), set(true)]);
}

fn adopted_policy() -> Event {
    Event::new(PullRequestEvent::AutoFixChanged {
        thread_id: THREAD.into(),
        link_revision: 1,
        transition_seq: 1,
        pull_request: PullRequestRef {
            provider: "github".into(),
            host: "github.com".into(),
            repository: "turtton/evorch".into(),
            number: 12,
        },
        reason: PullRequestAutoFixChange::Policy,
    })
}

// Given: auto fix on for this thread / When: the toggle is clicked / Then: it is switched off
#[test]
fn the_toggle_of_the_thread_that_has_auto_fix_switches_it_off() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, _) = window(directory.path(), shown(auto_state(on_here_state), |_| {}));
    window.apply_pull_request_action(toggle());
    assert_eq!(
        window.issued(),
        [WorkbenchCommand::SetPullRequestAutoFix {
            thread_id: THREAD.into(),
            link_revision: 1,
            policy_revision: 4,
            enabled: false,
        }]
    );
}

fn on_here_state(state: &mut PrAutoFixState) {
    on_here(state);
    state.destination_thread_id = Some(THREAD.into());
}

fn on_elsewhere(state: &mut PrAutoFixState) {
    state.enabled = true;
    state.destination_thread_id = Some("other".into());
    state.link_revision = Some(1);
}

// Given: another thread has auto fix on / When: the toggle is clicked / Then: nothing is issued
// until the move is confirmed, a cancel issues nothing, and a confirm names the previous thread
#[test]
fn moving_auto_fix_to_this_thread_is_confirmed_first_and_can_be_cancelled() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, _) = window(directory.path(), shown(auto_state(on_elsewhere), |_| {}));

    window.apply_pull_request_action(toggle());
    let dialog = window
        .destination_dialog()
        .cloned()
        .expect("the move is asked about");
    assert_eq!(dialog.previous_thread_id, "other");
    assert!(window.issued().is_empty());

    window.apply_pull_request_action(PullRequestAction::CancelAutoFixDestination);
    assert!(window.destination_dialog().is_none() && window.issued().is_empty());

    window.apply_pull_request_action(toggle());
    window.apply_pull_request_action(PullRequestAction::ConfirmAutoFixDestination {
        thread_id: THREAD.into(),
    });
    assert!(window.destination_dialog().is_none());
    assert_eq!(
        window.issued(),
        [WorkbenchCommand::ChangePullRequestAutoFixDestination {
            thread_id: THREAD.into(),
            link_revision: 1,
            policy_revision: 4,
            confirmed_previous: Some("other".into()),
        }]
    );
}

// Given: the limit stopped auto fix / When: the reset is clicked here, and on a thread that does
// not have the budget / Then: only the first issues the reset
#[test]
fn the_reset_acts_only_where_the_budget_is() {
    let directory = tempfile::tempdir().unwrap();
    let limited = |destination: &'static str| {
        auto_state(move |s| {
            s.used = 3;
            s.destination_thread_id = Some(destination.into());
            s.link_revision = Some(1);
            s.stop_reason = Some("limit_reached".into());
        })
    };
    let reset = || PullRequestAction::ResetAutoFixBudget {
        thread_id: THREAD.into(),
    };
    let (mut window, _) = window(directory.path(), shown(limited(THREAD), |_| {}));
    window.apply_pull_request_action(reset());
    assert_eq!(
        window.issued(),
        [WorkbenchCommand::ResetPullRequestAutoFixBudget {
            thread_id: THREAD.into(),
            link_revision: 1,
            policy_revision: 4,
        }]
    );

    let other = tempfile::tempdir().unwrap();
    let (mut window, _) = self::window(other.path(), shown(limited("other"), |_| {}));
    window.apply_pull_request_action(reset());
    assert!(window.issued().is_empty());
}

// Given: a failing head / When: send is clicked / Then: the command names the head the person
// saw, so a newer head is never sent in its place; a head already sent is not offered
#[test]
fn send_names_the_head_that_was_shown_and_is_off_once_it_was_sent() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, _) = window(directory.path(), shown(auto_state(|_| {}), |_| {}));
    let send = || PullRequestAction::SendToAgent {
        thread_id: THREAD.into(),
    };
    window.apply_pull_request_action(send());
    window.apply_pull_request_action(send());
    assert_eq!(
        window.issued(),
        [WorkbenchCommand::SendPullRequestNotice {
            thread_id: THREAD.into(),
            link_revision: 1,
            expected_head: SHA_A.into(),
        }]
    );

    let other = tempfile::tempdir().unwrap();
    let (mut sent, _) = self::window(
        other.path(),
        shown(auto_state(|_| {}), |auto| {
            auto.head_notice = Some(notice(PrNoticePhase::Adopted, PrNoticeMode::Manual));
        }),
    );
    sent.apply_pull_request_action(send());
    assert!(sent.issued().is_empty());
}

// Given: a thread attached read-only / When: each auto fix operation is asked for / Then: none
// reaches the sink
#[test]
fn a_thread_attached_read_only_refuses_the_auto_fix_operations() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, _) = window(directory.path(), shown(auto_state(|_| {}), |_| {}));
    window.readonly_threads.insert(THREAD.to_owned());
    for action in [
        toggle(),
        PullRequestAction::SendToAgent {
            thread_id: THREAD.into(),
        },
    ] {
        window.apply_pull_request_action(action);
    }
    assert!(window.issued().is_empty(), "{:?}", window.issued());
}

// Given: a window that does not hold the thread / When: the buttons are clicked / Then: nothing
// is issued
#[test]
fn a_window_that_does_not_hold_the_thread_issues_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let reader = Arc::new(Reader::default());
    *reader.view.lock().unwrap() = Some(shown(auto_state(|_| {}), |_| {}));
    let mut window = state(directory.path())
        .with_pull_requests(reader, Some(Arc::new(|_: &str| false)))
        .with_pull_request_spawner(Arc::new(|job| job()));
    for _ in 0..3 {
        window.pump_pull_requests();
    }
    window.apply_pull_request_action(toggle());
    window.apply_pull_request_action(PullRequestAction::SendToAgent {
        thread_id: THREAD.into(),
    });
    assert!(window.issued().is_empty());
}

// Given: an operation was refused (the screen was out of date) / Then: the person is told, the
// thread is read again, and the same operation can be tried again from the new state
#[test]
fn a_refusal_reads_the_thread_again_and_allows_a_new_attempt() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, reader) = window(directory.path(), shown(auto_state(|_| {}), |_| {}));
    window.apply_pull_request_action(toggle());
    window.apply_pull_request_action(toggle());
    assert_eq!(window.issued().len(), 1);

    window.apply_loop_event(LoopEvent::PullRequestRejected {
        thread_id: THREAD.into(),
        reason: "The pull request changed meanwhile".into(),
    });
    // The newer state the second read finds is what the next click acts on.
    *reader.view.lock().unwrap() = Some(shown(auto_state(|s| s.policy_revision = 5), |_| {}));
    for _ in 0..3 {
        window.pump_pull_requests();
    }
    window.apply_pull_request_action(toggle());

    assert_eq!(window.issued().len(), 2);
    assert!(matches!(
        window.issued()[1],
        WorkbenchCommand::SetPullRequestAutoFix {
            policy_revision: 5,
            ..
        }
    ));
}

// Given: the third automatic send stopped auto fix / When: the runtime's event arrives twice, and
// the next budget epoch stops again / Then: Notifications gets one entry per epoch, and none
// when the database does not show the stop or the window does not hold the thread
#[test]
fn the_limit_is_notified_once_per_budget_epoch_and_only_when_the_database_agrees() {
    let directory = tempfile::tempdir().unwrap();
    let stopped = |epoch: u64| {
        auto_state(move |s| {
            s.used = 3;
            s.destination_thread_id = Some(THREAD.into());
            s.limit_reached_epoch = Some(epoch);
            s.stop_reason = Some("limit_reached".into());
        })
    };
    let (mut window, reader) = window(directory.path(), shown(stopped(0), |_| {}));

    window.apply_events([adopted(true, Some(0))]);
    for _ in 0..3 {
        window.pump_pull_requests();
    }
    assert_eq!(limit_notifications(&window), 1);

    window.apply_events([adopted(true, Some(0))]);
    for _ in 0..3 {
        window.pump_pull_requests();
    }
    assert_eq!(
        limit_notifications(&window),
        1,
        "the same epoch is told once"
    );

    *reader.view.lock().unwrap() = Some(shown(stopped(1), |_| {}));
    window.apply_events([adopted(true, Some(1))]);
    for _ in 0..3 {
        window.pump_pull_requests();
    }
    assert_eq!(limit_notifications(&window), 2);

    // The database does not show a stop at epoch 2: nothing is told.
    window.apply_events([adopted(true, Some(2))]);
    for _ in 0..3 {
        window.pump_pull_requests();
    }
    assert_eq!(limit_notifications(&window), 2);
    // An ordinary adoption is no reason to notify.
    window.apply_events([adopted(true, None)]);
    assert_eq!(limit_notifications(&window), 2);
}

// Given: another window's event for the same stop / Then: a shared event only reads again and
// raises nothing
#[test]
fn a_limit_another_process_reached_notifies_nobody_here() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, _) = window(
        directory.path(),
        shown(
            auto_state(|s| {
                s.used = 3;
                s.limit_reached_epoch = Some(0);
            }),
            |_| {},
        ),
    );
    window.apply_shared_pull_request_event(&adopted(true, Some(0)));
    for _ in 0..3 {
        window.pump_pull_requests();
    }
    assert_eq!(limit_notifications(&window), 0);
}

// Given: an expanded bar / When: an automatic notice is adopted / Then: the bar folds; a manual
// one leaves it as it is
#[test]
fn an_automatic_adoption_folds_the_bar_and_a_manual_one_does_not() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, _) = window(directory.path(), shown(auto_state(|_| {}), |_| {}));
    window.apply_pull_request_action(PullRequestAction::ToggleExpanded {
        thread_id: THREAD.into(),
    });
    assert!(window.pull_request_model().bar(THREAD).unwrap().expanded);

    window.apply_events([adopted(false, None)]);
    assert!(window.pull_request_model().bar(THREAD).unwrap().expanded);
    window.apply_events([adopted(true, None)]);
    assert!(!window.pull_request_model().bar(THREAD).unwrap().expanded);
}

fn card(id: &str) -> HarnessCard {
    HarnessCard {
        notice_id: id.into(),
        repository: "turtton/evorch".into(),
        number: 12,
        head_sha: SHA_A.into(),
        manual: true,
        checks: vec!["unit: failure".into()],
        observed_at: None,
        logs: Vec::new(),
        logs_truncated: false,
    }
}

fn delivered(run: &str, id: &str) -> Event {
    Event::new(LifecycleEvent::HarnessNoticeDelivered {
        run_id: run.into(),
        notification_id: id.into(),
    })
}

fn cards(window: &WorkbenchState<DemoSource>) -> Vec<(String, bool)> {
    window
        .transcript()
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            crate::model::transcript::TranscriptEntry::HarnessNotice { notice_id, card } => {
                Some((notice_id.clone(), card.is_some()))
            }
            _ => None,
        })
        .collect()
}

// Given: a delivery event of a durable notice / When: it arrives twice (live, then replayed) /
// Then: the transcript holds one card for the notice ID, filled from the stored notice by one
// read, and a second notice gets its own card
#[test]
fn a_notice_is_one_card_however_often_its_event_arrives() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, reader) = window(directory.path(), shown(auto_state(|_| {}), |_| {}));
    window.transcripts.bind_thread_root(THREAD, "run-1");
    reader
        .cards
        .lock()
        .unwrap()
        .insert("prn-1".into(), card("prn-1"));

    window.apply_events([delivered("run-1", "prn-1"), delivered("run-1", "prn-1")]);
    assert_eq!(cards(&window), [("prn-1".to_owned(), false)]);
    for _ in 0..3 {
        window.pump_pull_requests();
    }
    window.apply_events([delivered("run-1", "prn-1")]);
    for _ in 0..3 {
        window.pump_pull_requests();
    }
    assert_eq!(cards(&window), [("prn-1".to_owned(), true)]);
    assert_eq!(
        reader.card_reads.lock().unwrap().as_slice(),
        ["prn-1",],
        "a card that is filled is not read again"
    );

    window.apply_events([delivered("run-1", "prn-2")]);
    assert_eq!(
        cards(&window),
        [("prn-1".to_owned(), true), ("prn-2".to_owned(), false)]
    );
}
