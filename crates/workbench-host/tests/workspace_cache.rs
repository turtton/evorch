//! A trust or sidebar change made through the core is bookkeeping for later
//! runs: it never rewrites what a conversation already sent to the provider
//! (instructions, tool schemas, earlier messages), so the prompt cache prefix
//! survives it.

mod common;

use async_trait::async_trait;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RuntimeError};
use std::sync::{Arc, Mutex};
use tools::ToolExecutor;
use workbench_host::areas::workspace::WorkspaceArea;
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::Command;
use workbench_protocol::commands::{CommandResult, ThreadTarget};
use workbench_protocol::ids::{ProjectId, ThreadId};
use workbench_protocol::namespaces::workspace::*;
use workbench_protocol::namespaces::{ConversationCommand, WorkspaceQuery, WorkspaceReply};
use workbench_protocol::queries::PageRequest;
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

use common::{host_with_project, send};
use event_bus::{AgentRunPhase, EventKind, EventReceiver, LifecycleEvent};

/// Waits for the turn of `run` to complete: the conversation stays open for
/// the next message, so the run itself does not end.
async fn turn(events: &mut EventReceiver, run: &str) {
    loop {
        match events.recv().await.unwrap().kind {
            EventKind::Lifecycle(LifecycleEvent::TurnCompleted { run_id, .. }) if run_id == run => {
                return;
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: AgentRunPhase::Error,
                reason,
                ..
            }) if run_id == run => panic!("run failed: {reason:?}"),
            _ => (),
        }
    }
}

#[derive(Default)]
struct Recording {
    requests: Mutex<Vec<(Vec<String>, String)>>,
}

#[async_trait]
impl AgentModel for Recording {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.requests.lock().unwrap().push((
            messages.iter().map(|m| format!("{m:?}")).collect(),
            format!("{tools:?}"),
        ));
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text { text: "ok".into() }],
            },
            usage: Default::default(),
            finish_reason: FinishReason::Stop,
        })
    }

    fn selected_model(&self, role: Role, _: Option<&str>) -> String {
        format!("test-{}", role.name().to_lowercase())
    }
}

fn applied(result: Option<Box<CommandResult>>) {
    match result.map(|r| *r) {
        Some(CommandResult::Workspace(WorkspaceResult::Applied(_))) => (),
        other => panic!("{other:?}"),
    }
}

#[test]
fn trust_and_sidebar_changes_do_not_rewrite_what_was_already_sent_to_the_provider() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let config = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let store = SidebarStore::new(config.path().join("sidebar.json"));
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(StateProject::new("p1"), "repo", &root)
        .unwrap();
    document.sidebar.threads.push(ThreadRecord::new(
        StateThread::new("thread-1"),
        StateProject::new("p1"),
        "first",
    ));
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }

    let model = Arc::new(Recording::default());
    let bus = Arc::new(event_bus::EventBus::new(64));
    let mut events = bus.subscribe();
    let db = tempfile::tempdir().unwrap();
    let storage_config = storage::StorageConfig {
        db_path: db.path().join("host.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus.clone())),
        model.clone(),
    )
    .with_run_store(runtime::RunStore::open(&storage_config, storage.handle()).unwrap());
    let mut host = host_with_project(&rt, runtime.clone(), &root);
    let area = WorkspaceArea::with_store(
        SidebarStore::new(config.path().join("sidebar.json")),
        Some(runtime.clone()),
        vec![config.path().to_path_buf()],
        None,
        |_| false,
    );

    let first = host
        .handle_command(
            Command::Conversation(ConversationCommand::SendChat(send(
                "thread-1", "sub-1", "hello",
            ))),
            local_desktop(),
        )
        .unwrap();
    rt.block_on(turn(&mut events, first.run_id.unwrap().as_str()));

    let command = |c: WorkspaceCommand| {
        applied(area.command(c, &local_desktop()).unwrap().result);
    };
    let project = ProjectId::new("p1").unwrap();
    let thread = ThreadId::new("thread-1").unwrap();
    let access = |area: &WorkspaceArea| match area
        .query(&WorkspaceQuery::GetProjectAccess(ProjectTarget {
            project: project.clone(),
        }))
        .unwrap()
    {
        WorkspaceReply::ProjectAccess(view) => view.revision,
        other => panic!("{other:?}"),
    };
    command(WorkspaceCommand::SetProjectTrust(SetProjectTrust {
        project: project.clone(),
        trust: TrustLevel::Approved,
        expected_revision: access(&area),
    }));
    command(WorkspaceCommand::UpdateAllowedDirectory(
        UpdateAllowedDirectory {
            project: project.clone(),
            action: AllowedDirectoryAction::Add {
                path: extra.path().to_string_lossy().into_owned(),
                trust: TrustLevel::Approved,
            },
            expected_revision: access(&area),
        },
    ));
    let detail = |area: &WorkspaceArea| match area
        .query(&WorkspaceQuery::GetThreadDetail(ThreadTarget {
            thread: thread.clone(),
        }))
        .unwrap()
    {
        WorkspaceReply::ThreadDetail(detail) => *detail,
        other => panic!("{other:?}"),
    };
    command(WorkspaceCommand::RenameThread(RenameThread {
        thread: thread.clone(),
        title: "renamed".into(),
        expected_revision: detail(&area).revision,
    }));
    let family = match area
        .query(&WorkspaceQuery::ListFamilies(PageRequest::default()))
        .unwrap()
    {
        WorkspaceReply::Families(page) => page.items.into_iter().next().unwrap(),
        other => panic!("{other:?}"),
    };
    command(WorkspaceCommand::SetFamilyPinned(SetFamilyPinned {
        thread: thread.clone(),
        pinned: true,
        expected_family: FamilyIdentity {
            key: family.key.clone(),
            membership_epoch: family.membership_epoch,
        },
        expected_revision: family.pin_revision,
    }));
    command(WorkspaceCommand::SetFamilyArchived(SetFamilyArchived {
        thread: thread.clone(),
        archived: true,
        expected_family: FamilyIdentity {
            key: family.key.clone(),
            membership_epoch: family.membership_epoch,
        },
        expected_revision: family.archive_revision,
    }));

    let second = host
        .handle_command(
            Command::Conversation(ConversationCommand::SendChat(send(
                "thread-1", "sub-2", "again",
            ))),
            local_desktop(),
        )
        .unwrap();
    rt.block_on(turn(&mut events, second.run_id.unwrap().as_str()));

    let requests = model.requests.lock().unwrap();
    assert!(requests.len() >= 2, "two turns were sent");
    let (before, tools_before) = &requests[0];
    let (after, tools_after) = &requests[1];
    assert!(after.len() > before.len());
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "everything sent before is still the prefix of what is sent next"
    );
    assert_eq!(
        tools_before, tools_after,
        "tool schemas and their order are stable"
    );
}
