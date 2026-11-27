use super::*;
use crate::core_client::features::execution::tests::{
    Screen, answer, asked, command_of, query_of, screen,
};
use crate::core_client::features::{
    FeatureAction, FeatureError, FeatureFrame, FeatureReply, FeatureRequest, FeatureView,
};
use crate::panes::pull_request_widgets::{INPUT_LABEL, MOVE_LABEL, PrGlyph};
use egui::accesskit::Role;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use workbench_protocol::{
    commands::CommandReply,
    errors::RetryAdvice,
    namespaces::pull_requests::{
        AutoFixView, CheckView, FactsView, HeadAttribution, LinkOrigin, LinkView,
        PullRequestChange, PullRequestState, ReviewDecision, SnapshotView,
    },
    wire::{Sequence, UnixMillis},
};

const THREAD: &str = "t1";
const HEAD: &str = "0123456789abcdef";

fn reference() -> PullRequestRef {
    PullRequestRef {
        provider: "github".into(),
        host: "github.com".into(),
        repository: "o/r".into(),
        number: 7,
    }
}

fn id(thread: &str) -> ThreadId {
    ThreadId::new(thread).unwrap()
}

fn thread() -> ThreadId {
    id(THREAD)
}

fn auto_fix_here(limit_reached: bool) -> AutoFixView {
    AutoFixView {
        enabled: !limit_reached,
        globally_enabled: true,
        destination: Some(thread()),
        link_revision: Some(Revision(4)),
        policy_revision: Revision(9),
        used: if limit_reached { 3 } else { 1 },
        reserved: 0,
        limit: 3,
        cooldown_until: None,
        stop_reason: limit_reached.then(|| "limit_reached".into()),
        limit_reached,
    }
}

fn check(name: &str, conclusion: &str) -> CheckView {
    CheckView {
        name: name.into(),
        kind: "check_run".into(),
        status: "completed".into(),
        conclusion: Some(conclusion.into()),
    }
}

fn open_snapshot(failing: bool) -> SnapshotView {
    let mut checks = vec![check("lint", "success")];
    if failing {
        checks.push(check("test", "failure"));
    }
    SnapshotView {
        facts: Some(FactsView {
            state: PullRequestState::Open,
            base_branch: "main".into(),
            head_repository: Some("o/r".into()),
            head_branch: "feat/x".into(),
            head_sha: HEAD.into(),
            checks,
            checks_complete: true,
            review_decision: ReviewDecision::None,
        }),
        title: Some("Fix the redirect".into()),
        fetched_at: Some(UnixMillis(u64::try_from(now_ns() / 1_000_000).unwrap())),
        last_failure: None,
        consecutive_failures: 0,
    }
}

fn view_of(thread: &str, linked: bool, owned_here: bool) -> ThreadPullRequest {
    ThreadPullRequest {
        thread: id(thread),
        tracking_enabled: true,
        owned_here,
        lifecycle_revision: Revision(3),
        transition_seq: Sequence(1),
        link: linked.then(|| LinkView {
            pull_request: reference(),
            link_revision: Revision(4),
            origin: LinkOrigin::Explicit,
            linked_at: UnixMillis(0),
        }),
        local_head: Some(HEAD.into()),
        snapshot: linked.then(|| open_snapshot(false)),
        candidates: vec![],
        auto_fix: linked.then(|| auto_fix_here(false)),
        head_attribution: HeadAttribution::Agent,
    }
}

fn view(linked: bool, owned_here: bool) -> ThreadPullRequest {
    view_of(THREAD, linked, owned_here)
}

fn failing(mut view: ThreadPullRequest) -> ThreadPullRequest {
    view.snapshot = Some(open_snapshot(true));
    view
}

fn thread_reply(view: ThreadPullRequest) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::PullRequests(PullRequestsReply::Thread(
        Box::new(view),
    ))))
}

fn operation_view(
    id: &OperationId,
    kind: PullRequestOperationKind,
    state: OperationState,
) -> PullRequestOperationView {
    PullRequestOperationView {
        operation: OperationView {
            operation_id: id.clone(),
            state,
            revision: Revision(1),
            result: None,
        },
        kind,
        thread: thread(),
        lifecycle_revision: None,
        policy_revision: None,
        notice_id: None,
        message: None,
        outcome_unknown: false,
    }
}

