//! The core is the writer of the workspace (PR 09), through the production
//! `BootstrappedHost`: a conversation the core creates is listed in the sidebar
//! and can be renamed, pinned and archived; a registered or renamed project is
//! listed under the saved name and can start conversations; what a user granted
//! survives a restart. Only the model is scripted.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_host::provenance::local_desktop;
use workbench_protocol::commands::{Command, CommandResult, ComposerRole, SendChat, ThreadTarget};
use workbench_protocol::ids::{ProjectId, SessionId, SubmissionId, ThreadId};
use workbench_protocol::namespaces::workspace::*;
use workbench_protocol::namespaces::{
    ConversationCommand, WorkspaceCommand, WorkspaceQuery, WorkspaceReply,
};
use workbench_protocol::queries::{PageRequest, Query, QueryReply};
use workspace_state::{
    ProjectId as StateProject, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

struct Idle;

#[async_trait::async_trait]
impl AgentModel for Idle {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "idle".into(),
                }],
            },
            usage: Default::default(),
            finish_reason: FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "core-workspace".into()
    }
}

struct Unisolated;

impl runtime::SandboxFactory for Unisolated {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

struct World {
    dir: tempfile::TempDir,
    root: PathBuf,
    core: BootstrappedHost,
}

fn options(root: &Path) -> BootstrapOptions {
    BootstrapOptions {
        config_dir: root.join("config"),
        project_root: root.join("repo"),
        ownership_root: root.join("owners"),
    }
}

async fn open(root: &Path) -> BootstrappedHost {
    BootstrappedHost::open_with_test_seams(
        options(root),
        TestSeams {
            model: Arc::new(Idle),
            sandbox: Arc::new(Unisolated),
        },
    )
    .await
    .unwrap()
}

/// A sidebar that lists project `p` at `repo`, with the threads named.
async fn world(threads: &[&str]) -> World {
    let dir = tempfile::Builder::new()
        .prefix("ws-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("repo")).unwrap();
    std::fs::create_dir_all(root.join("config")).unwrap();
    let repo = root.join("repo");
    let store = SidebarStore::new(root.join("config/sidebar.json"));
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(StateProject::new("p"), "Saved repo", &repo)
        .unwrap();
    for id in threads {
        document.sidebar.threads.push(ThreadRecord::new(
            StateThread::new(*id),
            StateProject::new("p"),
            *id,
        ));
    }
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    let core = open(&root).await;
    World { dir, root, core }
}

impl World {
    fn command(&self, command: WorkspaceCommand) -> WorkspaceResult {
        let reply = self
            .core
            .host
            .lock()
            .unwrap()
            .handle_command(Command::Workspace(command), local_desktop())
            .unwrap();
        match *reply.result.unwrap() {
            CommandResult::Workspace(result) => result,
            other => panic!("{other:?}"),
        }
    }

    fn applied(&self, command: WorkspaceCommand) -> Applied {
        match self.command(command) {
            WorkspaceResult::Applied(applied) => applied,
            WorkspaceResult::Rejected(rejection) => panic!("rejected: {rejection:?}"),
        }
    }

    fn query(&self, query: WorkspaceQuery) -> WorkspaceReply {
        match self
            .core
            .host
            .lock()
            .unwrap()
            .handle_query(&SessionId::new("test").unwrap(), &Query::Workspace(query))
            .unwrap()
        {
            QueryReply::Workspace(reply) => reply,
            other => panic!("{other:?}"),
        }
    }

    fn detail(&self, thread: &str) -> ThreadDetail {
        match self.query(WorkspaceQuery::GetThreadDetail(ThreadTarget {
            thread: ThreadId::new(thread).unwrap(),
        })) {
            WorkspaceReply::ThreadDetail(detail) => *detail,
            other => panic!("{other:?}"),
        }
    }

    fn families(&self) -> Vec<FamilyView> {
        match self.query(WorkspaceQuery::ListFamilies(PageRequest::default())) {
            WorkspaceReply::Families(page) => page.items,
            other => panic!("{other:?}"),
        }
    }

    fn projects(&self) -> Vec<(String, String)> {
        match self.query(WorkspaceQuery::ListProjects(PageRequest::default())) {
            WorkspaceReply::Projects(page) => page
                .items
                .into_iter()
                .map(|p| (p.id.as_str().to_owned(), p.display_name))
                .collect(),
            other => panic!("{other:?}"),
        }
    }

    fn start_conversation(&self, project: &str, thread: &str, text: &str) {
        let send = SendChat {
            project: ProjectId::new(project).unwrap(),
            thread: ThreadId::new(thread).unwrap(),
            submission_id: SubmissionId::new(format!("sub-{thread}-{}", text.len())).unwrap(),
            composer_role: ComposerRole::Worker,
            text: text.into(),
            skills: vec![],
            images: vec![],
            model_preference: None,
            fork_seed: None,
        };
        let mut host = self.core.host.lock().unwrap();
        host.reserve_new_chat_thread(&send).unwrap();
        host.handle_command(
            Command::Conversation(ConversationCommand::SendChat(send)),
            local_desktop(),
        )
        .unwrap();
    }

    fn saved(&self) -> SidebarDocument {
        SidebarStore::new(self.root.join("config/sidebar.json"))
            .read_unlocked()
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_the_core_creates_can_be_renamed_pinned_and_archived() {
    let mut w = world(&[]).await;
    w.start_conversation("p", "fresh", "Plan the release\nwith details");

    // Listed in the file the other clients read, under the first line.
    assert!(
        w.saved()
            .sidebar
            .threads
            .iter()
            .any(|t| t.id.to_string() == "fresh" && t.title == "Plan the release")
    );
    let detail = w.detail("fresh");
    assert_eq!(detail.title, "Plan the release");
    let family = w
        .families()
        .into_iter()
        .find(|f| f.key.as_str() == "fresh")
        .expect("the new conversation is a family");

    w.applied(WorkspaceCommand::RenameThread(RenameThread {
        thread: ThreadId::new("fresh").unwrap(),
        title: "Release plan".into(),
        expected_revision: detail.revision,
    }));
    w.applied(WorkspaceCommand::SetFamilyPinned(SetFamilyPinned {
        thread: ThreadId::new("fresh").unwrap(),
        pinned: true,
        expected_family: FamilyIdentity {
            key: family.key.clone(),
            membership_epoch: family.membership_epoch,
        },
        expected_revision: family.pin_revision,
    }));
    w.applied(WorkspaceCommand::SetFamilyArchived(SetFamilyArchived {
        thread: ThreadId::new("fresh").unwrap(),
        archived: true,
        expected_family: FamilyIdentity {
            key: family.key.clone(),
            membership_epoch: family.membership_epoch,
        },
        expected_revision: family.archive_revision,
    }));
    let detail = w.detail("fresh");
    assert_eq!(detail.title, "Release plan");
    assert!(detail.pinned && detail.archived);

    // A follow-up message does not list the thread twice.
    w.start_conversation("p", "fresh", "and more");
    assert_eq!(
        w.saved()
            .sidebar
            .threads
            .iter()
            .filter(|t| t.id.to_string() == "fresh")
            .count(),
        1
    );
    w.core.shutdown().await.unwrap();
    drop(w.dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_registered_project_is_listed_under_its_saved_name_and_starts_conversations() {
    let mut w = world(&[]).await;
    let second = w.root.join("second");
    std::fs::create_dir_all(&second).unwrap();
    let done = w.applied(WorkspaceCommand::RegisterProject(RegisterProject {
        name: "Second project".into(),
        path: second.to_string_lossy().into_owned(),
    }));
    let project = done.project.expect("the registration names the project");

    // Listed, under the saved name rather than the id.
    let projects = w.projects();
    assert!(
        projects.contains(&(project.as_str().to_owned(), "Second project".to_owned())),
        "{projects:?}"
    );
    assert!(projects.contains(&("p".to_owned(), "Saved repo".to_owned())));

    // A conversation can start in it at once, and is listed under it.
    w.start_conversation(project.as_str(), "in-second", "hello");
    assert_eq!(w.detail("in-second").project, project);

    // A rename reaches the project list.
    let access = match w.query(WorkspaceQuery::GetProjectAccess(ProjectTarget {
        project: project.clone(),
    })) {
        WorkspaceReply::ProjectAccess(view) => *view,
        other => panic!("{other:?}"),
    };
    w.applied(WorkspaceCommand::RenameProject(RenameProject {
        project: project.clone(),
        name: "Renamed".into(),
        expected_revision: access.revision,
    }));
    assert!(
        w.projects()
            .contains(&(project.as_str().to_owned(), "Renamed".to_owned()))
    );
    w.core.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn what_a_user_granted_is_declared_again_after_the_core_restarts() {
    let mut w = world(&["t1"]).await;
    let extra = w.root.join("reference");
    std::fs::create_dir_all(&extra).unwrap();
    let detail = w.detail("t1");
    w.applied(WorkspaceCommand::SetThreadExtraReadDirs(
        SetThreadExtraReadDirs {
            thread: ThreadId::new("t1").unwrap(),
            dirs: vec![extra.to_string_lossy().into_owned()],
            expected_revision: detail.revision,
        },
    ));
    assert_eq!(
        w.core.runtime.thread_read_only_directories("t1"),
        vec![extra.canonicalize().unwrap()]
    );
    w.core.shutdown().await.unwrap();

    let mut again = open(&w.root).await;
    assert_eq!(
        again.runtime.thread_read_only_directories("t1"),
        vec![extra.canonicalize().unwrap()],
        "the saved read-only directory is declared before any run starts"
    );
    again.shutdown().await.unwrap();
}
