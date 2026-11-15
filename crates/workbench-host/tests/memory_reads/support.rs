//! A host over a real database and runtime whose read areas are the ones under
//! test. Reads are operations, so a test asks, waits for the host's counted tasks
//! to end (the same signal a planned restart waits for) and asks again with the
//! same operation id to read what the operation kept.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use runtime::AgentRuntime;
use workbench_host::CommandHost;
use workbench_host::areas::{AreaTasks, Areas};
use workbench_host::commands::CommandService;
use workbench_host::projection::Projection;
use workbench_host::projects::StaticProjects;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::ids::{OperationId, ProjectId, SessionId};
use workbench_protocol::operation::OperationState;
use workbench_protocol::queries::{Query, QueryReply};

pub struct Parts {
    pub tasks: AreaTasks,
    pub projection: Arc<Projection>,
    pub database: PathBuf,
    pub runtime: AgentRuntime,
    pub dir: PathBuf,
}

pub struct World {
    pub rt: tokio::runtime::Runtime,
    pub dir: tempfile::TempDir,
    pub store: storage::Storage,
    pub runtime: AgentRuntime,
    pub reached: Arc<tokio::sync::Notify>,
    pub tasks: AreaTasks,
    pub host: CommandHost,
}

pub fn project() -> ProjectId {
    ProjectId::new("project-1").unwrap()
}

pub fn operation(name: &str) -> OperationId {
    OperationId::new(name).unwrap()
}

pub fn world(areas: impl FnOnce(Parts) -> Areas) -> World {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("host.db");
    let store = storage::Storage::open(storage::StorageConfig {
        db_path: database.clone(),
        ..Default::default()
    })
    .unwrap();
    let bus = Arc::new(event_bus::EventBus::new(64));
    let reached = Arc::new(tokio::sync::Notify::new());
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(NeverAnswers {
            reached: reached.clone(),
        }),
    )
    .with_run_store(
        runtime::RunStore::open(
            &storage::StorageConfig {
                db_path: database.clone(),
                ..Default::default()
            },
            store.handle(),
        )
        .unwrap(),
    );
    let mut projects = StaticProjects::default();
    projects.insert(&project(), dir.path().to_path_buf());
    let host = CommandHost::new(CommandService::new(runtime.clone(), rt.handle().clone()))
        .with_projects(projects);
    let tasks = AreaTasks::new(rt.handle().clone());
    let areas = areas(Parts {
        tasks: tasks.clone(),
        projection: host.projection().clone(),
        database,
        runtime: runtime.clone(),
        dir: dir.path().to_path_buf(),
    });
    let host = host.with_areas(areas);
    World {
        rt,
        dir,
        store,
        runtime,
        reached,
        tasks,
        host,
    }
}

pub struct NeverAnswers {
    /// Notified when a request arrives: the run is inside its first model call and
    /// its prepared context is stored.
    pub reached: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl runtime::AgentModel for NeverAnswers {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.reached.notify_one();
        std::future::pending().await
    }

    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "test".into()
    }
}

impl World {
    pub fn query(&self, query: Query) -> Result<QueryReply, WorkbenchError> {
        self.host
            .handle_query(&SessionId::new("session").unwrap(), &query)
    }

    /// Wait until no read is running: the counted tasks have ended.
    pub fn settle(&self) {
        let ended = self.tasks.counted_ended_notify();
        self.rt.block_on(async {
            while self.tasks.busy() {
                ended.notified().await;
            }
        });
    }

    /// Ask, wait for the read to end, ask again: the settled answer.
    pub fn read<T>(&self, ask: impl Fn() -> Result<T, WorkbenchError>) -> T {
        ask().expect("the read is accepted");
        self.settle();
        ask().expect("the settled read is answered")
    }
}

pub fn ready(state: &OperationState) -> bool {
    *state == OperationState::Ready
}

pub fn path_of(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}
