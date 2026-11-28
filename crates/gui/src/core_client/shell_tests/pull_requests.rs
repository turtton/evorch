use super::*;
use egui::accesskit::Role;
use egui_kittest::kittest::NodeT;
use workbench_protocol::ErrorCode;
use workbench_protocol::commands::{Command, CommandReply, CommandResult};
use workbench_protocol::namespaces::pull_requests::{
    AutoFixView, CandidateState, CandidateView, CheckView, FactsView, HeadAttribution, LinkOrigin,
    LinkView, PullRequestOperationKind, PullRequestOperationView, PullRequestRef, PullRequestState,
    PullRequestsCommand, PullRequestsQuery, PullRequestsReply, PullRequestsResult, ReviewDecision,
    SnapshotView, ThreadPullRequest,
};
use workbench_protocol::operation::{OperationState, OperationView};
use workbench_protocol::wire::UnixMillis;

use crate::panes::pull_request_widgets::INPUT_LABEL;

const LINKED: &str = "thr-1";
const HEAD: &str = "0123456789abcdef";
const INVALID_LOCATOR: &str = "not a pull request reference: o/r";

fn check(name: &str, conclusion: &str) -> CheckView {
    CheckView {
        name: name.into(),
        kind: "check_run".into(),
        status: "completed".into(),
        conclusion: Some(conclusion.into()),
    }
}

fn read_of(thread: &ThreadId, linked: bool) -> ThreadPullRequest {
    let now = crate::panes::pull_request_widgets::now_ns() / 1_000_000;
    ThreadPullRequest {
        thread: thread.clone(),
        tracking_enabled: true,
        lifecycle_revision: Revision(3),
        transition_seq: Sequence(1),
        link: linked.then(|| LinkView {
            pull_request: PullRequestRef {
                provider: "github".into(),
                host: "github.com".into(),
                repository: "o/r".into(),
                number: 7,
            },
            link_revision: Revision(4),
            origin: LinkOrigin::Explicit,
            linked_at: UnixMillis(0),
        }),
        local_head: Some(HEAD.into()),
        snapshot: linked.then(|| SnapshotView {
            facts: Some(FactsView {
                state: PullRequestState::Open,
                base_branch: "main".into(),
                head_repository: Some("o/r".into()),
                head_branch: "fix/login".into(),
                head_sha: HEAD.into(),
                checks: vec![check("lint", "success"), check("test", "failure")],
                checks_complete: true,
                review_decision: ReviewDecision::None,
            }),
            title: Some("Honour next on login".into()),
            fetched_at: Some(UnixMillis(u64::try_from(now).unwrap())),
            last_failure: None,
            consecutive_failures: 0,
        }),
        candidates: vec![],
        auto_fix: linked.then_some(AutoFixView {
            enabled: false,
            globally_enabled: true,
            destination: None,
            link_revision: Some(Revision(4)),
            policy_revision: Revision(9),
            used: 0,
            reserved: 0,
            limit: 3,
            cooldown_until: None,
            stop_reason: None,
            limit_reached: false,
        }),
        head_attribution: HeadAttribution::Agent,
    }
}

fn link_operation_ended_in(command: &PullRequestsCommand, state: OperationState) -> FeatureReply {
    let message = matches!(state, OperationState::Failed { .. }).then(|| INVALID_LOCATOR.into());
    FeatureReply::Command(Ok(CommandReply::with_result(CommandResult::PullRequests(
        PullRequestsResult::Operation(PullRequestOperationView {
            operation: OperationView {
                operation_id: command.operation_id().clone(),
                state,
                revision: Revision(1),
                result: None,
            },
            kind: PullRequestOperationKind::Link,
            thread: ThreadId::new("thr-2").unwrap(),
            lifecycle_revision: None,
            policy_revision: None,
            notice_id: None,
            message,
            outcome_unknown: false,
        }),
    ))))
}

type CommandAnswer<'a> = &'a dyn Fn(&PullRequestsCommand) -> Option<FeatureReply>;

impl Screen {
    fn settle_pull_requests(&mut self) -> Vec<PullRequestsCommand> {
        self.settle_pull_requests_replying_to_commands(&|_| None)
    }