fn command_reply(view: PullRequestOperationView) -> FeatureReply {
    FeatureReply::Command(Ok(CommandReply::with_result(CommandResult::PullRequests(
        PullRequestsResult::Operation(view),
    ))))
}

fn is_read_of(action: &FeatureAction, thread: &str) -> bool {
    matches!(
        &action.request,
        FeatureRequest::Query(Query::PullRequests(PullRequestsQuery::GetThreadPullRequest {
            thread: t
        })) if t.as_str() == thread
    )
}

fn loaded(view: ThreadPullRequest) -> Harness<'static, Screen> {
    let mut screen = screen(Box::<PullRequestsFeature>::default(), Some(THREAD));
    let first = asked(&mut screen);
    assert_eq!(first.len(), 1);
    assert!(is_read_of(&first[0], THREAD));
    answer(&mut screen, first[0].tag, thread_reply(view));
    screen
}

fn feature<'a>(screen: &'a mut Harness<'static, Screen>) -> &'a mut PullRequestsFeature {
    screen.state_mut().feature.as_pull_requests_mut().unwrap()
}

fn pr_command(action: &FeatureAction) -> &PullRequestsCommand {
    let Command::PullRequests(command) = command_of(action) else {
        panic!("a pull request command was expected");
    };
    command
}

#[test]
fn unlink_quotes_the_lifecycle_revision_and_is_not_sent_twice() {
    let mut screen = loaded(view(true, true));
    screen.get_by_label("Unlink").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1, "one click, one command");
    let PullRequestsCommand::UnlinkPullRequest(unlink) = pr_command(&sent[0]).clone() else {
        panic!("an unlink was expected");
    };
    assert_eq!(unlink.expected_revision, Revision(3));
    answer(
        &mut screen,
        sent[0].tag,
        command_reply(operation_view(
            &unlink.operation_id,
            PullRequestOperationKind::Unlink,
            OperationState::Pending,
        )),
    );
    screen.get_by_label("Working…");
    assert!(screen.get_by_label("Unlink").accesskit_node().is_disabled());
    assert!(asked(&mut screen).is_empty());
}

#[test]
fn a_refused_command_says_it_is_stale_and_reads_the_thread_again() {
    let mut screen = loaded(view(true, true));
    screen.get_by_label("Refresh").click();
    let sent = asked(&mut screen);
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError {
            message: "stale".into(),
            outcome_unknown: false,
            code: Some(ErrorCode::StaleRevision),
            retry: None,
        })),
    );
    screen
        .get_by_label("Pull request: it changed elsewhere. The latest state was read; try again.");
    let again = asked(&mut screen);
    assert_eq!(again.len(), 1);
    assert!(is_read_of(&again[0], THREAD));
}

fn unlink_refused_for_a_limit(retry: RetryAdvice) -> Harness<'static, Screen> {
    let mut screen = loaded(view(true, true));
    screen.get_by_label("Unlink").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError::new("limit")
            .with_code(ErrorCode::LimitExceeded)
            .with_retry(retry))),
    );
    screen
}

fn assert_not_resent(screen: &mut Harness<'static, Screen>) {
    let after = asked(screen);
    assert!(
        after.iter().all(|action| is_read_of(action, THREAD)),
        "a refused command is not resent: {after:?}"
    );
}

#[test]
fn a_full_memory_of_commands_says_a_restart_clears_it_and_is_not_resent() {
    let mut screen = unlink_refused_for_a_limit(RetryAdvice::Never);
    screen.get_by_label(limit_label(Some(RetryAdvice::Never)));
    assert!(limit_label(Some(RetryAdvice::Never)).contains("restart"));
    assert_not_resent(&mut screen);
}

