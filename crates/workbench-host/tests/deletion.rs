//! Deleting a conversation removes what the host knew about its messages: a deleted
//! run's submission is not answered from memory or storage, immediately, after the
//! rows are physically reclaimed, and on a host created afterwards.

mod common;

use std::path::Path;

use common::{host_with_project, runtime_on_store, runtime_with_store, send, web};
use rusqlite::Connection;
use workbench_host::commands::{ChatInput, ComposerRole, HostCommand, HostEvent, RejectKind};
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{AnswerUserQuestion, Command, CommandDisposition, CommandReply};
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::ids::{QuestionId, ThreadId};
use workbench_protocol::wire::Revision;

fn database(dir: &Path) -> Connection {
    Connection::open(dir.join("host.db")).unwrap()
}

/// What thread deletion does first: fence the run.
fn tombstone(dir: &Path, run: &str) {
    database(dir)
        .execute(
            "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES (?1, 'thread-1', 'f')",
            [run],
        )
        .unwrap();
}

/// What the reclaim does next: remove the run's rows, keeping the fence.
fn reclaim(dir: &Path, run: &str) {
    let conn = database(dir);
    for table in ["host_chat_submissions", "host_input_receipts"] {
        conn.execute(&format!("DELETE FROM {table} WHERE run_id = ?1"), [run])
            .unwrap();
    }
}

fn local_chat(root: &Path, text: &str) -> ChatInput {
    ChatInput {
        submission_id: "sub-1".into(),
        project_root: Some(root.to_path_buf()),
        composer_role: ComposerRole::Worker,
        images: Vec::new(),
        thread_id: "thread-1".into(),
        text: text.into(),
        skills: Vec::new(),
        model_preference: None,
        fork_seed: None,
    }
}

fn run_of(reply: &CommandReply) -> String {
    reply.run_id.as_ref().unwrap().to_string()
}

#[test]
fn a_deleted_conversations_message_is_not_answered_from_memory_or_storage() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, storage) = runtime_with_store(dir.path());
    let mut host = host_with_project(&rt, runtime, dir.path());
    let accepted = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    assert_eq!(host.submission_provenance("sub-1"), Some(web()));

    tombstone(dir.path(), &run_of(&accepted));

    assert_eq!(host.submission_provenance("sub-1"), None);
    let again = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap_err();
    let desktop = host.submit_local(
        HostCommand::SendChat(local_chat(dir.path(), "hello")),
        &local_desktop(),
    );
    assert_eq!(again.code, ErrorCode::Conflict);
    assert!(matches!(
        desktop.as_slice(),
        [HostEvent::ChatRejected {
            kind: RejectKind::Conflict,
            ..
        }]
    ));
    assert!(
        host.receipts().submission("sub-1").is_none(),
        "the in-memory record was dropped"
    );

    let mut recreated = host_with_project(&rt, runtime_on_store(dir.path(), &storage), dir.path());
    assert_eq!(recreated.submission_provenance("sub-1"), None);
    let error = recreated
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Conflict);
}

#[test]
fn after_the_rows_are_reclaimed_nothing_about_the_message_comes_back() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, storage) = runtime_with_store(dir.path());
    let mut host = host_with_project(&rt, runtime, dir.path());
    let accepted = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    let original = run_of(&accepted);
    tombstone(dir.path(), &original);
    reclaim(dir.path(), &original);

    // The host that remembered the message still refuses to answer for it.
    assert_eq!(host.submission_provenance("sub-1"), None);
    let same_host = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap_err();
    assert_eq!(same_host.code, ErrorCode::Conflict);

    // A host created afterwards has nothing to remember, so the id is new: it is never
    // answered with the deleted run.
    let mut recreated = host_with_project(&rt, runtime_on_store(dir.path(), &storage), dir.path());
    assert_eq!(recreated.submission_provenance("sub-1"), None);
    match recreated.handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web()) {
        Ok(reply) => {
            assert_eq!(reply.disposition, CommandDisposition::Accepted);
            assert_ne!(run_of(&reply), original);
        }
        Err(error) => assert_ne!(error.code, ErrorCode::NotFound, "{error}"),
    }
}

#[test]
fn a_deleted_questions_answer_provenance_is_gone() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, storage) = runtime_with_store(dir.path());
    let question = common::offline_question(&rt, dir.path(), &storage);
    let mut host = host_with_project(&rt, runtime, dir.path());
    host.handle_command(
        Command::AnswerUserQuestion(AnswerUserQuestion {
            thread: ThreadId::new("thread-1").unwrap(),
            question_id: QuestionId::new(question.id.clone()).unwrap(),
            answer: "JSON".into(),
            question_revision: Revision(0),
        }),
        web(),
    )
    .unwrap();
    assert_eq!(host.answer_provenance(&question.id), Some(web()));

    let conn = database(dir.path());
    conn.execute(
        "DELETE FROM user_question_links WHERE question_id = ?1",
        [&question.id],
    )
    .unwrap();
    conn.execute("DELETE FROM user_questions WHERE id = ?1", [&question.id])
        .unwrap();

    assert_eq!(host.answer_provenance(&question.id), None);
}