    fn settle_pull_requests_replying_to_commands(
        &mut self,
        reply_to_command: CommandAnswer<'_>,
    ) -> Vec<PullRequestsCommand> {
        self.settle_pull_requests_reading(
            &|thread| read_of(thread, thread.as_str() == LINKED),
            reply_to_command,
        )
    }

    fn settle_pull_requests_reading(
        &mut self,
        read: &dyn Fn(&ThreadId) -> ThreadPullRequest,
        reply_to_command: CommandAnswer<'_>,
    ) -> Vec<PullRequestsCommand> {
        let mut commands = Vec::new();
        for _ in 0..16 {
            self.harness.run_steps(1);
            while let Ok(action) = self.actions.try_recv() {
                let Action::Feature(FeatureAction {
                    feature,
                    tag,
                    request,
                }) = action
                else {
                    continue;
                };
                let reply = match request {
                    FeatureRequest::Query(Query::Workspace(query)) => {
                        FeatureReply::Query(Ok(QueryReply::Workspace(answer(&query))))
                    }
                    FeatureRequest::Query(Query::PullRequests(
                        PullRequestsQuery::GetThreadPullRequest { thread },
                    )) => FeatureReply::Query(Ok(QueryReply::PullRequests(
                        PullRequestsReply::Thread(Box::new(read(&thread))),
                    ))),
                    FeatureRequest::Command(Command::PullRequests(command)) => {
                        let reply = reply_to_command(&command);
                        commands.push(command);
                        match reply {
                            Some(reply) => reply,
                            None => continue,
                        }
                    }
                    _ => continue,
                };
                let mut shared = self.shared.lock().unwrap();
                shared.features.reply(feature, tag, reply);
            }
        }
        commands
    }

    fn button_enabled(&self, label: &str) -> bool {
        !self
            .harness
            .get_by_role_and_label(Role::Button, label)
            .accesskit_node()
            .is_disabled()
    }

    fn capture_with_the_pointer_away(&mut self, suffix: &str) {
        self.harness
            .event(egui::Event::PointerMoved(egui::pos2(1.0, 899.0)));
        self.harness.run_steps(2);
        self.capture(suffix);
    }

    fn open_link_dialog_for_storage_writer(&mut self) {
        self.settle_pull_requests();
        self.harness
            .get_by_label("Refactor storage writer")
            .click_secondary();
        self.harness.run_steps(2);
        self.harness.get_by_label("Link PR…").click();
        self.settle_pull_requests();
    }

    fn type_locator(&mut self, text: &str) {
        self.harness
            .get_by_role_and_label(Role::TextInput, INPUT_LABEL)
            .focus();
        self.harness.run_steps(2);
        self.harness
            .get_by_role_and_label(Role::TextInput, INPUT_LABEL)
            .type_text(text);
        self.harness.run_steps(2);
    }
}

// Given: thr-1, selected, links o/r#7 whose test check failed
// When: the conversation is drawn, its bar expanded, and Refresh pressed
// Then: the bar above the composer names the pull request, its state and CI, expands
//       into the facts locally (nothing is sent), and Refresh asks the core to fetch
//       thr-1's pull request
#[test]
fn a_linked_threads_conversation_draws_its_bar_and_refreshes_it_through_the_core() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    assert!(screen.settle_pull_requests().is_empty());
    screen.harness.get_by_label("#7");
    screen.harness.get_by_label("Open");
    screen.harness.get_by_label("CI failing 1/2");
    screen.harness.get_by_label("Unlink");
    assert!(screen.harness.query_by_label("main ← fix/login").is_none());
    screen.harness.get_by_label("Expand pull request").click();
    assert!(screen.settle_pull_requests().is_empty());
    screen.harness.get_by_label("Collapse pull request");
    screen.harness.get_by_label("main ← fix/login");
    screen.harness.get_by_label_contains("test · failure");
    screen.capture_with_the_pointer_away("bar");
    screen.harness.get_by_label("Refresh").click();
    let commands = screen.settle_pull_requests();
    let [PullRequestsCommand::RefreshPullRequest(refresh)] = &commands[..] else {
        panic!("one refresh was expected: {commands:?}");
    };
    assert_eq!(refresh.thread.as_str(), LINKED);
}