#[test]
fn too_many_running_operations_reads_differently_and_is_not_resent() {
    let mut screen = unlink_refused_for_a_limit(RetryAdvice::UserActionRequired);
    screen.get_by_label(limit_label(Some(RetryAdvice::UserActionRequired)));
    assert_ne!(
        limit_label(Some(RetryAdvice::UserActionRequired)),
        limit_label(Some(RetryAdvice::Never)),
        "the two limits are told apart"
    );
    assert!(!limit_label(Some(RetryAdvice::UserActionRequired)).contains("restart"));
    assert_not_resent(&mut screen);
}

const UNKNOWN: &str = "Pull request: the outcome could not be confirmed. Read it again to check.";

#[test]
fn a_lost_reply_is_checked_by_operation_id_and_never_shown_as_done() {
    let mut screen = loaded(view(true, true));
    screen.get_by_label("Unlink").click();
    let sent = asked(&mut screen);
    let PullRequestsCommand::UnlinkPullRequest(unlink) = pr_command(&sent[0]).clone() else {
        panic!("an unlink was expected");
    };
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError {
            message: "lost".into(),
            outcome_unknown: true,
            code: None,
            retry: None,
        })),
    );
    screen.get_by_label(UNKNOWN);
    assert!(screen.query_by_label("Pull request unlinked.").is_none());
    let check = asked(&mut screen);
    assert_eq!(check.len(), 1, "the operation is asked for, not resent");
    assert!(matches!(
        query_of(&check[0]),
        Query::PullRequests(PullRequestsQuery::GetPullRequestOperation { operation_id })
            if *operation_id == unlink.operation_id
    ));
    answer(
        &mut screen,
        check[0].tag,
        FeatureReply::Query(Err(FeatureError {
            message: "gone".into(),
            outcome_unknown: false,
            code: Some(ErrorCode::NotFound),
            retry: None,
        })),
    );
    screen.get_by_label(UNKNOWN);
    assert!(screen.query_by_label("Pull request unlinked.").is_none());
}

#[test]
fn a_thread_the_core_does_not_hold_is_read_only() {
    let screen = loaded(view(true, false));
    assert!(
        screen
            .query_all_by_label("Not tracked in this window")
            .next()
            .is_some()
    );
    for label in ["Unlink", "Refresh"] {
        assert!(screen.get_by_label(label).accesskit_node().is_disabled());
    }
}

#[test]
fn the_limit_notice_is_shown_from_the_core_event_and_decided_nowhere_else() {
    let mut screen = loaded(view(true, true));
    screen.state_mut().feature.event(&UiEvent::PullRequests(
        PullRequestsEvent::AutoFixLimitReached {
            thread: thread(),
            pull_request: reference(),
            link_revision: Revision(4),
            budget_epoch: 1,
            limit: 3,
        },
    ));
    screen.run_steps(2);
    screen.get_by_label("Auto fix stopped on o/r#7 after 3 sends");
    let sent = asked(&mut screen);
    assert!(
        sent.iter()
            .all(|action| matches!(action.request, FeatureRequest::Query(_)))
    );
}

#[test]
fn a_notification_the_core_claimed_is_said_in_the_embedded_words() {
    let mut screen = loaded(view(true, true));
    screen.state_mut().feature.event(&UiEvent::PullRequests(
        PullRequestsEvent::PullRequestChanged {
            thread: thread(),
            pull_request: reference(),
            change: PullRequestChange::FactsChanged,
            link_revision: Revision(4),
            transition_seq: Sequence(2),
            notifications: vec!["checks_failed".into(), "merged".into()],
        },
    ));
    screen.run_steps(2);
    screen.get_by_label("CI failed on o/r#7");
    screen.get_by_label("o/r#7 merged");
}

fn type_into_link_dialog(screen: &mut Harness<'static, Screen>, text: &str) {
    screen
        .get_by_role_and_label(Role::TextInput, INPUT_LABEL)
        .focus();
    screen.run_steps(2);
    screen
        .get_by_role_and_label(Role::TextInput, INPUT_LABEL)
        .type_text(text);
    screen.run_steps(2);
}

fn link_button(screen: &Harness<'static, Screen>) -> bool {
    !screen
        .get_by_role_and_label(Role::Button, "Link")
        .accesskit_node()
        .is_disabled()
}

