use workspace_ui::{LineageKind, ProjectId, SidebarState, ThreadId, UiSettings};

use super::WorkbenchState;
use super::pull_requests::DestinationChange;
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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use event_bus::{
    Event, LifecycleEvent, PullRequestAutoFixChange, PullRequestEvent, PullRequestRef,
};
use storage::pull_request_auto_fix::PrLimitClaim;
use storage::pull_request_auto_fix::{PrAutoFixState, PrNoticeMode, PrNoticePhase};

use super::pull_requests::{LimitClaimOutcome, PullRequestReader};
use crate::model::commands::LoopEvent;
use crate::model::harness_card::HarnessCard;
use crate::model::notifications::NotificationKind;
use crate::model::pull_requests::PullRequestAction;
use crate::model::pull_requests::ThreadPullRequest;
use crate::model::pull_requests::tests::{
    SHA_A, auto_state, failing, notice, on_here, passing, with_auto,
};

const THREAD: &str = "parent";

#[derive(Default)]
struct Reader {
    /// The limit claims of the database, shared by every window of a test.
    claims: Arc<Mutex<BTreeSet<u64>>>,
    claim_failures: Mutex<usize>,
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

    fn claim_limit_notification(
        &self,
        claim: &PrLimitClaim,
        still_held: &dyn Fn() -> bool,
    ) -> Result<LimitClaimOutcome, String> {
        let mut failures = self.claim_failures.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            return Err("busy".into());
        }
        if !still_held() {
            return Ok(LimitClaimOutcome::NotHeld);
        }
        Ok(if self.claims.lock().unwrap().insert(claim.budget_epoch) {
            LimitClaimOutcome::Claimed
        } else {
            LimitClaimOutcome::Declined
        })
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

fn adopted(automatic: bool) -> Event {
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
        reason: PullRequestAutoFixChange::Adopted { automatic },
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

// Given: the limit stopped auto fix / When: the reset is clicked on the thread that has the
// budget / Then: it is issued at once, with no confirmation to name
#[test]
fn the_reset_where_the_budget_is_needs_no_confirmation() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, _) = window(directory.path(), shown(limited(THREAD), |_| {}));
    window.apply_pull_request_action(reset());
    assert!(window.destination_dialog().is_none());
    assert_eq!(
        window.issued(),
        [WorkbenchCommand::ResetPullRequestAutoFixBudget {
            thread_id: THREAD.into(),
            link_revision: 1,
            policy_revision: 4,
            confirmed_previous: None,
        }]
    );
}

// Given: the limit stopped auto fix in another thread / When: the reset is clicked here / Then:
// the same confirmation as moving auto fix opens and nothing is issued; a cancel changes nothing
// and a confirm issues the reset naming the thread it leaves
#[test]
fn the_reset_from_another_thread_is_confirmed_first_and_can_be_cancelled() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, _) = window(directory.path(), shown(limited("other"), |_| {}));
    let controls = window.auto_fix_controls(THREAD).unwrap();
    assert!(controls.reset.is_some_and(|reset| reset.enabled));

    window.apply_pull_request_action(reset());
    let dialog = window
        .destination_dialog()
        .cloned()
        .expect("a move is asked about");
    assert_eq!(dialog.previous_thread_id, "other");
    assert_eq!(dialog.then, DestinationChange::ResetBudget);
    assert!(window.issued().is_empty());

    window.apply_pull_request_action(PullRequestAction::CancelAutoFixDestination);
    assert!(window.destination_dialog().is_none() && window.issued().is_empty());

    window.apply_pull_request_action(reset());
    window.apply_pull_request_action(PullRequestAction::ConfirmAutoFixDestination {
        thread_id: THREAD.into(),
    });
    assert!(window.destination_dialog().is_none());
    assert_eq!(
        window.issued(),
        [WorkbenchCommand::ResetPullRequestAutoFixBudget {
            thread_id: THREAD.into(),
            link_revision: 1,
            policy_revision: 4,
            confirmed_previous: Some("other".into()),
        }]
    );
}

fn limited(destination: &'static str) -> PrAutoFixState {
    auto_state(move |s| {
        s.used = 3;
        s.destination_thread_id = Some(destination.into());
        s.link_revision = Some(1);
        s.stop_reason = Some("limit_reached".into());
    })
}

