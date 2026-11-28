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

fn begin(filename: &str, byte_length: u64) -> Command {
    Command::Conversation(Conversation::BeginAttachment {
        project: project(),
        thread: thread("thread-1"),
        client_upload_id: uuid::Uuid::new_v4().to_string(),
        filename: filename.into(),
        media_type_hint: "application/octet-stream".into(),
        byte_length,
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
fn thread_scoped_controls_preserve_the_shared_tombstone_fence() {
    use workbench_host::commands::{HostCommand, HostEvent, RejectKind};
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let mut tombstones = storage::thread_retention::TombstoneSet::default();
    tombstones.threads.insert("thread-1".into());
    runtime
        .set_tombstone_guard(runtime::thread_retention::TombstoneGuard::new(tombstones))
        .unwrap();
    let mut host = host_with_project(&rt, runtime, dir.path());
    for command in [
        HostCommand::SetGoalReview {
            thread_id: "thread-1".into(),
            goal_id: "missing".into(),
            enabled: true,
        },
        HostCommand::SetGoalChecksPaused {
            thread_id: "thread-1".into(),
            goal_id: "missing".into(),
            paused: false,
        },
        HostCommand::CompactChat {
            thread_id: "thread-1".into(),
        },
        HostCommand::DeliverFollowUpsNextTurn {
            thread_id: "thread-1".into(),
        },
        HostCommand::ResumeLaneDeliveries {
            thread_id: "thread-1".into(),
        },
        HostCommand::AnswerUserQuestion {
            thread_id: "thread-1".into(),
            question_id: "missing".into(),
            answer: "must not be recorded".into(),
        },
    ] {
        assert!(matches!(
            host.submit_local(command, &local_desktop()).as_slice(),
            [HostEvent::ChatRejected {
                kind: RejectKind::NotFound,
                ..
            }]
        ));
    }
}

#[test]
fn generic_attachments_are_scoped_and_commands_are_correlatable_by_receipt() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = host_with_project(&rt, runtime(), dir.path());
    for filename in ["../bad", "bad\nname", ""] {
        assert_eq!(
            host.handle_command(begin(filename, 1), web())
                .unwrap_err()
                .code,
            ErrorCode::InvalidRequest
        );
    }
    let session = SessionId::new("desktop-session").unwrap();
    let request = RequestId::new("upload-1").unwrap();
    let reply = host
        .handle_request(&session, &request, begin("opaque.bin", 5), local_desktop())
        .unwrap();
    assert!(matches!(
        host.command_receipt(&session, &request).unwrap().state,
        CommandReceiptState::Applied { .. }
    ));
    let ConversationResult::AttachmentBegun { upload_id, .. } = conversation_result(reply) else {
        panic!()
    };
    host.handle_command(
        Command::Conversation(Conversation::AppendAttachment {
            upload_id: upload_id.clone(),
            offset: 0,
            data_base64: STANDARD.encode(b"hello"),
        }),
        web(),
    )
    .unwrap();
    let reply = host
        .handle_command(
            Command::Conversation(Conversation::FinishAttachment { upload_id }),
            web(),
        )
        .unwrap();
    let ConversationResult::AttachmentFinished(reference) = conversation_result(reply) else {
        panic!()
    };
    let mut chat = send("thread-1", "image-input", "");
    chat.attachments.push(reference.id.clone());
    let mut other = chat.clone();
    other.thread = thread("thread-2");
    assert_eq!(
        host.handle_command(
            Command::Conversation(ConversationCommand::SendChat(other)),
            web()
        )
        .unwrap_err()
        .code,
        ErrorCode::NotFound
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
            attachment_id: reference.id,
        }),
        web(),
    )
    .unwrap();
}

fn save_access(path: &Path, root: &Path, trust: &str) {
    let mut document = workspace_state::SidebarDocument::default();
    let project = workspace_state::ProjectId::new(project().as_str());
    document
        .sidebar
        .add_project(project.clone(), "Project", root)
        .unwrap();
    document
        .sidebar
        .set_project_trust(
            &project,
            if trust == "approved" {
                workspace_state::TrustState::Approved
            } else {
                workspace_state::TrustState::Unapproved
            },
        )
        .unwrap();
    std::fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
}