#[test]
fn linking_goes_through_the_dialog_and_closes_when_the_core_links() {
    let mut screen = loaded(view(false, true));
    screen.get_by_label("No pull request is linked to this thread.");
    screen.get_by_label("Link PR…").click();
    screen.run_steps(2);
    screen.get_by_label("Link pull request");
    type_into_link_dialog(&mut screen, "o/r#7");
    assert!(link_button(&screen));
    screen.get_by_role_and_label(Role::Button, "Link").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let PullRequestsCommand::LinkPullRequest(link) = pr_command(&sent[0]).clone() else {
        panic!("a link was expected");
    };
    assert_eq!(link.locator, "o/r#7");
    assert_eq!(link.expected_revision, Revision(3));
    answer(
        &mut screen,
        sent[0].tag,
        command_reply(operation_view(
            &link.operation_id,
            PullRequestOperationKind::Link,
            OperationState::Pending,
        )),
    );
    screen.get_by_label("Link pull request");
    assert!(!link_button(&screen), "one link at a time");

    let mut done = operation_view(
        &link.operation_id,
        PullRequestOperationKind::Link,
        OperationState::Ready,
    );
    done.operation.revision = Revision(2);
    screen.state_mut().feature.event(&UiEvent::PullRequests(
        PullRequestsEvent::PullRequestOperationCompleted { operation: done },
    ));
    screen.run_steps(2);
    assert!(screen.query_by_label("Link pull request").is_none());
    screen.get_by_label("Pull request linked.");
}

#[test]
fn a_locator_the_core_refuses_is_said_in_the_dialog() {
    let mut screen = loaded(view(false, true));
    feature(&mut screen).enqueue_sidebar(PullRequestIntent::OpenLink(thread()));
    screen.run_steps(2);
    type_into_link_dialog(&mut screen, "not a pr");
    screen.get_by_role_and_label(Role::Button, "Link").click();
    let sent = asked(&mut screen);
    let PullRequestsCommand::LinkPullRequest(link) = pr_command(&sent[0]).clone() else {
        panic!("a link was expected");
    };
    let mut failed = operation_view(
        &link.operation_id,
        PullRequestOperationKind::Link,
        OperationState::Failed {
            code: ErrorCode::InvalidRequest,
        },
    );
    failed.message = Some("not a github.com pull request".into());
    answer(&mut screen, sent[0].tag, command_reply(failed));
    screen.get_by_label("Link pull request");
    screen.get_by_label("not a github.com pull request");
    assert!(link_button(&screen), "the person may correct it");
}

#[test]
fn a_link_chosen_for_another_thread_waits_for_its_read_and_quotes_its_revision() {
    let mut screen = loaded(view(false, true));
    feature(&mut screen).enqueue_sidebar(PullRequestIntent::OpenLink(id("t2")));
    let read = asked(&mut screen);
    assert_eq!(read.len(), 1);
    assert!(is_read_of(&read[0], "t2"));
    type_into_link_dialog(&mut screen, "o/r#7");
    screen.get_by_label("Reading the thread's pull request…");
    assert!(
        !link_button(&screen),
        "the revision to quote is not read yet"
    );
    let mut other = view_of("t2", false, true);
    other.lifecycle_revision = Revision(8);
    answer(&mut screen, read[0].tag, thread_reply(other));
    assert!(link_button(&screen));
    screen.get_by_role_and_label(Role::Button, "Link").click();
    let sent = asked(&mut screen);
    let PullRequestsCommand::LinkPullRequest(link) = pr_command(&sent[0]) else {
        panic!("a link was expected");
    };
    assert_eq!(link.thread.as_str(), "t2");
    assert_eq!(link.expected_revision, Revision(8));
}

fn linked_with(shape: impl FnOnce(&mut AutoFixView)) -> ThreadPullRequest {
    let mut view = failing(view(true, true));
    shape(view.auto_fix.as_mut().unwrap());
    view
}

#[test]
fn a_reset_that_leaves_the_destination_alone_confirms_nothing() {
    let mut screen = loaded(linked_with(|auto| *auto = auto_fix_here(true)));
    screen.get_by_label("Limit reached 3/3");
    screen.get_by_label("Reset count").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let PullRequestsCommand::ResetAutoFixBudget(reset) = pr_command(&sent[0]) else {
        panic!("a reset was expected");
    };
    assert_eq!(reset.confirmed_previous, None);
    assert_eq!(reset.policy_revision, Revision(9));
}

