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

#[tokio::test]
async fn lane_delivery_registers_and_recreates_reclaimed_scratch_before_the_model_runs() {
    let mut f = Fixture::new();
    f.runtime = f.runtime.with_sandbox_root(f._dir.path().to_path_buf());
    Arc::get_mut(&mut f.runtime.shared).unwrap().sandbox_factory = Some(Arc::new(DirectFactory));
    let sockets = test_socket_dir::SocketDir::new();
    let host = crate::ownership::OwnerHost::open(
        sockets.path(),
        Default::default(),
        Arc::new(EventBus::new(32)),
    )
    .unwrap();
    host.start("delivery-lead").unwrap();
    let scratch = host.thread_scratch();
    f.runtime.set_thread_scratch(scratch.clone());
    let (consulted, mut received) = tokio::sync::mpsc::unbounded_channel();
    host.set_scratch_archive_checker(Arc::new(move |_| {
        consulted.send(()).unwrap();
        Ok(true)
    }));
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "scratch-resume").await;
    let dir = scratch.thread_dir("delivery-lead");
    std::fs::write(dir.join("keep"), "keep").unwrap();
    host.hint_scratch_archive("delivery-lead");
    received.recv().await.unwrap();
    let pass = host.scratch_archive_barrier();
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
    host.hint_scratch_archive("delivery-lead");
    let pass = host.scratch_archive_barrier();
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
