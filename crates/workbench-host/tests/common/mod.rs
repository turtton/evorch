//! Shared fixtures: a real `AgentRuntime` whose model never answers, so a run
//! stays in flight until a test cancels it.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use event_bus::EventBus;
use providers::{ChatResponse, Message, ToolSpec};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RuntimeError};
use tools::ToolExecutor;
use workbench_host::CommandHost;
use workbench_host::commands::CommandService;
use workbench_host::projects::StaticProjects;
use workbench_protocol::commands::{ComposerRole, SendChat};
use workbench_protocol::envelope::{Provenance, Surface};
use workbench_protocol::ids::{ClientId, ProjectId, SubmissionId, ThreadId};

#[derive(Default)]
pub struct HeldModel;

#[async_trait]
impl AgentModel for HeldModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        std::future::pending().await
    }

    fn selected_model(&self, role: Role, _: Option<&str>) -> String {
        format!("test-{}", role.name().to_lowercase())
    }
}

pub fn runtime() -> AgentRuntime {
    let bus = Arc::new(EventBus::new(64));
    AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(HeldModel),
    )
}

pub fn project() -> ProjectId {
    ProjectId::new("project-1").unwrap()
}

/// A host whose directory knows `project()` at `root`.
pub fn host_with_project(
    rt: &tokio::runtime::Runtime,
    runtime: AgentRuntime,
    root: &Path,
) -> CommandHost {
    let mut projects = StaticProjects::default();
    projects.insert(&project(), root.to_path_buf());
    CommandHost::new(CommandService::new(runtime, rt.handle().clone())).with_projects(projects)
}

pub fn web() -> Provenance {
    Provenance::human(Surface::Web, ClientId::new("web-1").unwrap())
}

pub fn cli() -> Provenance {
    Provenance::human(Surface::Cli, ClientId::new("cli-1").unwrap())
}

pub fn send(thread: &str, submission: &str, text: &str) -> SendChat {
    SendChat {
        project: project(),
        thread: ThreadId::new(thread).unwrap(),
        submission_id: SubmissionId::new(submission).unwrap(),
        composer_role: ComposerRole::Worker,
        text: text.into(),
        skills: Vec::new(),
        images: Vec::new(),
        model_preference: None,
        fork_seed: None,
    }
}

/// A runtime with a run store in `dir`; follow-ups and question lookups need one.
/// Keep the returned storage alive as long as the runtime is used.
pub fn runtime_with_store(dir: &Path) -> (AgentRuntime, storage::Storage) {
    let config = storage::StorageConfig {
        db_path: dir.join("host.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let runtime =
        runtime().with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
    (runtime, storage)
}

/// Another runtime over the same store, as a second incarnation of the host would be.
pub fn runtime_on_store(dir: &Path, storage: &storage::Storage) -> AgentRuntime {
    let config = storage::StorageConfig {
        db_path: dir.join("host.db"),
        ..Default::default()
    };
    runtime().with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap())
}

/// A question another incarnation asked and nobody is waiting for, on `thread-1`.
pub fn offline_question(
    rt: &tokio::runtime::Runtime,
    dir: &Path,
    storage: &storage::Storage,
) -> event_bus::UserQuestion {
    let requester = runtime_on_store(dir, storage);
    rt.block_on(async {
        let run = requester
            .delegate_chat(
                "thread-1",
                Role::Worker,
                "Prepare the output".into(),
                runtime::RunConfig::default(),
            )
            .unwrap();
        let question = requester
            .request_user_question(run, "Which format?".into(), vec!["JSON".into()], true)
            .unwrap();
        requester.cancel(run).unwrap();
        requester.wait(run).await.unwrap();
        requester
            .host_input_receipts(&["flush".to_owned()])
            .unwrap();
        question
    })
}

/// Ownership of `thread` that moved to another host after `permit` was taken, so
/// `owner` no longer owns it and `permit` is stale.
pub struct LostOwnership {
    pub owner: Arc<runtime::ownership::OwnerHost>,
    pub permit: runtime::ownership::OwnerPermit,
    _successor: runtime::ownership::OwnerHost,
    _dir: tempfile::TempDir,
}

pub fn lost_ownership(thread: &str) -> LostOwnership {
    let dir = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new(64));
    let owner =
        runtime::ownership::OwnerHost::open(dir.path(), Default::default(), bus.clone()).unwrap();
    owner.start(thread).unwrap();
    let permit = owner.owned_permit(thread).unwrap();
    let successor =
        runtime::ownership::OwnerHost::open(dir.path(), Default::default(), bus).unwrap();
    owner.handoff(&permit, &successor).unwrap();
    LostOwnership {
        owner: Arc::new(owner),
        permit,
        _successor: successor,
        _dir: dir,
    }
}