#[test]
fn a_reset_never_moves_the_destination_without_the_person_confirming_it() {
    let mut screen = loaded(linked_with(|auto| {
        *auto = auto_fix_here(true);
        auto.destination = Some(id("t-other"));
    }));
    screen.get_by_label("Reset count").click();
    assert!(
        asked(&mut screen).is_empty(),
        "reading the destination is not confirming it"
    );
    screen.get_by_label("Move auto fix here?");
    screen.get_by_label_contains("\u{201c}t-other\u{201d}");
    screen.get_by_label("Cancel").click();
    screen.run_steps(2);
    assert!(asked(&mut screen).is_empty());
    assert!(screen.query_by_label("Move auto fix here?").is_none());

    screen.get_by_label("Reset count").click();
    screen.run_steps(2);
    screen.get_by_label(MOVE_LABEL).click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let PullRequestsCommand::ResetAutoFixBudget(reset) = pr_command(&sent[0]) else {
        panic!("a reset was expected");
    };
    assert_eq!(
        reset.confirmed_previous.as_ref().map(ThreadId::as_str),
        Some("t-other")
    );
}

#[test]
fn taking_over_the_notices_names_the_thread_it_takes_them_from() {
    let mut screen = loaded(linked_with(|auto| auto.destination = Some(id("t-other"))));
    screen.get_by_label("On in another thread");
    screen.get_by_label("Auto fix").click();
    assert!(asked(&mut screen).is_empty());
    screen.get_by_label(MOVE_LABEL).click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let PullRequestsCommand::ChangeAutoFixDestination(change) = pr_command(&sent[0]) else {
        panic!("a destination change was expected");
    };
    assert_eq!(
        change.confirmed_previous.as_ref().map(ThreadId::as_str),
        Some("t-other")
    );
    assert_eq!(change.link_revision, Revision(4));
}

#[test]
fn the_toggle_turns_auto_fix_off_here_and_on_again() {
    let mut screen = loaded(failing(view(true, true)));
    screen.get_by_label("2 left");
    screen.get_by_label("Auto fix").click();
    let sent = asked(&mut screen);
    let PullRequestsCommand::SetAutoFix(off) = pr_command(&sent[0]) else {
        panic!("a switch was expected");
    };
    assert!(!off.enabled);
    assert_eq!(
        (off.link_revision, off.policy_revision),
        (Revision(4), Revision(9))
    );

    let mut screen = loaded(linked_with(|auto| {
        auto.enabled = false;
        auto.destination = None;
    }));
    screen.get_by_label("Auto fix").click();
    let sent = asked(&mut screen);
    let PullRequestsCommand::SetAutoFix(on) = pr_command(&sent[0]) else {
        panic!("a switch was expected");
    };
    assert!(on.enabled);
}

#[test]
fn send_to_agent_quotes_the_failing_head_and_the_link() {
    let mut screen = loaded(failing(view(true, true)));
    screen.get_by_label("Send to agent").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let PullRequestsCommand::RequestManualNotice(notice) = pr_command(&sent[0]) else {
        panic!("a manual notice was expected");
    };
    assert_eq!(notice.expected_head, HEAD);
    assert_eq!(notice.link_revision, Revision(4));
}

#[test]
fn the_bar_is_offered_only_for_the_selected_linked_thread() {
    let mut screen = loaded(view(false, true));
    assert!(feature(&mut screen).bar().is_none(), "nothing is linked");
    feature(&mut screen).enqueue_sidebar(PullRequestIntent::Want(id("t2")));
    let read = asked(&mut screen);
    answer(
        &mut screen,
        read[0].tag,
        thread_reply(view_of("t2", true, true)),
    );
    assert!(
        feature(&mut screen).bar().is_none(),
        "another thread's pull request is not this conversation's"
    );
    assert_eq!(
        feature(&mut screen).menu().badge(&id("t2")),
        Some(PullRequestBadge {
            number: 7,
            glyph: PrGlyph::Passing,
        })
    );
}

