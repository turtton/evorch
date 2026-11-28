//! `/btw` and `/run` in the composer: the side question's card above it, and a
//! background Worker that leaves the screen where it is.
use egui::Key;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use workbench_protocol::commands::{Command, ComposerRole};
use workbench_protocol::ids::OperationId;
use workbench_protocol::namespaces::execution::{AskBtw, BtwView};
use workbench_protocol::namespaces::{
    ConversationCommand, ExecutionCommand, ExecutionQuery, ExecutionReply,
};

use super::*;

fn composer() -> Screen {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen
}

fn btw_view(operation: &OperationId, state: &str, answer: Option<&str>, settled: bool) -> BtwView {
    view(json!({
        "operation": {
            "operation_id": operation.as_str(),
            "state": {"type": state},
            "revision": "2",
        },
        "thread": "thr-1",
        "source": {"run": "run-1", "settled": settled},
        "answer": answer,
        "truncated": false,
    }))
}

impl Screen {
    fn send_text(&mut self, text: &str) {
        self.harness.get_by_label("Message").focus();
        self.harness.run_steps(2);
        self.harness.get_by_label("Message").type_text(text);
        self.harness.run_steps(2);
        self.harness.key_press(Key::Enter);
        self.harness.run_steps(2);
    }

    /// What the composer sent, with the answer of every side question read `answer`
    /// gives for the operation it names. The reads are returned too.
    fn drain_btw(
        &mut self,
        answer: &dyn Fn(&OperationId) -> Option<BtwView>,
    ) -> (Vec<Action>, Vec<FeatureRequest>) {
        let mut others = Vec::new();
        let mut features = Vec::new();
        for action in self.settle() {
            let Action::Feature(FeatureAction {
                feature: FeatureId::Execution,
                tag,
                request,
            }) = action
            else {
                others.push(action);
                continue;
            };
            if let FeatureRequest::Query(Query::Execution(ExecutionQuery::GetBtw(get))) = &request
                && let Some(view) = answer(&get.operation_id)
            {
                let reply = FeatureReply::Query(Ok(QueryReply::Execution(ExecutionReply::Btw(
                    Box::new(view),
                ))));
                self.shared
                    .lock()
                    .unwrap()
                    .features
                    .reply(FeatureId::Execution, tag, reply);
            }
            features.push(request);
        }
        self.harness.run_steps(3);
        (others, features)
    }
}

fn asked(actions: &[Action]) -> AskBtw {
    let [
        Action::Command {
            command: Command::Execution(ExecutionCommand::AskBtw(ask)),
            draft: Some((thread, _)),
        },
    ] = actions
    else {
        panic!("one side question, got {} actions", actions.len());
    };
    assert_eq!(thread, "thr-1");
    ask.clone()
}

// Given: thr-1's composer
// When: "/btw what changed?" is sent and the core answers with Markdown
// Then: AskBtw is sent for thr-1 with the draft tied to it, and the answer is drawn in
//       a card directly above the composer (not a popup); the answer is Markdown, so
//       its marks are not shown, and a card that is closed is gone
#[test]
fn a_side_question_is_answered_in_a_card_above_the_composer() {
    let mut screen = composer();
    screen.send_text("/btw what changed?");
    let sent = screen.drain_btw(&|operation| {
        Some(btw_view(
            operation,
            "ready",
            Some("**Result**: forty-two"),
            true,
        ))
    });
    let (actions, features) = sent;
    let ask = asked(&actions);
    assert_eq!(
        (ask.thread.as_str(), ask.question.as_str()),
        ("thr-1", "what changed?")
    );
    assert!(features.iter().any(|request| matches!(
        request,
        FeatureRequest::Query(Query::Execution(ExecutionQuery::GetBtw(get)))
            if get.operation_id == ask.operation_id
    )));

    let answer = screen.harness.get_by_label_contains("forty-two").rect();
    let composer = screen.harness.get_by_label("Message").rect();
    assert!(
        answer.max.y <= composer.min.y,
        "the answer {answer:?} sits above the composer {composer:?}"
    );
    assert!(
        screen
            .harness
            .query_by_label_contains("**Result**")
            .is_none(),
        "the marks of the Markdown are rendered, not shown"
    );
    screen.capture("btw-answer");

    screen.harness.get_by_label("Close side question").click();
    screen.harness.run_steps(3);
    assert!(
        screen
            .harness
            .query_by_label_contains("forty-two")
            .is_none()
    );
    let (actions, features) = screen.drain_btw(&|_| None);
    assert!(actions.is_empty(), "{} actions", actions.len());
    assert!(
        features.is_empty(),
        "a settled card needs no cancel: {features:?}"
    );
}

