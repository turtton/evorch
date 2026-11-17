//! The pull request feature drawn headless: a command quotes the revision the
//! screen read, an outcome the screen did not see is never shown as done, and
//! nothing is sent twice.
use super::*;
use crate::core_client::features::execution::tests::{
    Screen, answer, asked, command_of, query_of, screen,
};
use crate::core_client::features::{FeatureError, FeatureReply};
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use workbench_protocol::{
    commands::CommandReply,
    namespaces::pull_requests::{AutoFixView, LinkOrigin, LinkView, PullRequestRef},
    wire::{Sequence, UnixMillis},
};

const THREAD: &str = "t1";

fn reference() -> PullRequestRef {
    PullRequestRef {
        provider: "github".into(),
        host: "github.com".into(),
        repository: "o/r".into(),
        number: 7,
    }
}

fn thread() -> ThreadId {
    ThreadId::new(THREAD).unwrap()
}

fn auto_fix(limit_reached: bool) -> AutoFixView {
    AutoFixView {
        enabled: true,
        globally_enabled: true,
        destination: None,
        link_revision: Some(Revision(4)),
        policy_revision: Revision(9),
        used: if limit_reached { 3 } else { 1 },
        reserved: 0,
        limit: 3,
        cooldown_until: None,
        stop_reason: None,
        limit_reached,
    }
}

fn view(linked: bool, owned_here: bool) -> ThreadPullRequest {
    ThreadPullRequest {
        thread: thread(),
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
        local_head: None,
        snapshot: None,
        candidates: vec![],
        auto_fix: linked.then(|| auto_fix(false)),
        head_attribution: HeadAttribution::Unknown,
    }
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

fn loaded(view: ThreadPullRequest) -> Harness<'static, Screen> {
    let mut screen = screen(Box::<PullRequestsFeature>::default(), Some(THREAD));
    let first = asked(&mut screen);
    assert_eq!(first.len(), 1);
    assert!(matches!(
        query_of(&first[0]),
        Query::PullRequests(PullRequestsQuery::GetThreadPullRequest { thread: t }) if *t == thread()
    ));
    answer(&mut screen, first[0].tag, thread_reply(view));
    screen
}

fn pr_command(action: &crate::core_client::features::FeatureAction) -> &PullRequestsCommand {
    let Command::PullRequests(command) = command_of(action) else {
        panic!("a pull request command was expected");
    };
    command
}

#[test]
fn unlink_quotes_the_lifecycle_revision_and_is_not_sent_twice() {
    let mut screen = loaded(view(true, true));
    screen.get_by_label("リンク解除").click();
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
    screen.get_by_label("実行中…");
    assert!(asked(&mut screen).is_empty());
}

#[test]
fn a_refused_command_says_it_is_stale_and_reads_the_thread_again() {
    let mut screen = loaded(view(true, true));
    screen.get_by_label("更新").click();
    let sent = asked(&mut screen);
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError {
            message: "stale".into(),
            outcome_unknown: false,
            code: Some(ErrorCode::StaleRevision),
        })),
    );
    screen.get_by_label(
        "他の場所で変更されました。最新の状態を読み込みました。もう一度操作してください。",
    );
    let again = asked(&mut screen);
    assert_eq!(again.len(), 1);
    assert!(matches!(
        query_of(&again[0]),
        Query::PullRequests(PullRequestsQuery::GetThreadPullRequest { .. })
    ));
}