#[test]
fn a_move_into_failure_expands_the_bar_once() {
    let mut screen = loaded(view(true, true));
    let expanded =
        |screen: &mut Harness<'static, Screen>| feature(screen).bar().unwrap().view.expanded;
    assert!(!expanded(&mut screen));
    let refresh = |screen: &mut Harness<'static, Screen>, view: ThreadPullRequest| {
        feature(screen).event(&UiEvent::PullRequests(
            PullRequestsEvent::PullRequestChanged {
                thread: thread(),
                pull_request: reference(),
                change: PullRequestChange::FactsChanged,
                link_revision: Revision(4),
                transition_seq: Sequence(2),
                notifications: vec![],
            },
        ));
        let read = asked(screen);
        assert_eq!(read.len(), 1);
        answer(screen, read[0].tag, thread_reply(view));
    };
    refresh(&mut screen, failing(view(true, true)));
    assert!(expanded(&mut screen), "a move into failure expands the bar");
    let toggle = feature(&mut screen)
        .bar()
        .unwrap()
        .action(BarChoice::ToggleExpanded);
    feature(&mut screen).enqueue_bar(toggle);
    screen.run_steps(2);
    assert!(!expanded(&mut screen));
    refresh(&mut screen, failing(view(true, true)));
    assert!(
        !expanded(&mut screen),
        "a re-poll of the same failure keeps the bar the person closed"
    );
}

#[test]
fn a_change_of_a_thread_not_selected_reads_it_again() {
    let mut screen = loaded(view(true, true));
    feature(&mut screen).enqueue_sidebar(PullRequestIntent::Want(id("t2")));
    let read = asked(&mut screen);
    answer(
        &mut screen,
        read[0].tag,
        thread_reply(view_of("t2", true, true)),
    );
    assert!(asked(&mut screen).is_empty());
    feature(&mut screen).event(&UiEvent::PullRequests(
        PullRequestsEvent::PullRequestChanged {
            thread: id("t2"),
            pull_request: reference(),
            change: PullRequestChange::FactsChanged,
            link_revision: Revision(4),
            transition_seq: Sequence(2),
            notifications: vec!["merged".into()],
        },
    ));
    let again = asked(&mut screen);
    assert_eq!(again.len(), 1);
    assert!(is_read_of(&again[0], "t2"));
    assert!(
        screen.query_by_label("o/r#7 merged").is_none(),
        "another thread's notice is not this tab's"
    );
}

#[test]
fn rows_are_read_once_and_at_most_four_at_a_time() {
    let mut screen = screen(Box::<PullRequestsFeature>::default(), None);
    for name in ["t2", "t3", "t4", "t5", "t6", "t2", "t3"] {
        feature(&mut screen).enqueue_sidebar(PullRequestIntent::Want(id(name)));
    }
    let first = asked(&mut screen);
    assert_eq!(first.len(), MAX_IN_FLIGHT);
    assert!(
        asked(&mut screen).is_empty(),
        "nothing more while four are out"
    );
    answer(
        &mut screen,
        first[0].tag,
        thread_reply(view_of("t2", false, true)),
    );
    let next = asked(&mut screen);
    assert_eq!(next.len(), 1);
    assert!(is_read_of(&next[0], "t6"));
    feature(&mut screen).enqueue_sidebar(PullRequestIntent::Want(id("t2")));
    assert!(
        asked(&mut screen).is_empty(),
        "a row read once is not read again"
    );
}

fn maintained_only(thread: &str) -> Harness<'static, Screen> {
    let state = Screen {
        feature: Box::<PullRequestsFeature>::default(),
        selection: crate::core_client::features::Selection {
            project: Some(workbench_protocol::ids::ProjectId::new("p").unwrap()),
            thread: Some(id(thread)),
        },
        cache: crate::core_client::cache::Cache::default(),
        frame: FeatureFrame::default(),
    };
    Harness::new_ui_state(
        |_ui, screen: &mut Screen| {
            let capabilities = [Capability::Queries, Capability::Commands];
            let view = FeatureView {
                connected: true,
                capabilities: &capabilities,
                cache: &screen.cache,
                selection: &screen.selection,
            };
            let mut ctx = FeatureContext {
                data: &mut crate::core_client::features::data::DataInbox::default(),
                feature: screen.feature.id(),
                view,
                frame: &mut screen.frame,
            };
            let visibility = Visibility {
                available: true,
                expanded: false,
                minimized: false,
            };
            screen.feature.maintain(&mut ctx, visibility);
        },
        state,
    )
}