fn reset() -> PullRequestAction {
    PullRequestAction::ResetAutoFixBudget {
        thread_id: THREAD.into(),
    }
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

fn stopped(epoch: u64) -> ThreadPullRequest {
    shown(
        auto_state(move |s| {
            s.used = 3;
            s.destination_thread_id = Some(THREAD.into());
            s.link_revision = Some(1);
            s.budget_epoch = epoch;
            s.limit_reached_epoch = Some(epoch);
            s.stop_reason = Some("limit_reached".into());
        }),
        |_| {},
    )
}

fn pump(window: &mut WorkbenchState<DemoSource>) {
    for _ in 0..4 {
        window.pump_pull_requests();
    }
}

/// A window of a process that starts on `claims`, the database's limit claims, and `view`.
fn window_on(
    directory: &std::path::Path,
    claims: &Arc<Mutex<BTreeSet<u64>>>,
    view: ThreadPullRequest,
    holds_thread: bool,
) -> (WorkbenchState<DemoSource>, Arc<Reader>) {
    let reader = Arc::new(Reader {
        claims: Arc::clone(claims),
        ..Reader::default()
    });
    *reader.view.lock().unwrap() = Some(view);
    let mut window = state(directory)
        .with_pull_requests(reader.clone(), Some(Arc::new(move |_: &str| holds_thread)))
        .with_pull_request_spawner(Arc::new(|job| job()));
    pump(&mut window);
    (window, reader)
}

// Given: auto fix stopped at the limit and the process ended before its event reached the
// window / When: a window starts on that database / Then: Notifications gets the stop once, however
// often the thread is read again
#[test]
fn a_stop_whose_event_was_lost_is_notified_when_the_database_is_read() {
    let directory = tempfile::tempdir().unwrap();
    let claims = Arc::default();
    let (mut window, _) = window_on(directory.path(), &claims, stopped(0), true);
    assert_eq!(limit_notifications(&window), 1);

    window.apply_events([adopted(true)]);
    pump(&mut window);
    assert_eq!(limit_notifications(&window), 1);
}

// Given: a stop the runtime's event already notified / When: the app restarts, and a second window
// opens / Then: neither tells it again
#[test]
fn a_stop_already_notified_is_not_told_again_after_a_restart_or_in_another_window() {
    let directory = tempfile::tempdir().unwrap();
    let claims = Arc::default();
    let (mut first, reader) = window_on(
        directory.path(),
        &claims,
        shown(auto_state(|_| {}), |_| {}),
        true,
    );
    *reader.view.lock().unwrap() = Some(stopped(0));
    first.apply_events([adopted(true)]);
    pump(&mut first);
    assert_eq!(limit_notifications(&first), 1);

    let (restarted, _) = window_on(directory.path(), &claims, stopped(0), true);
    assert_eq!(limit_notifications(&restarted), 0);
    let (other_window, _) = window_on(directory.path(), &claims, stopped(0), true);
    assert_eq!(limit_notifications(&other_window), 0);
}

// Given: a stop shown to a window that does not hold the thread / Then: it raises nothing and
// leaves the notification to the owner
#[test]
fn a_window_that_does_not_hold_the_thread_leaves_the_stop_to_the_owner() {
    let directory = tempfile::tempdir().unwrap();
    let claims: Arc<Mutex<BTreeSet<u64>>> = Arc::default();
    let (mut other, _) = window_on(directory.path(), &claims, stopped(0), false);
    other.apply_shared_pull_request_event(&adopted(true));
    pump(&mut other);
    assert_eq!(limit_notifications(&other), 0);
    assert!(claims.lock().unwrap().is_empty());

    let (owner, _) = window_on(directory.path(), &claims, stopped(0), true);
    assert_eq!(limit_notifications(&owner), 1);
}

// Given: a stop that was told / When: the budget is reset and the next epoch stops again / Then:
// that stop is told once more, and neither the reset state nor a manual off tells anything
#[test]
fn a_reset_budget_notifies_its_next_stop() {
    let directory = tempfile::tempdir().unwrap();
    let claims = Arc::default();
    let (mut window, reader) = window_on(directory.path(), &claims, stopped(0), true);
    assert_eq!(limit_notifications(&window), 1);

    let reset = shown(
        auto_state(|s| {
            on_here_state(s);
            s.budget_epoch = 1;
            s.limit_reached_epoch = Some(0);
        }),
        |_| {},
    );
    *reader.view.lock().unwrap() = Some(reset);
    window.apply_events([adopted_policy()]);
    pump(&mut window);
    assert_eq!(limit_notifications(&window), 1);

    *reader.view.lock().unwrap() = Some(stopped(1));
    window.apply_events([adopted(true)]);
    pump(&mut window);
    assert_eq!(limit_notifications(&window), 2);

    let disabled = shown(
        auto_state(|s| {
            s.budget_epoch = 1;
            s.limit_reached_epoch = Some(1);
            s.stop_reason = Some("disabled".into());
        }),
        |_| {},
    );
    *reader.view.lock().unwrap() = Some(disabled);
    window.apply_events([adopted_policy()]);
    pump(&mut window);
    assert_eq!(limit_notifications(&window), 2);
}

// Given: a claim that could not be written / When: the thread is read again / Then: the stop is
// told once, when the claim succeeds
#[test]
fn a_failed_claim_is_tried_again_by_the_next_read() {
    let directory = tempfile::tempdir().unwrap();
    let claims = Arc::default();
    let reader = Arc::new(Reader {
        claims: Arc::clone(&claims),
        ..Reader::default()
    });
    *reader.claim_failures.lock().unwrap() = 1;
    *reader.view.lock().unwrap() = Some(stopped(0));
    let mut window = state(directory.path())
        .with_pull_requests(reader, None)
        .with_pull_request_spawner(Arc::new(|job| job()));
    pump(&mut window);
    assert_eq!(limit_notifications(&window), 0);

    window.apply_events([adopted(true)]);
    pump(&mut window);
    assert_eq!(limit_notifications(&window), 1);
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

    window.apply_events([adopted(false)]);
    for _ in 0..3 {
        window.pump_pull_requests();
    }
    assert!(window.pull_request_model().bar(THREAD).unwrap().expanded);
    window.apply_events([adopted(true)]);
    for _ in 0..3 {
        window.pump_pull_requests();
    }
    assert!(!window.pull_request_model().bar(THREAD).unwrap().expanded);
}

// Given: a bar that has seen the checks pass / When: the failure arrives with the read that
// follows an automatic adoption, and later reads come / Then: the failure transition does not
// unfold the bar the adoption folded, and it stays folded; the next failure expands it again
#[test]
fn the_fold_of_an_adoption_survives_the_read_that_shows_the_failure() {
    let directory = tempfile::tempdir().unwrap();
    let (mut window, reader) = window(
        directory.path(),
        shown_with(passing(), auto_state(on_here_state)),
    );
    let bar = |window: &WorkbenchState<DemoSource>| {
        window.pull_request_model().bar(THREAD).unwrap().expanded
    };
    assert!(!bar(&window));

    *reader.view.lock().unwrap() = Some(shown_with(failing(), auto_state(on_here_state)));
    window.apply_events([adopted(true)]);
    for _ in 0..4 {
        window.pump_pull_requests();
    }
    assert!(
        !bar(&window),
        "the failure transition expanded the bar after the fold"
    );

    window.apply_events([adopted_policy()]);
    for _ in 0..4 {
        window.pump_pull_requests();
    }
    assert!(!bar(&window), "a later read does not unfold it");

    // A new failure is a new transition: it expands the bar once more.
    *reader.view.lock().unwrap() = Some(shown_with(passing(), auto_state(on_here_state)));
    window.apply_events([adopted_policy()]);
    for _ in 0..4 {
        window.pump_pull_requests();
    }
    *reader.view.lock().unwrap() = Some(shown_with(failing(), auto_state(on_here_state)));
    window.apply_events([adopted_policy()]);
    for _ in 0..4 {
        window.pump_pull_requests();
    }
    assert!(bar(&window));
}

fn shown_with(
    facts: storage::pull_requests::PullRequestFacts,
    state: PrAutoFixState,
) -> ThreadPullRequest {
    let mut shown = with_auto(facts, state, |_| {});
    shown.thread_id = THREAD.into();
    if let Some(link) = &mut shown.link {
        link.thread_id = THREAD.into();
    }
    shown
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

// --- the claim of a limit notification against a real database ---

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use storage::pull_request_auto_fix::{PrPolicyAction, PrPolicyUpdate};
use storage::pull_requests::{LinkPullRequest, PullRequestKey};
use storage::{Storage, StorageConfig, StorageHandle};

use super::pull_requests::StoragePullRequestReader;

/// A database where `parent` is linked to turtton/evorch#12 (link revision 1) and auto fix,
/// destined for `parent`, stopped at the limit of budget epoch 0.
struct StoppedWorld {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Storage,
}

fn pull_request_key() -> PullRequestKey {
    PullRequestKey::github("turtton/evorch", 12).unwrap()
}

impl StoppedWorld {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("limit.db"),
            ..StorageConfig::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        let world = Self {
            _dir: dir,
            config,
            storage,
        };
        world.sql(
            "INSERT INTO threads(thread_id,archived,created_at_ns,updated_at_ns,source) VALUES ('parent',0,1,1,'runtime')",
        );
        world.link(0);
        world
            .handle()
            .update_pr_auto_fix_policy(PrPolicyUpdate {
                key: pull_request_key(),
                expected_policy_revision: 0,
                expected_destination_thread_id: None,
                destination_thread_id: THREAD.into(),
                link_revision: 1,
                action: PrPolicyAction::SetEnabled(true),
            })
            .unwrap();
        world.sql(
            "UPDATE pr_auto_fix_state SET enabled=0,used=3,stop_reason='limit_reached',limit_reached_epoch=0;
             INSERT INTO pr_auto_fix_limit_events(pr_key,budget_epoch,notice_id,at_ns) SELECT pr_key,0,'n',1 FROM pr_auto_fix_state;",
        );
        world
    }

    fn handle(&self) -> StorageHandle {
        self.storage.handle()
    }

    fn sql(&self, statements: &str) {
        let connection = rusqlite::Connection::open(&self.config.db_path).unwrap();
        connection.execute_batch(statements).unwrap();
    }

    fn link(&self, expected_revision: u64) {
        self.handle()
            .link_pull_request(LinkPullRequest {
                thread_id: THREAD.into(),
                key: pull_request_key(),
                expected_revision,
                at_ns: 1,
            })
            .unwrap();
    }

    fn reader(&self) -> StoragePullRequestReader {
        StoragePullRequestReader::new(self.config.db_path.clone()).with_limit_claims(self.handle())
    }

    fn notified(&self) -> Option<i64> {
        self.handle()
            .pr_auto_fix_limit_events(pull_request_key(), None)
            .unwrap()[0]
            .notified_at_ns
    }
}

type Step = Box<dyn FnOnce() + Send>;

/// The real reader, with a step the test runs inside the claim job, at the moment a queued
/// claim would have been waiting: once before the claim is written, and once between the claim
/// and the second ask whether the window still holds the thread.
struct Spy {
    inner: StoragePullRequestReader,
    before_claim: Mutex<Option<Step>>,
    after_claim: Mutex<Option<Step>>,
    claims_run: AtomicUsize,
}

impl Spy {
    fn new(inner: StoragePullRequestReader) -> Arc<Self> {
        Arc::new(Self {
            inner,
            before_claim: Mutex::new(None),
            after_claim: Mutex::new(None),
            claims_run: AtomicUsize::new(0),
        })
    }
}

impl PullRequestReader for Spy {
    fn load(&self, thread_id: &str) -> Result<ThreadPullRequest, String> {
        self.inner.load(thread_id)
    }

    fn linked_threads(&self) -> Result<Vec<String>, String> {
        self.inner.linked_threads()
    }

    fn harness_card(&self, notice_id: &str) -> Result<Option<HarnessCard>, String> {
        self.inner.harness_card(notice_id)
    }

    fn claim_limit_notification(
        &self,
        claim: &PrLimitClaim,
        still_held: &dyn Fn() -> bool,
    ) -> Result<LimitClaimOutcome, String> {
        self.claims_run.fetch_add(1, Ordering::SeqCst);
        if let Some(step) = self.before_claim.lock().unwrap().take() {
            step();
        }
        let asked = AtomicUsize::new(0);
        self.inner.claim_limit_notification(claim, &|| {
            // The second ask comes after the claim is written.
            if asked.fetch_add(1, Ordering::SeqCst) == 1
                && let Some(step) = self.after_claim.lock().unwrap().take()
            {
                step();
            }
            still_held()
        })
    }
}

fn real_window(
    directory: &std::path::Path,
    reader: Arc<Spy>,
    held: &Arc<AtomicBool>,
) -> WorkbenchState<DemoSource> {
    let held = Arc::clone(held);
    let mut window = state(directory)
        .with_pull_requests(
            reader,
            Some(Arc::new(move |_: &str| held.load(Ordering::SeqCst))),
        )
        .with_pull_request_spawner(Arc::new(|job| job()));
    pump(&mut window);
    window
}

// Given: the claim of a stop is about to be written / When: the window loses the thread before
// it runs / Then: it spends nothing and tells nobody; the window that holds the thread tells
// the stop once
#[test]
fn a_window_that_lost_the_thread_before_its_claim_ran_spends_nothing() {
    let world = StoppedWorld::new();
    let directory = tempfile::tempdir().unwrap();
    let held = Arc::new(AtomicBool::new(true));
    let spy = Spy::new(world.reader());
    let lose = Arc::clone(&held);
    *spy.before_claim.lock().unwrap() = Some(Box::new(move || lose.store(false, Ordering::SeqCst)));

    let old_owner = real_window(directory.path(), spy.clone(), &held);

    assert_eq!(spy.claims_run.load(Ordering::SeqCst), 1);
    assert_eq!(limit_notifications(&old_owner), 0);
    assert_eq!(world.notified(), None);

    let new_owner = real_window(
        directory.path(),
        Spy::new(world.reader()),
        &Arc::new(AtomicBool::new(true)),
    );
    assert_eq!(limit_notifications(&new_owner), 1);
    assert!(world.notified().is_some());
}

// Given: the claim was written / When: the window turns out to have lost the thread meanwhile /
// Then: the claim is given back, nobody is told here, and the window that holds the thread takes
// it
#[test]
fn a_claim_taken_just_as_the_thread_was_lost_is_given_back() {
    let world = StoppedWorld::new();
    let directory = tempfile::tempdir().unwrap();
    let held = Arc::new(AtomicBool::new(true));
    let spy = Spy::new(world.reader());
    let lose = Arc::clone(&held);
    *spy.after_claim.lock().unwrap() = Some(Box::new(move || lose.store(false, Ordering::SeqCst)));

    let old_owner = real_window(directory.path(), spy, &held);

    assert_eq!(limit_notifications(&old_owner), 0);
    assert_eq!(world.notified(), None, "the claim stayed taken");

    let new_owner = real_window(
        directory.path(),
        Spy::new(world.reader()),
        &Arc::new(AtomicBool::new(true)),
    );
    assert_eq!(limit_notifications(&new_owner), 1);
}

// Given: the claim of a stop is about to be written / When: the link ends, or ends and is made
// again, before it runs / Then: the stop is not told, and the claim is not spent
#[test]
fn an_unlink_or_a_relink_before_the_claim_ran_tells_nothing() {
    for relink in [false, true] {
        let world = StoppedWorld::new();
        let directory = tempfile::tempdir().unwrap();
        let held = Arc::new(AtomicBool::new(true));
        let spy = Spy::new(world.reader());
        let handle = world.handle();
        *spy.before_claim.lock().unwrap() = Some(Box::new(move || {
            handle.unlink_pull_request(THREAD, 1, 2).unwrap();
            if relink {
                handle
                    .link_pull_request(LinkPullRequest {
                        thread_id: THREAD.into(),
                        key: pull_request_key(),
                        expected_revision: 2,
                        at_ns: 3,
                    })
                    .unwrap();
            }
        }));

        let window = real_window(directory.path(), spy, &held);

        assert_eq!(limit_notifications(&window), 0, "relink: {relink}");
        assert_eq!(world.notified(), None, "relink: {relink}");
    }
}

// Given: a stop shown to the window that holds the thread / When: the claim job has run / Then:
// the notification is raised by the frame that drains its result (no read or wait in between),
// and a window that began to close starts no claim
#[test]
fn the_claim_result_is_shown_by_the_frame_that_drains_it_and_closing_starts_no_claim() {
    let world = StoppedWorld::new();
    let directory = tempfile::tempdir().unwrap();
    let held = Arc::new(AtomicBool::new(true));

    let closing_spy = Spy::new(world.reader());
    let reader = Arc::clone(&closing_spy);
    let mut closing = state(directory.path())
        .with_pull_requests(reader, Some(Arc::new(|_: &str| true)))
        .with_pull_request_spawner(Arc::new(|job| job()));
    closing.shutdown_requested = true;
    pump(&mut closing);
    assert_eq!(closing_spy.claims_run.load(Ordering::SeqCst), 0);
    assert_eq!(world.notified(), None);

    let spy = Spy::new(world.reader());
    let mut window = state(directory.path())
        .with_pull_requests(
            spy.clone(),
            Some(Arc::new({
                let held = Arc::clone(&held);
                move |_: &str| held.load(Ordering::SeqCst)
            })),
        )
        .with_pull_request_spawner(Arc::new(|job| job()));
    while spy.claims_run.load(Ordering::SeqCst) == 0 {
        window.pump_pull_requests();
    }
    // The pump that ran the claim also drained its result: nothing else ran in between.
    assert_eq!(limit_notifications(&window), 1);
    assert!(world.notified().is_some());
}

// Given: window A took the claim and then lost the thread to window B / When: B's claim is declined
// because A's is still taken, and A then gives its claim back / Then: B's next read claims it and
// tells the stop, once
#[test]
fn a_declined_claim_is_tried_again_once_the_other_window_gave_its_claim_back() {
    use std::sync::mpsc::channel;

    let world = StoppedWorld::new();
    let directory = tempfile::tempdir().unwrap();
    let held_a = Arc::new(AtomicBool::new(true));
    let spy_a = Spy::new(world.reader());

    // B is a window of another process, which can only live on its own thread.
    let (go_tx, go_rx) = channel::<()>();
    let (declined_tx, declined_rx) = channel::<usize>();
    let (again_tx, again_rx) = channel::<()>();
    let (told_tx, told_rx) = channel::<usize>();
    let reader_b = world.reader();
    let directory_b = tempfile::tempdir().unwrap();
    let window_b = std::thread::spawn(move || {
        let spy_b = Spy::new(reader_b);
        let held_b = Arc::new(AtomicBool::new(true));
        let mut window = state(directory_b.path())
            .with_pull_requests(
                spy_b.clone(),
                Some(Arc::new({
                    let held = Arc::clone(&held_b);
                    move |_: &str| held.load(Ordering::SeqCst)
                })),
            )
            .with_pull_request_spawner(Arc::new(|job| job()));
        go_rx.recv().unwrap();
        while spy_b.claims_run.load(Ordering::SeqCst) == 0 {
            window.pump_pull_requests();
        }
        declined_tx.send(limit_notifications(&window)).unwrap();
        again_rx.recv().unwrap();
        window.apply_shared_pull_request_event(&adopted(true));
        pump(&mut window);
        told_tx.send(limit_notifications(&window)).unwrap();
    });

    let lose = Arc::clone(&held_a);
    *spy_a.after_claim.lock().unwrap() = Some(Box::new(move || {
        // A has taken the claim; the thread moves to B, whose claim finds it taken.
        lose.store(false, Ordering::SeqCst);
        go_tx.send(()).unwrap();
        assert_eq!(
            declined_rx.recv().unwrap(),
            0,
            "B was declined and tells nothing yet"
        );
    }));
    let window_a = real_window(directory.path(), spy_a, &held_a);
    assert_eq!(limit_notifications(&window_a), 0);
    assert_eq!(world.notified(), None, "A gave its claim back");

    again_tx.send(()).unwrap();
    assert_eq!(
        told_rx.recv().unwrap(),
        1,
        "B claims on its next read and tells the stop"
    );
    window_b.join().unwrap();
    assert!(world.notified().is_some());
}