// Given: a side question the core has not answered yet
// When: another /btw is sent, then the card is closed
// Then: the second keeps its draft and says why; closing asks the core to cancel the
//       question and not the conversation's own Stop
#[test]
fn a_side_question_being_answered_is_closed_with_a_cancel() {
    let mut screen = composer();
    screen.send_text("/btw first?");
    let (actions, _) =
        screen.drain_btw(&|operation| Some(btw_view(operation, "pending", None, true)));
    let ask = asked(&actions);
    screen.accept("thr-1");
    screen.harness.get_by_label_contains("Asking");

    screen.send_text("/btw second?");
    screen.harness.get_by_label_contains("still being answered");
    let (actions, _) = screen.drain_btw(&|_| None);
    assert!(actions.is_empty(), "{} actions", actions.len());

    screen.harness.get_by_label("Close side question").click();
    screen.harness.run_steps(3);
    let (_, features) = screen.drain_btw(&|_| None);
    assert!(
        features.iter().any(|request| matches!(
            request,
            FeatureRequest::Command(Command::Execution(ExecutionCommand::CancelBtw(cancel)))
                if cancel.operation_id == ask.operation_id
        )),
        "{features:?}"
    );
    assert!(
        !features.iter().any(|request| matches!(
            request,
            FeatureRequest::Command(Command::Execution(ExecutionCommand::StopChat(_)))
        )),
        "{features:?}"
    );
}

// Given: thr-1, and then a new conversation
// When: "/run fix it" is sent from each
// Then: a Worker is created in a thread of its own with the draft tied to the one it
//       was typed in, and the screen stays on that thread
#[test]
fn a_background_run_creates_a_worker_thread_and_keeps_the_screen() {
    let mut screen = composer();
    screen.send_text("/run fix it");
    let (actions, _) = screen.drain_btw(&|_| None);
    let [
        Action::Command {
            command: Command::Conversation(ConversationCommand::CreateExecution(chat)),
            draft: Some((thread, _)),
        },
    ] = actions.as_slice()
    else {
        panic!("one run, got {} actions", actions.len());
    };
    assert_eq!(thread, "thr-1");
    assert_ne!(chat.thread.as_str(), "thr-1");
    assert_eq!(
        (
            chat.text.as_str(),
            chat.composer_role,
            chat.attachments.len()
        ),
        ("fix it", ComposerRole::Worker, 0)
    );
    let state = screen.harness.state();
    assert_eq!(state.selected.as_ref().map(ThreadId::as_str), Some("thr-1"));

    screen.accept("thr-1");
    screen.send_text("/new");
    screen.harness.get_by_label("What should we do in evorch?");
    let new_thread = screen.harness.state().selected.clone().unwrap();
    screen.send_text("/run again");
    let (actions, _) = screen.drain_btw(&|_| None);
    let runs: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            Action::Command {
                command: Command::Conversation(ConversationCommand::CreateExecution(chat)),
                ..
            } => Some(chat),
            _ => None,
        })
        .collect();
    assert_eq!(runs.len(), 1, "one run among {} actions", actions.len());
    assert_ne!(runs[0].thread, new_thread);
    assert_eq!(screen.harness.state().selected.as_ref(), Some(&new_thread));
    screen.harness.get_by_label("What should we do in evorch?");
}