#[test]
fn the_selected_thread_is_read_while_the_tab_is_hidden() {
    let mut screen = maintained_only(THREAD);
    let read = asked(&mut screen);
    assert_eq!(read.len(), 1);
    assert!(is_read_of(&read[0], THREAD));
    answer(
        &mut screen,
        read[0].tag,
        thread_reply(failing(view(true, true))),
    );
    let (bar_thread, bar) = feature(&mut screen)
        .bar()
        .map(|bar| (bar.thread.clone(), bar.view.ci))
        .unwrap();
    assert_eq!(bar_thread, thread());
    assert_eq!(bar, "CI failing 1/2");
    let refresh = feature(&mut screen)
        .bar()
        .unwrap()
        .action(BarChoice::Refresh);
    feature(&mut screen).enqueue_bar(refresh);
    let sent = asked(&mut screen);
    assert!(matches!(
        pr_command(&sent[0]),
        PullRequestsCommand::RefreshPullRequest(refresh) if refresh.thread == thread()
    ));
}

#[test]
fn candidates_are_offered_by_their_label_and_linked_at_the_revision_read() {
    let mut view = view(false, true);
    view.candidates = vec![
        workbench_protocol::namespaces::pull_requests::CandidateView {
            candidate_id: 5,
            pull_request: reference(),
            state: CandidateState::Presented,
            source_run_id: workbench_protocol::ids::RunId::new("run-1").unwrap(),
            source_call_id: "call-1".into(),
        },
    ];
    let mut screen = loaded(view);
    screen.get_by_label("Suggested pull requests");
    screen.get_by_label("Link PR o/r#7").click();
    let sent = asked(&mut screen);
    let PullRequestsCommand::ConfirmPullRequestCandidate(confirm) = pr_command(&sent[0]) else {
        panic!("a confirmation was expected");
    };
    assert_eq!(confirm.candidate_id, 5);
    assert_eq!(confirm.expected_revision, Revision(3));
}

const STORE_CLOSED: &str = "the event store is not open";

fn read_failure() -> FeatureReply {
    FeatureReply::Query(Err(
        FeatureError::new(STORE_CLOSED).with_code(ErrorCode::StorageUnavailable)
    ))
}

#[test]
fn a_failed_read_of_the_selected_thread_is_said_instead_of_a_spinner() {
    let mut screen = screen(Box::<PullRequestsFeature>::default(), Some(THREAD));
    let first = asked(&mut screen);
    assert!(is_read_of(&first[0], THREAD));
    answer(&mut screen, first[0].tag, read_failure());
    screen.get_by_label(STORE_CLOSED);
    assert!(
        screen.query_by_label("Reading the pull request…").is_none(),
        "the read is over"
    );
    assert!(asked(&mut screen).is_empty(), "nothing is read in a loop");
}

#[test]
fn a_failed_read_for_the_link_dialog_is_said_there_and_read_again_on_reopening() {
    let mut screen = loaded(view(false, true));
    feature(&mut screen).enqueue_sidebar(PullRequestIntent::OpenLink(id("t2")));
    let read = asked(&mut screen);
    assert!(is_read_of(&read[0], "t2"));
    answer(&mut screen, read[0].tag, read_failure());
    screen.get_by_label("Link pull request");
    screen.get_by_label(STORE_CLOSED);
    assert!(
        screen
            .query_by_label("Reading the thread's pull request…")
            .is_none()
    );
    assert!(!link_button(&screen));
    assert!(asked(&mut screen).is_empty(), "nothing is read in a loop");

    feature(&mut screen).enqueue_sidebar(PullRequestIntent::OpenLink(id("t2")));
    let again = asked(&mut screen);
    assert_eq!(again.len(), 1);
    assert!(is_read_of(&again[0], "t2"));
    answer(
        &mut screen,
        again[0].tag,
        thread_reply(view_of("t2", false, true)),
    );
    type_into_link_dialog(&mut screen, "o/r#7");
    assert!(link_button(&screen));
}

