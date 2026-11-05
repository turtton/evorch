//! The submission ledger shared by both doors: what counts as the same message,
//! and that ownership is checked before the ledger is consulted.

mod common;

use common::{
    cli, differing_messages, host_with_project, host_with_two_projects, lost_ownership, runtime,
    send, web,
};
use workbench_host::commands::{ChatInput, ComposerRole, HostCommand, HostEvent, RejectKind};
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{Command, CommandDisposition};
use workbench_protocol::envelope::Surface;
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::ids::RunId;

fn accepted_run(events: &[HostEvent]) -> String {
    events
        .iter()
        .find_map(|event| match event {
            HostEvent::ChatAccepted { run_id, .. } => Some(run_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no run was accepted: {events:?}"))
}

#[test]
fn any_difference_in_a_resent_message_is_a_conflict() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let mut host = host_with_two_projects(&rt, runtime(), dir.path(), elsewhere.path());
    host.handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    let agents = host.service().runtime().list_agents().len();

    for (field, message) in differing_messages("thread-1", "sub-1", "hello") {
        let error = host
            .handle_command(Command::SendChat(message), cli())
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Conflict, "{field}");
    }

    assert_eq!(host.service().runtime().list_agents().len(), agents);
    assert_eq!(host.submission_provenance("sub-1"), Some(web()));
    let same = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), cli())
        .unwrap();
    assert_eq!(same.disposition, CommandDisposition::AlreadyApplied);
}

fn local_chat(root: &std::path::Path, submission: &str, text: &str) -> ChatInput {
    ChatInput {
        submission_id: submission.into(),
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

#[test]
fn a_message_sent_through_either_door_is_recognised_by_the_other() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (runtime, _storage) = common::runtime_with_store(dir.path());
    let mut host = host_with_project(&rt, runtime, dir.path());
    let desktop = local_desktop();

    let first = host.submit_local(
        HostCommand::SendChat(local_chat(dir.path(), "sub-1", "hello")),
        &desktop,
    );
    let from_protocol = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    let different = host.handle_command(Command::SendChat(send("thread-1", "sub-1", "bye")), web());
    let protocol_first = host
        .handle_command(Command::SendChat(send("thread-1", "sub-2", "more")), web())
        .unwrap();
    let from_local = host.submit_local(
        HostCommand::SendChat(local_chat(dir.path(), "sub-2", "more")),
        &desktop,
    );
    let local_different = host.submit_local(
        HostCommand::SendChat(local_chat(dir.path(), "sub-2", "other")),
        &desktop,
    );

    assert_eq!(
        from_protocol.disposition,
        CommandDisposition::AlreadyApplied
    );
    assert_eq!(
        from_protocol.run_id.as_ref().map(RunId::as_str),
        Some(accepted_run(&first).as_str())
    );
    assert_eq!(different.unwrap_err().code, ErrorCode::Conflict);
    assert_eq!(
        Some(accepted_run(&from_local)),
        protocol_first.run_id.map(|run| run.to_string())
    );
    assert!(matches!(
        local_different.as_slice(),
        [HostEvent::ChatRejected {
            kind: RejectKind::Conflict,
            ..
        }]
    ));
    assert_eq!(
        host.service().runtime().list_agents().len(),
        1,
        "one thread, one root run, however many doors"
    );
    let sender = host.submission_provenance("sub-1").unwrap();
    assert_eq!(sender.surface, Surface::Desktop);
    assert_eq!(host.submission_provenance("sub-2"), Some(web()));
}

#[test]
fn a_caller_that_lost_the_thread_learns_nothing_from_the_ledger() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    host.handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap();
    let lost = lost_ownership("thread-1");
    host.service_mut().set_ownership(Some(lost.owner.clone()));

    let protocol = host
        .handle_command(Command::SendChat(send("thread-1", "sub-1", "hello")), web())
        .unwrap_err();
    let local = host.submit_local(
        HostCommand::SendChat(local_chat(dir.path(), "sub-1", "hello")),
        &local_desktop(),
    );
    let with_stale_permit = host.submit_local_chat_with_permit(
        local_chat(dir.path(), "sub-1", "hello"),
        lost.permit.clone(),
        &local_desktop(),
    );

    assert_eq!(protocol.code, ErrorCode::PermissionDenied);
    assert!(matches!(
        local.as_slice(),
        [HostEvent::CommandRejected {
            kind: RejectKind::Fenced,
            ..
        }]
    ));
    assert!(matches!(
        with_stale_permit.as_slice(),
        [HostEvent::ChatRejected {
            kind: RejectKind::Fenced,
            ..
        }]
    ));
}