#[test]
fn a_lost_reply_is_checked_by_operation_id_and_never_shown_as_done() {
    let mut screen = loaded(view(true, true));
    screen.get_by_label("リンク解除").click();
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
        })),
    );
    screen.get_by_label(
        "操作の結果を確認できませんでした。最新の状態を読み込み直して確認してください。",
    );
    assert!(screen.query_by_label("リンクを解除しました。").is_none());
    let check = asked(&mut screen);
    assert_eq!(check.len(), 1, "the operation is asked for, not resent");
    assert!(matches!(
        query_of(&check[0]),
        Query::PullRequests(PullRequestsQuery::GetPullRequestOperation { operation_id })
            if *operation_id == unlink.operation_id
    ));
    // The core restarted and no longer knows it: still not a success.
    answer(
        &mut screen,
        check[0].tag,
        FeatureReply::Query(Err(FeatureError {
            message: "gone".into(),
            outcome_unknown: false,
            code: Some(ErrorCode::NotFound),
        })),
    );
    screen.get_by_label(
        "操作の結果を確認できませんでした。最新の状態を読み込み直して確認してください。",
    );
    assert!(screen.query_by_label("リンクを解除しました。").is_none());
}

#[test]
fn a_thread_the_core_does_not_hold_is_read_only() {
    let screen = loaded(view(true, false));
    screen.get_by_label("このコアはこのスレッドを保持していないため、閲覧のみです。");
    assert!(
        screen
            .get_by_label("リンク解除")
            .accesskit_node()
            .is_disabled()
    );
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
    screen.get_by_label(
        "自動修正の上限（3 回）に達したため停止しました。再開するには予算をリセットしてください。",
    );
    // An event is a hint to read again, and nothing is sent by the screen itself.
    let sent = asked(&mut screen);
    assert!(sent.iter().all(|action| matches!(
        action.request,
        crate::core_client::features::FeatureRequest::Query(_)
    )));
}

#[test]
fn linking_sends_the_locator_with_the_revision_read() {
    let mut screen = loaded(view(false, true));
    screen.get_by_role(egui::accesskit::Role::TextInput).focus();
    screen.run_steps(4);
    screen
        .get_by_role(egui::accesskit::Role::TextInput)
        .type_text("o/r#7");
    screen.run_steps(4);
    screen.get_by_label("リンク").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let PullRequestsCommand::LinkPullRequest(link) = pr_command(&sent[0]) else {
        panic!("a link was expected");
    };
    assert_eq!(link.locator, "o/r#7");
    assert_eq!(link.expected_revision, Revision(3));
}

fn linked_with_destination(destination: &str) -> ThreadPullRequest {
    let mut view = view(true, true);
    view.auto_fix.as_mut().unwrap().destination = Some(ThreadId::new(destination).unwrap());
    view
}

#[test]
fn a_reset_that_leaves_the_destination_alone_confirms_nothing() {
    let mut view = view(true, true);
    view.auto_fix.as_mut().unwrap().destination = Some(ThreadId::new(THREAD).unwrap());
    let mut screen = loaded(view);
    screen.get_by_label("予算をリセット").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let PullRequestsCommand::ResetAutoFixBudget(reset) = pr_command(&sent[0]) else {
        panic!("a reset was expected");
    };
    assert_eq!(reset.confirmed_previous, None);
}

#[test]
fn a_reset_never_moves_the_destination_without_the_person_confirming_it() {
    let mut screen = loaded(linked_with_destination("t-other"));
    screen.get_by_label("予算をリセット").click();
    assert!(
        asked(&mut screen).is_empty(),
        "reading the destination is not confirming it"
    );
    screen.get_by_label_contains("t-other からこのスレッド");
    screen.get_by_label("やめる").click();
    screen.run_steps(2);
    assert!(asked(&mut screen).is_empty());

    screen.get_by_label("予算をリセット").click();
    screen.run_steps(2);
    screen.get_by_label("移す").click();
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
    let mut screen = loaded(linked_with_destination("t-other"));
    screen.get_by_label("通知先をこのスレッドにする").click();
    assert!(asked(&mut screen).is_empty());
    screen.get_by_label("移す").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let PullRequestsCommand::ChangeAutoFixDestination(change) = pr_command(&sent[0]) else {
        panic!("a destination change was expected");
    };
    assert_eq!(
        change.confirmed_previous.as_ref().map(ThreadId::as_str),
        Some("t-other")
    );
}
