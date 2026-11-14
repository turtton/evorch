mod common;

use base64::{Engine, engine::general_purpose::STANDARD};
use common::*;
use workbench_protocol::namespaces::ConversationCommand;
fn thread(value: &str) -> workbench_protocol::ids::ThreadId {
    workbench_protocol::ids::ThreadId::new(value).unwrap()
}
use std::{path::Path, sync::Arc};
use workbench_host::provenance::local_desktop;
use workbench_protocol::{
    commands::{Command, CommandDisposition, ComposerRole, ContinueChat},
    conversation::{ConversationCommand as Conversation, ConversationResult},
    errors::ErrorCode,
    ids::{RequestId, SessionId},
    queries::CommandReceiptState,
};

fn image() -> String {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        1,
        1,
        image::Rgba([0, 0, 0, 255]),
    ))
    .write_to(&mut bytes, image::ImageFormat::Png)
    .unwrap();
    STANDARD.encode(bytes.into_inner())
}
fn upload(data: String, mime_type: &str) -> Command {
    Command::Conversation(Conversation::UploadImage {
        project: project(),
        thread: thread("thread-1"),
        data,
        mime_type: mime_type.into(),
    })
}

#[test]
fn resolve_known_project_without_inventing_trust_or_registering_paths() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    let resolved = host
        .handle_command(
            Command::Conversation(Conversation::ResolveProject {
                root: dir.path().join(".").to_string_lossy().into_owned(),
            }),
            cli(),
        )
        .unwrap();
    assert_eq!(
        conversation_result(resolved),
        ConversationResult::ProjectResolved { project: project() }
    );
    for root in ["/not/a/project", "."] {
        assert!(
            host.handle_command(
                Command::Conversation(Conversation::ResolveProject { root: root.into() }),
                cli()
            )
            .is_err()
        );
    }
}

