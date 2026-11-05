//! Submission ids that outlive the host's memory. The runtime's durable input
//! receipt records only the words of a message, so it cannot prove a resend is the
//! same message: an id found only there is refused, never treated as a match and
//! never applied a second time.

mod common;

use common::{host_with_project, runtime_on_store, runtime_with_store, send, web};
use workbench_host::receipts::Receipts;
use workbench_protocol::commands::Command;
use workbench_protocol::errors::ErrorCode;

fn unverifiable_resend_is_refused(message: workbench_protocol::commands::SendChat) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, storage) = runtime_with_store(dir.path());
    let mut first = host_with_project(&rt, runtime.clone(), dir.path());
    first
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    let agents = runtime.list_agents().len();

    // A new host on a second runtime over the same database: nothing in memory,
    // only the durable receipt.
    let restarted_runtime = runtime_on_store(dir.path(), &storage);
    let mut restarted = host_with_project(&rt, restarted_runtime.clone(), dir.path());
    let error = restarted
        .handle_command(Command::SendChat(message), web())
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::Conflict);
    assert_eq!(runtime.list_agents().len(), agents);
    assert!(restarted_runtime.list_agents().is_empty());
    assert!(restarted.submission_provenance("sub-1").is_none());
}

#[test]
fn after_a_restart_the_same_message_is_not_applied_again() {
    unverifiable_resend_is_refused(send("thread-1", "sub-1", "hello"));
}

#[test]
fn after_a_restart_a_message_that_differs_only_in_thread_is_refused() {
    unverifiable_resend_is_refused(send("thread-2", "sub-1", "hello"));
}

#[test]
fn after_a_restart_a_message_that_differs_in_its_words_or_skills_is_refused() {
    unverifiable_resend_is_refused(send("thread-1", "sub-1", "bye"));
    let mut with_skill = send("thread-1", "sub-1", "hello");
    with_skill.skills = vec!["review".into()];
    unverifiable_resend_is_refused(with_skill);
}

#[test]
fn an_id_dropped_from_a_full_ledger_falls_back_to_the_durable_receipt() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, _storage) = runtime_with_store(dir.path());
    let mut host = host_with_project(&rt, runtime.clone(), dir.path())
        .with_receipts(Receipts::with_capacity(1));
    host.handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    host.handle_command(Command::SendChat(send("thread-1", "sub-2", "more")), web())
        .unwrap();
    assert!(
        host.submission_provenance("sub-1").is_none(),
        "the first id was evicted"
    );
    let agents = runtime.list_agents().len();

    let error = host
        .handle_command(Command::SendChat(send("thread-2", "sub-1", "hello")), web())
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::Conflict);
    assert_eq!(runtime.list_agents().len(), agents);
}