// Given: thr-1's bar, with auto fix off and the failing head pushed by the agent
// When: Unlink, Send to agent or the Auto fix checkbox is pressed
// Then: each sends its one command, quoting the revisions and head the core gave
#[test]
fn the_bars_actions_send_their_commands_with_the_revisions_read() {
    for label in ["Unlink", "Send to agent", "Auto fix"] {
        let mut screen = screen(egui::vec2(1440.0, 900.0));
        screen.settle_pull_requests();
        assert!(
            label == "Auto fix" || screen.button_enabled(label),
            "{label}"
        );
        screen.harness.get_by_label(label).click();
        let commands = screen.settle_pull_requests();
        match (label, &commands[..]) {
            ("Unlink", [PullRequestsCommand::UnlinkPullRequest(unlink)]) => {
                assert_eq!(unlink.thread.as_str(), LINKED);
                assert_eq!(unlink.expected_revision, Revision(3));
            }
            ("Send to agent", [PullRequestsCommand::RequestManualNotice(send)]) => {
                assert_eq!(send.thread.as_str(), LINKED);
                assert_eq!(send.link_revision, Revision(4));
                assert_eq!(send.expected_head, HEAD);
            }
            ("Auto fix", [PullRequestsCommand::SetAutoFix(set)]) => {
                assert_eq!(set.thread.as_str(), LINKED);
                assert!(set.enabled);
                assert_eq!(set.link_revision, Revision(4));
                assert_eq!(set.policy_revision, Revision(9));
            }
            _ => panic!("{label} sent {commands:?}"),
        }
    }
}

#[test]
fn the_sidebar_row_shows_the_linked_pull_request_and_its_checks() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle_pull_requests();
    screen.harness.get_by_label("Pull request #7: failing");
    assert!(
        screen
            .harness
            .query_all_by_label_contains("Pull request #")
            .count()
            == 1,
        "only thr-1 is linked"
    );
}

#[test]
fn an_unlinked_threads_conversation_has_no_bar() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle_pull_requests();
    screen
        .harness
        .get_by_label("Refactor storage writer")
        .click();
    screen.settle_pull_requests();
    screen.harness.run_steps(2);
    assert!(screen.harness.query_by_label("CI failing 1/2").is_none());
    assert!(screen.harness.query_by_label("Unlink").is_none());
}

#[test]
fn link_pr_in_a_threads_menu_opens_the_dialog_in_the_pull_requests_tab() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.open_link_dialog_for_storage_writer();
    screen.harness.get_by_label("Link pull request");
    assert_eq!(
        screen.front_of_leaf(CoreTab::Feature(FeatureId::PullRequests)),
        CoreTab::Feature(FeatureId::PullRequests),
        "the Pull requests tab is in front"
    );
}

// Given: the "Link PR…" dialog opened from thr-2's menu
// When: a locator is typed and linked, the core refuses it, and a corrected one is linked
// Then: Link waits for text; the first link quotes thr-2's lifecycle revision and the
//       core's refusal is said in the dialog, which stays open; the second link closes it
//       without reporting it on the selected thr-1's tab
#[test]
fn the_link_dialog_sends_the_locator_says_the_cores_refusal_and_closes_once_linked() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.open_link_dialog_for_storage_writer();
    assert!(!screen.button_enabled("Link"), "nothing to link yet");

    screen.type_locator("o/r");
    assert!(screen.button_enabled("Link"));
    screen
        .harness
        .get_by_role_and_label(Role::Button, "Link")
        .click();
    let refused = |command: &PullRequestsCommand| {
        Some(link_operation_ended_in(
            command,
            OperationState::Failed {
                code: ErrorCode::InvalidRequest,
            },
        ))
    };
    let commands = screen.settle_pull_requests_replying_to_commands(&refused);
    let [PullRequestsCommand::LinkPullRequest(link)] = &commands[..] else {
        panic!("one link was expected: {commands:?}");
    };
    assert_eq!(link.thread.as_str(), "thr-2");
    assert_eq!(link.locator, "o/r");
    assert_eq!(link.expected_revision, Revision(3));
    screen.harness.get_by_label("Link pull request");
    screen.harness.get_by_label(INVALID_LOCATOR);
    screen.capture_with_the_pointer_away("link-dialog");

    screen.type_locator("#8");
    screen
        .harness
        .get_by_role_and_label(Role::Button, "Link")
        .click();
    let linked = |command: &PullRequestsCommand| {
        Some(link_operation_ended_in(command, OperationState::Ready))
    };
    let commands = screen.settle_pull_requests_replying_to_commands(&linked);
    let [PullRequestsCommand::LinkPullRequest(link)] = &commands[..] else {
        panic!("one link was expected: {commands:?}");
    };
    assert_eq!(link.locator, "o/r#8");
    assert!(screen.harness.query_by_label("Link pull request").is_none());
    assert!(
        screen
            .harness
            .query_by_label("Pull request linked.")
            .is_none(),
        "the selected thr-1's tab says nothing of thr-2's link"
    );
}