#[test]
fn validated_images_are_scoped_and_commands_are_correlatable_by_receipt() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    for (data, mime) in [
        (String::from("/etc/passwd"), "image/png"),
        (STANDARD.encode("not an image"), "image/png"),
        (image(), "image/jpeg"),
    ] {
        assert_eq!(
            host.handle_command(upload(data, mime), web())
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }
    let session = SessionId::new("desktop-session").unwrap();
    let request = RequestId::new("upload-1").unwrap();
    let reply = host
        .handle_request(
            &session,
            &request,
            upload(image(), "image/png"),
            local_desktop(),
        )
        .unwrap();
    assert!(matches!(
        host.command_receipt(&session, &request).unwrap().state,
        CommandReceiptState::Applied { .. }
    ));
    let ConversationResult::ImageUploaded(reference) = conversation_result(reply) else {
        panic!()
    };
    let mut chat = send("thread-1", "image-input", "");
    chat.images.push(reference.clone());
    let mut other = chat.clone();
    other.thread = thread("thread-2");
    assert_eq!(
        host.handle_command(
            Command::Conversation(ConversationCommand::SendChat(other)),
            web()
        )
        .unwrap_err()
        .code,
        ErrorCode::PermissionDenied
    );
    let first = host
        .handle_command(
            Command::Conversation(Conversation::CreateExecution(chat.clone())),
            local_desktop(),
        )
        .unwrap();
    let repeated = host
        .handle_command(
            Command::Conversation(ConversationCommand::SendChat(chat)),
            web(),
        )
        .unwrap();
    assert_eq!(repeated.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(repeated.run_id, first.run_id);
    assert_eq!(
        host.submission_provenance("image-input"),
        Some(local_desktop())
    );
    host.handle_command(
        Command::Conversation(Conversation::DiscardAttachment {
            attachment_id: reference.attachment_id,
        }),
        web(),
    )
    .unwrap();
}

fn save_access(path: &Path, root: &Path, trust: &str) {
    std::fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({
            "projects":[{"id":project(),"repo_root":root,"trust":trust}]
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn released_offline_legacy_thread_claims_canonically_and_restores_on_continue() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = test_socket_dir::SplitDir::new();
    let owners = test_socket_dir::SocketDir::new();
    let (_, store) = runtime_with_store(dir.path());
    let question = offline_question(&rt, dir.path(), &store);
    store
        .handle()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "thread-1".into(),
            project_root: Some(dir.path().to_string_lossy().into_owned()),
            root_run_id: Some(question.run_id.clone()),
            ..Default::default()
        }])
        .unwrap();
    let bus = Arc::new(event_bus::EventBus::new(64));
    let old = runtime::ownership::OwnerHost::open(owners.path(), Default::default(), bus.clone())
        .unwrap();
    let before = old.start("thread-1").unwrap();
    old.quiesce().unwrap();
    let successor = Arc::new(
        runtime::ownership::OwnerHost::open(owners.path(), Default::default(), bus).unwrap(),
    );
    let runtime = runtime_on_store(dir.path(), &store);
    let mut host = host_with_project(&rt, runtime.clone(), dir.path())
        .with_legacy_import(owners.path().into(), dir.path().join("sidebar.json"));
    host.service_mut().set_ownership(Some(successor.clone()));
    let prepare = || {
        Command::Conversation(Conversation::PrepareExistingThread {
            project: project(),
            thread: thread("thread-1"),
        })
    };
    let access = dir.path().join("sidebar.json");
    save_access(&access, dir.path(), "unapproved");
    assert_eq!(
        host.handle_command(prepare(), local_desktop())
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    save_access(&access, dir.path(), "approved");
    // Released is not enough: the old endpoint is still reachable.
    assert_eq!(
        host.handle_command(prepare(), local_desktop())
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    drop(old);
    host.handle_command(prepare(), local_desktop()).unwrap();
    let after = successor.owned_permit("thread-1").unwrap();
    assert_eq!(after.lease.generation, before.lease.generation + 1);
    let reply = host
        .handle_command(
            Command::Conversation(ConversationCommand::ContinueChat(ContinueChat {
                project: project(),
                thread: thread("thread-1"),
                composer_role: ComposerRole::Worker,
                model_preference: None,
            })),
            web(),
        )
        .unwrap();
    assert_eq!(reply.run_id.unwrap().as_str(), question.run_id);
    let run = question.run_id.parse().unwrap();
    runtime.cancel(run).unwrap();
    rt.block_on(runtime.wait(run)).unwrap();
    // Re-read trust rather than caching a previous successful import.
    save_access(&access, dir.path(), "unapproved");
    assert_eq!(
        host.handle_command(prepare(), local_desktop())
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
}

#[test]
fn new_team_goal_uses_the_desktop_delegation_meaning_without_a_synthetic_chat() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("team.db"),
        ..Default::default()
    };
    let store = storage::Storage::open(config.clone()).unwrap();
    let model = Arc::new(HeldModel::default());
    let bus = Arc::new(event_bus::EventBus::new(64));
    let runtime = runtime::AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        model.clone(),
    )
    .with_run_store(runtime::RunStore::open(&config, store.handle()).unwrap());
    let mut host = host_with_project(&rt, runtime.clone(), dir.path());
    host.service_mut().set_memory_storage(config);
    host.service_mut().set_team_writer(store.handle());
    let reply = host
        .handle_command(
            Command::Conversation(Conversation::CreateGoal(
                workbench_protocol::commands::SubmitGoal {
                    project: project(),
                    thread: thread("team"),
                    objective: "implement independent tasks".into(),
                    references: vec![],
                    constraints: vec![],
                    delegation: Some(workbench_protocol::commands::Delegation::Team {
                        value: "independent paths".into(),
                    }),
                },
            )),
            local_desktop(),
        )
        .unwrap();
    rt.block_on(model.reached.notified());
    assert_eq!(runtime.team_tasks().len(), 1);
    let run = reply.run_id.unwrap().as_str().parse().unwrap();
    runtime.cancel(run).unwrap();
    rt.block_on(runtime.wait(run)).unwrap();
}

/// The conversation result a command answered with.
fn conversation_result(reply: workbench_protocol::commands::CommandReply) -> ConversationResult {
    match reply.result.map(|result| *result) {
        Some(workbench_protocol::commands::CommandResult::Conversation(result)) => result,
        other => panic!("expected a conversation result, got {other:?}"),
    }
}
