//! The side question's wire contract: what a question may hold and which
//! states may carry an answer or a failure.
use workbench_protocol::{
    errors::ErrorCode,
    ids::{OperationId, ThreadId},
    namespaces::execution::{AskBtw, BtwView},
    operation::{OperationState, OperationView},
    wire::Revision,
};

fn ask(question: &str) -> AskBtw {
    AskBtw {
        operation_id: OperationId::new("op-1").unwrap(),
        thread: ThreadId::new("thread-1").unwrap(),
        question: question.to_owned(),
    }
}

fn view(state: OperationState) -> BtwView {
    BtwView {
        operation: OperationView {
            operation_id: OperationId::new("op-1").unwrap(),
            state,
            revision: Revision(1),
            result: None,
        },
        thread: ThreadId::new("thread-1").unwrap(),
        source: None,
        answer: None,
        truncated: false,
        failure: None,
    }
}

#[test]
fn a_question_must_say_something_and_stay_plain_text() {
    assert!(ask("why?").validate().is_ok());
    assert!(ask("").validate().is_err());
    assert!(ask(" \n\t ").validate().is_err());
    assert!(ask("bell\u{7}").validate().is_err());
    assert!(ask(&"a".repeat(256 * 1024 + 1)).validate().is_err());
}

#[test]
fn only_a_ready_question_has_an_answer_and_only_a_failed_one_a_failure() {
    let mut pending = view(OperationState::Pending);
    pending.answer = Some("text".to_owned());
    assert!(pending.validate().is_err());

    let mut ready = view(OperationState::Ready);
    ready.answer = Some("text".to_owned());
    assert!(ready.validate().is_ok());
    ready.failure = Some("boom".to_owned());
    assert!(ready.validate().is_err());

    let mut failed = view(OperationState::Failed {
        code: ErrorCode::Internal,
    });
    failed.failure = Some("boom".to_owned());
    assert!(failed.validate().is_ok());
    failed.answer = Some("text".to_owned());
    assert!(failed.validate().is_err());
}