// Given: thr-1 selected and linked, then thr-2 selected, which links nothing
// When: the Pull requests tab is brought to the front
// Then: it shows thr-1's facts in English (state, branches, checks), then for thr-2
//       says nothing is linked and offers "Link PR…"
#[test]
fn the_pull_requests_tab_shows_the_facts_in_english() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle_pull_requests();
    shell::focus(
        &mut screen.harness.state_mut().dock,
        CoreTab::Feature(FeatureId::PullRequests),
    );
    screen.settle_pull_requests();
    for label in [
        "State",
        "main ← fix/login",
        "Review",
        "test · failure",
        "1 passed",
        "Honour next on login",
    ] {
        assert!(
            screen.harness.query_all_by_label_contains(label).count() > 0,
            "{label}"
        );
    }
    screen.capture_with_the_pointer_away("tab");

    screen
        .harness
        .get_by_label("Refactor storage writer")
        .click();
    screen.settle_pull_requests();
    screen
        .harness
        .get_by_label("No pull request is linked to this thread.");
    assert!(screen.button_enabled("Link PR…"));
}

// Given: thr-1 whose root's calls are a shell result and a change, with a pull request
//        suggested by the first (call-2 of run-1), one by a call of another run that has
//        the same call id, and one by a call that is not in the conversation
// When: the group of calls is opened, and the first's Link PR is pressed
// Then: only the first call's card offers its suggestion, pressing it sends that
//       suggestion's confirmation at the revision read and leaves the card closed
#[test]
fn a_suggestion_is_offered_on_the_card_of_the_call_that_made_it() {
    let candidate = |id: u64, run: &str, call: &str| CandidateView {
        candidate_id: id,
        pull_request: PullRequestRef {
            provider: "github".into(),
            host: "github.com".into(),
            repository: "o/r".into(),
            number: 40 + id,
        },
        state: CandidateState::Presented,
        source_run_id: RunId::new(run).unwrap(),
        source_call_id: call.into(),
    };
    let read = |thread: &ThreadId| {
        let mut read = read_of(thread, false);
        read.candidates = vec![
            candidate(1, "run-1", "call-2"),
            candidate(2, "run-2", "call-2"),
            candidate(3, "run-1", "call-99"),
        ];
        read
    };
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle_pull_requests_reading(&read, &|_| None);
    screen.harness.get_by_label("2 tool calls").click();
    screen.settle_pull_requests_reading(&read, &|_| None);
    screen.harness.get_by_label("Link PR o/r#41");
    screen.capture_with_the_pointer_away("suggestion");
    screen.harness.get_by_label("Link PR o/r#41").click();
    let commands = screen.settle_pull_requests_reading(&read, &|_| None);
    let [PullRequestsCommand::ConfirmPullRequestCandidate(confirm)] = &commands[..] else {
        panic!("one confirmation was expected: {commands:?}");
    };
    assert_eq!(confirm.candidate_id, 1);
    assert_eq!(confirm.expected_revision, Revision(3));
    assert!(
        screen.harness.query_by_label("Output").is_none(),
        "pressing a button on the card does not open it"
    );
    assert!(screen.harness.query_by_label("Link PR o/r#42").is_none());
    assert!(screen.harness.query_by_label("Link PR o/r#43").is_none());
}