#[test]
fn a_link_of_another_thread_is_not_reported_on_the_selected_thread() {
    let mut screen = loaded(view(false, true));
    feature(&mut screen).enqueue_sidebar(PullRequestIntent::OpenLink(id("t2")));
    let read = asked(&mut screen);
    answer(
        &mut screen,
        read[0].tag,
        thread_reply(view_of("t2", false, true)),
    );
    type_into_link_dialog(&mut screen, "o/r#7");
    screen.get_by_role_and_label(Role::Button, "Link").click();
    let sent = asked(&mut screen);
    let PullRequestsCommand::LinkPullRequest(link) = pr_command(&sent[0]).clone() else {
        panic!("a link was expected");
    };
    let mut done = operation_view(
        &link.operation_id,
        PullRequestOperationKind::Link,
        OperationState::Ready,
    );
    done.thread = id("t2");
    answer(&mut screen, sent[0].tag, command_reply(done));
    assert!(screen.query_by_label("Link pull request").is_none());
    screen.get_by_label("No pull request is linked to this thread.");
    assert!(
        screen.query_by_label("Pull request linked.").is_none(),
        "t1's tab says nothing of t2's link"
    );
}

fn changed(thread: &str) -> UiEvent {
    UiEvent::PullRequests(PullRequestsEvent::PullRequestChanged {
        thread: id(thread),
        pull_request: reference(),
        change: PullRequestChange::FactsChanged,
        link_revision: Revision(4),
        transition_seq: Sequence(2),
        notifications: vec![],
    })
}

#[test]
fn a_failed_read_again_marks_what_is_shown_out_of_date_and_acts_on_none_of_it() {
    let mut screen = loaded(view(true, true));
    let press = feature(&mut screen)
        .bar()
        .unwrap()
        .action(BarChoice::Refresh);
    feature(&mut screen).event(&changed(THREAD));
    let read = asked(&mut screen);
    assert_eq!(read.len(), 1);
    assert!(is_read_of(&read[0], THREAD));
    answer(&mut screen, read[0].tag, read_failure());

    screen.get_by_label_contains(STORE_CLOSED);
    assert!(screen.get_by_label("Unlink").accesskit_node().is_disabled());
    let bar = feature(&mut screen).bar().unwrap();
    assert!(!bar.view.actions_enabled);
    assert!(bar.view.statuses.contains(&Status::ReadFailed {
        message: STORE_CLOSED.into()
    }));
    feature(&mut screen).enqueue_bar(press);
    assert!(
        asked(&mut screen).is_empty(),
        "nothing is sent from a view known to be out of date"
    );

    feature(&mut screen).enqueue_sidebar(PullRequestIntent::OpenLink(thread()));
    let again = asked(&mut screen);
    assert_eq!(again.len(), 1, "opening the dialog reads the thread again");
    answer(&mut screen, again[0].tag, read_failure());
    screen.get_by_label("Link pull request");
    screen.get_by_label(STORE_CLOSED);
    assert!(!link_button(&screen));
}

#[test]
fn a_bar_press_is_dropped_when_a_newer_read_lands_before_it_is_applied() {
    let mut screen = loaded(failing(view(true, true)));
    let press = feature(&mut screen)
        .bar()
        .unwrap()
        .action(BarChoice::ToggleAutoFix);
    feature(&mut screen).event(&changed(THREAD));
    let read = asked(&mut screen);
    assert_eq!(read.len(), 1);
    answer(
        &mut screen,
        read[0].tag,
        thread_reply(linked_with(|auto| {
            auto.enabled = false;
            auto.policy_revision = Revision(10);
        })),
    );
    feature(&mut screen).enqueue_bar(press);
    assert!(
        asked(&mut screen).is_empty(),
        "turning auto fix off is never sent as turning it on"
    );
    screen.get_by_label(failure_label(ErrorCode::StaleRevision, false));
}