#[test]
fn ordinary_continue_revalidates_current_project_trust_without_a_takeover() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = test_socket_dir::SplitDir::new();
    let other = tempfile::tempdir().unwrap();
    let other_project = workbench_protocol::ids::ProjectId::new("other-project").unwrap();
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
    let runtime = runtime_on_store(dir.path(), &store);
    let access = dir.path().join("sidebar.json");
    let service =
        workbench_host::commands::CommandService::new(runtime.clone(), rt.handle().clone())
            .with_sidebar_path(access.clone());
    let mut projects = workbench_host::projects::StaticProjects::default();
    projects.insert(&project(), dir.path().into());
    projects.insert(&other_project, other.path().into());
    let mut host = workbench_host::CommandHost::new(service).with_projects(projects);
    let continuation = || {
        Command::Conversation(ConversationCommand::ContinueChat(ContinueChat {
            project: project(),
            thread: thread("thread-1"),
            composer_role: ComposerRole::Worker,
            model_preference: None,
        }))
    };
    let wrong_project = || {
        Command::Conversation(ConversationCommand::ContinueChat(ContinueChat {
            project: other_project.clone(),
            thread: thread("thread-1"),
            composer_role: ComposerRole::Worker,
            model_preference: None,
        }))
    };
    save_access(&access, dir.path(), "unapproved");
    assert_eq!(
        host.handle_command(continuation(), web()).unwrap_err().code,
        ErrorCode::PermissionDenied
    );
    assert!(
        !host
            .service()
            .bindings
            .thread_roots
            .contains_key("thread-1")
    );
    save_access(&access, dir.path(), "approved");
    for previous in [None, Some(project().to_string())] {
        host.service_mut().bindings.goal_projects.remove("thread-1");
        if let Some(previous) = &previous {
            host.service_mut()
                .bindings
                .goal_projects
                .insert("thread-1".into(), previous.clone());
        }
        let wrong_goal = Command::Execution(
            workbench_protocol::namespaces::ExecutionCommand::SubmitGoal(
                workbench_protocol::commands::SubmitGoal {
                    project: other_project.clone(),
                    thread: thread("thread-1"),
                    objective: "must not replace the goal binding".into(),
                    references: Vec::new(),
                    constraints: Vec::new(),
                    delegation: Some(workbench_protocol::commands::Delegation::Direct),
                },
            ),
        );
        assert_eq!(
            host.handle_command(wrong_goal, web()).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            host.service().bindings.goal_projects.get("thread-1"),
            previous.as_ref()
        );
    }
    assert_eq!(
        host.handle_command(wrong_project(), web())
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    assert!(
        !host
            .service()
            .bindings
            .thread_roots
            .contains_key("thread-1")
    );
    let reply = host.handle_command(continuation(), web()).unwrap();
    assert_eq!(reply.run_id.unwrap().as_str(), question.run_id);
    let run = question.run_id.parse().unwrap();
    runtime.cancel(run).unwrap();
    rt.block_on(runtime.wait(run)).unwrap();
    let root = host.service().bindings.thread_roots["thread-1"].clone();
    assert_eq!(
        host.handle_command(wrong_project(), web())
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(host.service().bindings.thread_roots["thread-1"], root);
    save_access(&access, dir.path(), "unapproved");
    assert_eq!(
        host.handle_command(continuation(), web()).unwrap_err().code,
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

#[test]
fn ordinary_continue_does_not_open_or_modify_a_corrupt_legacy_registry() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = test_socket_dir::SplitDir::new();
    let (_, store) = runtime_with_store(dir.path());
    let question = offline_question(&rt, dir.path(), &store);
    let legacy = dir.path().join("owners.db");
    std::fs::write(&legacy, b"legacy corrupt registry").unwrap();
    let runtime = runtime_on_store(dir.path(), &store);
    let mut host = host_with_project(&rt, runtime.clone(), dir.path());
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
    assert_eq!(std::fs::read(legacy).unwrap(), b"legacy corrupt registry");
    let run = question.run_id.parse().unwrap();
    runtime.cancel(run).unwrap();
    rt.block_on(runtime.wait(run)).unwrap();
}
