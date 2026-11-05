//! Submission ids that outlive the host's memory: the host stores a fingerprint of
//! every accepted chat, so after a restart, or once the bounded in-memory table has
//! dropped an id, an identical resend is answered with the original run and anything
//! else under that id is refused. An id with only the runtime's input receipt (the
//! words, no identity) cannot be compared and is refused too.

mod common;

use common::{
    differing_messages, host_with_project, host_with_two_projects, runtime_on_store,
    runtime_with_store, send, web,
};
use workbench_host::commands::{ChatInput, ComposerRole, HostCommand, HostEvent, RejectKind};
use workbench_host::provenance::local_desktop;
use workbench_host::receipts::Receipts;
use workbench_protocol::commands::{Command, CommandDisposition};
use workbench_protocol::errors::ErrorCode;

#[test]
fn after_a_restart_an_identical_resend_is_answered_with_the_original_run() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, storage) = runtime_with_store(dir.path());
    let mut first = host_with_project(&rt, runtime.clone(), dir.path());
    let accepted = first
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    let agents = runtime.list_agents().len();

    // A new host on a second runtime over the same database: nothing in memory.
    let restarted_runtime = runtime_on_store(dir.path(), &storage);
    let mut restarted = host_with_project(&rt, restarted_runtime.clone(), dir.path());
    let again = restarted
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();

    assert_eq!(again.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(again.run_id, accepted.run_id);
    assert_eq!(again.submission_id, accepted.submission_id);
    assert_eq!(runtime.list_agents().len(), agents);
    assert!(
        restarted_runtime.list_agents().is_empty(),
        "nothing was applied a second time"
    );
    assert_eq!(restarted.submission_provenance("sub-1"), Some(web()));
}

#[test]
fn after_a_restart_every_difference_in_the_message_is_a_conflict() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let (runtime, storage) = runtime_with_store(dir.path());
    let mut first = host_with_two_projects(&rt, runtime, dir.path(), elsewhere.path());
    first
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    let restarted_runtime = runtime_on_store(dir.path(), &storage);
    let mut restarted =
        host_with_two_projects(&rt, restarted_runtime.clone(), dir.path(), elsewhere.path());

    for (field, message) in differing_messages("thread-1", "sub-1", "hello") {
        let error = restarted
            .handle_command(Command::SendChat(message), web())
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict, "{field}");
    }

    assert!(restarted_runtime.list_agents().is_empty());
}

#[test]
fn an_id_dropped_from_a_full_ledger_is_still_told_apart_by_its_stored_fingerprint() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let (runtime, _storage) = runtime_with_store(dir.path());
    let mut host = host_with_two_projects(&rt, runtime.clone(), dir.path(), elsewhere.path())
        .with_receipts(Receipts::with_capacity(1));
    let accepted = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    host.handle_command(Command::SendChat(send("thread-1", "sub-2", "more")), web())
        .unwrap();
    assert!(
        host.receipts().submission("sub-1").is_none(),
        "the first id was evicted from memory"
    );
    let agents = runtime.list_agents().len();

    let again = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    for (field, message) in differing_messages("thread-1", "sub-1", "hello") {
        let error = host
            .handle_command(Command::SendChat(message), web())
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict, "{field}");
    }

    assert_eq!(again.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(again.run_id, accepted.run_id);
    assert_eq!(runtime.list_agents().len(), agents);
}

#[test]
fn the_desktop_door_gets_the_same_answer_after_a_restart() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, storage) = runtime_with_store(dir.path());
    let mut first = host_with_project(&rt, runtime, dir.path());
    let accepted = first
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    let restarted_runtime = runtime_on_store(dir.path(), &storage);
    let mut restarted = host_with_project(&rt, restarted_runtime.clone(), dir.path());
    let chat = |text: &str| ChatInput {
        submission_id: "sub-1".into(),
        project_root: Some(dir.path().to_path_buf()),
        composer_role: ComposerRole::Worker,
        images: Vec::new(),
        thread_id: "thread-1".into(),
        text: text.into(),
        skills: Vec::new(),
        model_preference: None,
        fork_seed: None,
    };

    let same = restarted.submit_local(HostCommand::SendChat(chat("hello")), &local_desktop());
    let different = restarted.submit_local(HostCommand::SendChat(chat("bye")), &local_desktop());

    assert!(matches!(
        same.as_slice(),
        [HostEvent::ChatAccepted { run_id, .. }]
            if Some(run_id.as_str()) == accepted.run_id.as_ref().map(|run| run.as_str())
    ));
    assert!(matches!(
        different.as_slice(),
        [HostEvent::ChatRejected {
            kind: RejectKind::Conflict,
            ..
        }]
    ));
    assert!(restarted_runtime.list_agents().is_empty());
}

#[test]
fn an_input_receipt_without_a_stored_fingerprint_cannot_be_compared_and_is_refused() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, storage) = runtime_with_store(dir.path());
    // Accepted by the runtime directly, as before fingerprints were stored.
    rt.block_on(async {
        runtime
            .delegate_chat(
                "thread-1",
                runtime::Role::Worker,
                "hello".into(),
                runtime::RunConfig {
                    initial_prompt_id: Some("sub-1".into()),
                    ..Default::default()
                },
            )
            .unwrap();
    });
    let restarted_runtime = runtime_on_store(dir.path(), &storage);
    let mut host = host_with_project(&rt, restarted_runtime.clone(), dir.path());

    let error = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::Conflict);
    assert!(restarted_runtime.list_agents().is_empty());
    assert!(host.submission_provenance("sub-1").is_none());
}

#[test]
fn a_fingerprint_that_could_not_be_stored_still_blocks_a_reapply_after_a_restart() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, storage) = runtime_with_store(dir.path());
    // Storing a fingerprint fails; the acceptance of the message itself does not.
    rusqlite::Connection::open(dir.path().join("host.db"))
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER injected_failure BEFORE INSERT ON host_chat_submissions \
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();
    let mut host = host_with_project(&rt, runtime.clone(), dir.path());
    let accepted = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    let same_host = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    assert_eq!(same_host.run_id, accepted.run_id);
    assert_eq!(same_host.disposition, CommandDisposition::AlreadyApplied);
    assert!(
        runtime
            .host_chat_submissions(&["sub-1".to_owned()])
            .unwrap()
            .is_empty(),
        "the failure left no stored fingerprint"
    );

    let restarted_runtime = runtime_on_store(dir.path(), &storage);
    let mut restarted = host_with_project(&rt, restarted_runtime.clone(), dir.path());
    let error = restarted
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::Conflict);
    assert!(
        restarted_runtime.list_agents().is_empty(),
        "nothing was applied a second time"
    );
}
