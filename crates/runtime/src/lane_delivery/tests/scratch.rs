//! Scratch must be prepared by the common waiting boundary for lane resumes,
//! not just for user inbox messages. Reuse the real durable delivery fixture.
use super::*;

struct DirectFactory;
impl crate::SandboxFactory for DirectFactory {
    fn build(
        &self,
        _: &crate::ExecutionPolicy,
        _: &crate::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}

struct HeldLaneModel {
    base: Arc<Model>,
    target: Mutex<Option<String>>,
    entered: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl AgentModel for HeldLaneModel {
    fn selected_model(&self, role: Role, project: Option<&str>) -> String {
        self.base.selected_model(role, project)
    }
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        let held = self.target.lock().unwrap().as_deref() == Some(&invocation.run_id);
        if held {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        self.base.complete(invocation, role, messages, tools).await
    }
}

#[tokio::test]
async fn lane_delivery_registers_and_recreates_reclaimed_scratch_before_the_model_runs() {
    let (mut f, scratch, archive) = archive_fixture();
    f.runtime = f.runtime.with_sandbox_root(f._dir.path().to_path_buf());
    Arc::get_mut(&mut f.runtime.shared).unwrap().sandbox_factory = Some(Arc::new(DirectFactory));
    let (consulted, mut received) = tokio::sync::mpsc::unbounded_channel();
    archive.set_checker(Arc::new(move |_| {
        consulted.send(()).unwrap();
        Ok(true)
    }));
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "scratch-resume").await;
    let dir = scratch.thread_dir("delivery-lead");
    std::fs::write(dir.join("keep"), "keep").unwrap();
    archive.hint("delivery-lead");
    received.recv().await.unwrap();
    let pass = archive.barrier();
    tokio::task::spawn_blocking(move || pass.recv().unwrap())
        .await
        .unwrap();
    assert!(!dir.exists());

    f.model.block_next.store(true, Ordering::SeqCst);
    f.report(&lane, Some(1), LaneStatus::Ready);
    assert_eq!(
        f.drive().await,
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    f.model.blocked.notified().await;
    assert!(
        dir.is_dir(),
        "lane input must recreate the same scratch before model use"
    );
    assert!(!dir.join("keep").exists(), "the resumed scratch is empty");
    archive.hint("delivery-lead");
    let pass = archive.barrier();
    tokio::task::spawn_blocking(move || pass.recv().unwrap())
        .await
        .unwrap();
    assert!(
        received.try_recv().is_err(),
        "the resumed execution is registered"
    );
    assert!(dir.exists());
    f.runtime.stop(lead, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(lead).await.unwrap();
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::SelfOnly)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
}

#[tokio::test]
async fn active_lane_thread_scratch_is_kept_until_its_execution_ends() {
    let (mut f, scratch, archive) = archive_fixture();
    f.runtime = f.runtime.with_sandbox_root(f._dir.path().to_path_buf());
    let model = Arc::new(HeldLaneModel {
        base: f.model.clone(),
        target: Mutex::new(None),
        entered: tokio::sync::Notify::new(),
    });
    {
        let shared = Arc::get_mut(&mut f.runtime.shared).unwrap();
        shared.sandbox_factory = Some(Arc::new(DirectFactory));
        shared.model = model.clone();
    }
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "archive-busy-lane").await;
    archive.set_checker(Arc::new(|_| Ok(true)));
    // Opening an attached lane may also wake the lead. Only this lane's
    // invocation proves that its scratch registration has resumed.
    *model.target.lock().unwrap() = Some(lane.root_run_id.to_string());
    f.runtime
        .send_message(lane.root_run_id, "resume active lane".into())
        .unwrap();
    model.entered.notified().await;
    assert_eq!(
        f.runtime.scratch_key(lane.root_run_id),
        crate::scratch::ScratchKey::Thread(lane.thread_id.clone())
    );
    assert!(scratch.archive_snapshot(&lane.thread_id).is_none());
    let dir = scratch.thread_dir(&lane.thread_id);
    std::fs::write(dir.join("keep"), "lane scratch").unwrap();
    archive.hint(&lane.thread_id);
    let pass = archive.barrier();
    tokio::task::spawn_blocking(move || pass.recv().unwrap())
        .await
        .unwrap();
    assert_eq!(std::fs::read(dir.join("keep")).unwrap(), b"lane scratch");
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::SelfOnly)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    let pass = archive.barrier();
    tokio::task::spawn_blocking(move || pass.recv().unwrap())
        .await
        .unwrap();
    assert!(!dir.exists());
    f.runtime.stop(lead, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(lead).await.unwrap();
}

/// Uses the same root writer and two-way binding as production, without owners.db.
fn archive_fixture() -> (
    Fixture,
    Arc<crate::scratch::ThreadScratch>,
    crate::scratch::archive::ArchiveService,
) {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().join("threads");
    std::fs::create_dir_all(&root).unwrap();
    let sidebar = dir.path().join("sidebar.json");
    std::fs::write(&sidebar, "{}").unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("delivery.db"),
        ..Default::default()
    };
    let leases =
        Arc::new(storage::WriterLeases::acquire(&config.db_path, &root, Duration::ZERO).unwrap());
    let shared = storage::shared_with_wait(&config.db_path, &root, Duration::ZERO).unwrap();
    let storage = storage::Storage::open_with_writer_lease(
        config.clone(),
        shared.db_lease().unwrap(),
        leases.db_lease(),
    )
    .unwrap();
    crate::thread_retention::binding::ensure_binding(
        &storage::Database::open(&config).unwrap(),
        &storage.handle(),
        &leases.authority().unwrap(),
        crate::thread_retention::BindingPaths {
            ownership_root: &root,
            sidebar_path: &sidebar,
        },
        &config.db_path,
        1,
    )
    .unwrap();
    let scratch = Arc::new(crate::scratch::ThreadScratch::for_ownership_root(&root).unwrap());
    let archive = crate::scratch::archive::ArchiveService::new(
        &scratch,
        crate::scratch::archive::authority(config.clone(), root, sidebar, leases, scratch.clone()),
    )
    .unwrap();
    let model = Arc::new(Model::default());
    let bus = Arc::new(EventBus::new(16384));
    let events = bus.subscribe();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        model.clone(),
    )
    .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    runtime.set_thread_scratch(scratch.clone());
    (
        Fixture {
            runtime,
            storage,
            config,
            model,
            events,
            _dir: dir,
        },
        scratch,
        archive,
    )
}
