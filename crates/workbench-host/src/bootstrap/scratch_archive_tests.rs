//! Archiving a family through the production host collects its scratch: the
//! command hints the owner once the archive is saved, the checker `bootstrap`
//! installed reads the shared sidebar, and the owner's archive pass renames the
//! directory. Only the model and the per-run sandbox are replaced.

use super::*;
use crate::provenance::local_desktop;
use runtime::{AgentInvocationContext, AgentModel, Role, RuntimeError};
use sha2::{Digest, Sha256};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use workbench_protocol::commands::{Command, CommandResult, ThreadTarget};
use workbench_protocol::ids::{SessionId, ThreadId as WireThread};
use workbench_protocol::namespaces::workspace::*;
use workbench_protocol::namespaces::{WorkspaceCommand, WorkspaceQuery, WorkspaceReply};
use workbench_protocol::queries::{PageRequest, Query, QueryReply};
use workspace_state::{
    ProjectId, SidebarDocument, SidebarStore, ThreadId as StateThread, ThreadRecord,
};

struct NoProviderCalls;
#[async_trait::async_trait]
impl AgentModel for NoProviderCalls {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "local/test".into()
    }
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        panic!("archiving must not call a provider");
    }
}

struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

struct World {
    _owners: test_socket_dir::SocketDir,
    owners: PathBuf,
    boot: BootstrappedHost,
    _dir: tempfile::TempDir,
}

async fn world(threads: &[&str]) -> World {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    let store = SidebarStore::new(config.join("sidebar.json"));
    let mut document = SidebarDocument::default();
    document
        .sidebar
        .add_project(ProjectId::new("p"), "repo", &repo)
        .unwrap();
    for id in threads {
        document.sidebar.threads.push(ThreadRecord::new(
            StateThread::new(*id),
            ProjectId::new("p"),
            *id,
        ));
    }
    document.sidebar.refresh_derived();
    {
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
    }
    let owners = test_socket_dir::SocketDir::new();
    let boot = BootstrappedHost::open_with_test_seams(
        BootstrapOptions {
            config_dir: config,
            project_root: repo,
            ownership_root: owners.path().into(),
        },
        TestSeams {
            model: Arc::new(NoProviderCalls),
            sandbox: Arc::new(Direct),
        },
    )
    .await
    .unwrap();
    World {
        owners: owners.path().to_path_buf(),
        _owners: owners,
        boot,
        _dir: dir,
    }
}

impl World {
    /// A scratch directory with content under the owner's identity-marked root.
    fn scratch(&self, thread: &str) -> PathBuf {
        self.boot.owner.start(thread).unwrap();
        let dir = self.boot.owner.thread_scratch().thread_dir(thread);
        let canonical = self.owners.canonicalize().unwrap();
        let digest = format!("{:x}", Sha256::digest(canonical.as_os_str().as_bytes()));
        let name = dir.file_name().unwrap().to_str().unwrap();
        let prepared = sandbox::scratch::ScratchRoot::fixed(&digest[..8], &digest)
            .unwrap()
            .prepare(name)
            .unwrap();
        assert_eq!(prepared, dir);
        std::fs::write(dir.join("keep"), b"scratch").unwrap();
        dir
    }

    fn archive(&self, thread: &str) {
        assert!(matches!(
            self.archive_result(thread),
            WorkspaceResult::Applied(_)
        ));
    }

    fn archive_result(&self, thread: &str) -> WorkspaceResult {
        let wire = WireThread::new(thread).unwrap();
        let mut host = self.boot.host.lock().unwrap();
        let session = SessionId::new("test").unwrap();
        let QueryReply::Workspace(WorkspaceReply::Families(page)) = host
            .handle_query(
                &session,
                &Query::Workspace(WorkspaceQuery::ListFamilies(PageRequest::default())),
            )
            .unwrap()
        else {
            panic!("not families")
        };
        let QueryReply::Workspace(WorkspaceReply::ThreadDetail(detail)) = host
            .handle_query(
                &session,
                &Query::Workspace(WorkspaceQuery::GetThreadDetail(ThreadTarget {
                    thread: wire.clone(),
                })),
            )
            .unwrap()
        else {
            panic!("not a thread detail")
        };
        let family = page
            .items
            .into_iter()
            .find(|family| Some(&family.key) == detail.family.as_ref())
            .expect("the thread belongs to a family");
        let reply = host
            .handle_command(
                Command::Workspace(WorkspaceCommand::SetFamilyArchived(SetFamilyArchived {
                    thread: wire,
                    archived: true,
                    expected_family: FamilyIdentity {
                        key: family.key.clone(),
                        membership_epoch: family.membership_epoch,
                    },
                    expected_revision: family.archive_revision,
                })),
                local_desktop(),
            )
            .unwrap();
        match *reply.result.unwrap() {
            CommandResult::Workspace(result) => result,
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn archiving_through_the_host_collects_the_scratch() {
    let w = world(&["t", "other"]).await;
    let archived = w.scratch("t");
    let kept = w.scratch("other");
    w.archive("t");
    w.boot.owner.scratch_archive_barrier().recv().unwrap();
    assert!(!archived.exists());
    assert!(kept.join("keep").exists());
}

#[tokio::test]
async fn shutdown_releases_ownership_and_detaches_the_scratch() {
    let mut w = world(&["t"]).await;
    let dir = w.scratch("t");
    w.archive("t");
    w.boot.shutdown().await.unwrap();
    assert!(!dir.exists());
    assert_eq!(
        w.boot.owner.attach("t").unwrap().state,
        runtime::ownership::OwnerState::Released
    );
}

#[tokio::test]
async fn host_attachment_removal_failure_after_save_does_not_prevent_scratch_detachment() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let w = world(&["t"]).await;
    let dir = w.scratch("t");
    let root = w.boot.runtime.attachment_root().unwrap();
    let files = root.files("t").unwrap();
    files
        .file(
            "old.txt",
            rustix::fs::OFlags::CREATE | rustix::fs::OFlags::EXCL | rustix::fs::OFlags::WRONLY,
        )
        .unwrap()
        .write_all(b"attachment")
        .unwrap();
    std::fs::set_permissions(files.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = w.archive_result("t");
    std::fs::set_permissions(files.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    if std::fs::read_dir(files.path()).is_ok_and(|mut entries| entries.next().is_none()) {
        // A privileged test runner cannot inject a permission failure.
        return;
    }
    assert!(matches!(
        result,
        WorkspaceResult::Rejected(WorkspaceRejection::StorageFailed { .. })
    ));
    w.boot.owner.scratch_archive_barrier().recv().unwrap();
    assert!(
        !dir.exists(),
        "saved archive is collectible despite attachment failure"
    );
}
