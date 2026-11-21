use super::*;
use crate::restart::tests::{checked_target, restart_metadata, restart_model};
use crate::{
    backend::HostBackend,
    lifecycle::{
        LifecycleDriver, LoopHost, OperatorSignals, idle_clock, product_exit::ProductOptions,
    },
};
use std::{future::Future, task::Poll};
use workbench_host::bootstrap::{BootstrapOptions, TestSeams};

async fn boot(dir: &test_socket_dir::SplitDir) -> BootstrappedHost {
    BootstrappedHost::open_for_core_with_test_seams(
        BootstrapOptions {
            config_dir: dir.path().into(),
            project_root: dir.path().into(),
            ownership_root: dir.path().join("owners"),
        },
        TestSeams {
            model: Arc::new(restart_model::Model::default()),
            sandbox: Arc::new(restart_model::Direct),
        },
        &crate::recovery_service::CoreGraphRecovery,
    )
    .await
    .unwrap()
}
fn context(
    dir: &test_socket_dir::SplitDir,
    host: &BootstrappedHost,
    driver: &LifecycleDriver,
) -> (ProductExitContext, OperatorSignals, Arc<HostBackend>) {
    let identity = core_instance::ConfigIdentity::new(dir.path()).unwrap();
    let launch = core_instance::LaunchGuard::acquire(&identity).unwrap();
    let instance = Arc::new(core_instance::Instance::acquire(&launch, dir.sockets()).unwrap());
    let mask = crate::signal_test_entry::mask().clone();
    let signals = OperatorSignals::new(&mask).unwrap();
    let backend = Arc::new(HostBackend::new(
        host.host.clone(),
        instance.identity.clone(),
    ));
    let context = ProductExitContext::new(
        instance,
        mask,
        signals.clone(),
        driver.control_drain(),
        ProductOptions::new(identity.config_dir().into(), false),
    );
    (context, signals, backend)
}
fn environment(context: &ProductExitContext) -> io::Result<FinalizationEnvironment> {
    Ok(FinalizationEnvironment::new(
        context.instance().clone(),
        context.mask().clone(),
        context.signals().clone(),
        context.control().clone(),
        context.options().successor_args(),
    ))
}
async fn sealed(driver: &LifecycleDriver) -> RestartId {
    let id = driver.restart.request_planned(Default::default()).unwrap();
    driver.restart.wait_sealed(id).await.unwrap();
    id
}
fn tx(host: &BootstrappedHost) -> RestartTransaction {
    let mut metadata = restart_metadata::metadata();
    metadata.old_builtin_tool_names = host.runtime.restart_builtin_tool_names();
    host.storage_writer()
        .mutate_restart(
            metadata.restart_id.clone(),
            None,
            storage::recovery::restart::RestartMutation::Create(Box::new(metadata)),
        )
        .unwrap()
}
async fn command(
    dir: &test_socket_dir::SplitDir,
    host: &BootstrappedHost,
    id: RestartId,
) -> (
    PreparedFinalization,
    oneshot::Receiver<Result<(), FinalizationError>>,
) {
    let (current, checked) = checked_target::checked_target(dir.path()).await;
    let (reply, receipt) = oneshot::channel();
    (
        PreparedFinalization {
            id,
            tx: tx(host),
            current,
            checked,
            reply,
        },
        receipt,
    )
}
async fn pending<T>(mut future: std::pin::Pin<&mut impl Future<Output = T>>) {
    assert!(
        std::future::poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx)))
            .await
            .is_pending()
    );
}
async fn masked_child() -> bool {
    if std::env::var_os("EVORCH_MASKED_TEST_CHILD").is_none() {
        let name = std::thread::current().name().unwrap().to_owned();
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--nocapture"])
            .env("EVORCH_MASKED_TEST_CHILD", "1")
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return true;
    }
    false
}
fn signal() {
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TERM)
        .unwrap();
}

#[tokio::test]
async fn unavailable_environment_and_rejected_start_never_reopen_delivery() {
    if masked_child().await {
        return;
    }
    fn unavailable(_: &ProductExitContext) -> io::Result<FinalizationEnvironment> {
        Err(io::Error::other("finalization unavailable"))
    }
    for invalid_tx in [false, true] {
        let dir = test_socket_dir::SplitDir::new();
        let mut host = boot(&dir).await;
        let mut driver = LifecycleDriver::new(&host);
        let (context, mut signals, _backend) = context(&dir, &host, &driver);
        let handle = driver.finalization();
        handle
            .install_environment_builder(if invalid_tx { environment } else { unavailable })
            .unwrap();
        let id = sealed(&driver).await;
        drop(handle.claim(id).unwrap());
        let (mut command, receipt) = command(&dir, &host, id).await;
        if invalid_tx {
            command.tx.phase = storage::recovery::restart::RestartPhase::Aborted;
        }
        {
            let mut loop_host = LoopHost::Owned {
                host: &mut host,
                context: &context,
                inbox: driver.finalization_inbox.take(),
            };
            assert!(loop_host.dispatch(command, &handle, &mut signals).await);
            assert_eq!(
                receipt.await.unwrap(),
                Err(if invalid_tx {
                    FinalizationError::Rejected
                } else {
                    FinalizationError::Unavailable
                })
            );
            assert!(matches!(handle.claim(id), Err(FinalizationError::Conflict)));
        }
        assert_eq!(
            driver.restart.snapshot().phase,
            crate::restart::RestartPhase::Sealed
        );
        driver.restart.transition_to_shutdown();
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn claim_is_fail_closed_single_use_and_survives_preparation_failure() {
    let dir = test_socket_dir::SplitDir::new();
    let mut host = boot(&dir).await;
    let driver = LifecycleDriver::new(&host);
    let handle = driver.finalization();
    let id = sealed(&driver).await;
    assert!(matches!(
        handle.claim(id),
        Err(FinalizationError::Unavailable)
    ));
    handle.install_environment_builder(environment).unwrap();
    assert_eq!(
        handle.install_environment_builder(environment),
        Err(FinalizationError::Conflict)
    );
    let permit = handle.claim(id).unwrap();
    assert!(matches!(handle.claim(id), Err(FinalizationError::Conflict)));
    drop(permit); // Includes Create ACK unknown/preparation failure: no latch reset.
    assert!(matches!(handle.claim(id), Err(FinalizationError::Conflict)));
    driver.restart.transition_to_shutdown();
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn accepted_is_immediate_and_lost_reply_is_unknown() {
    let (send, receipt) = oneshot::channel();
    send.send(Ok(())).unwrap();
    assert!(matches!(
        accepted(receipt).await,
        Ok(AdminExecutionOutcome::AwaitRestartRecovery)
    ));
    let (send, receipt) = oneshot::channel();
    drop(send);
    assert!(matches!(
        accepted(receipt).await,
        Err(FinalizationError::Unknown)
    ));
}
#[tokio::test]
async fn guard_drop_and_unwind_release_buffered_reply_and_bounded_delivery() {
    let dir = test_socket_dir::SplitDir::new();
    let mut host = boot(&dir).await;
    let mut driver = LifecycleDriver::new(&host);
    let id = sealed(&driver).await;
    let handle = driver.finalization();
    let (command, receipt) = command(&dir, &host, id).await;
    handle.send.send(command).await.ok().unwrap();
    let wait = handle.send.reserve();
    tokio::pin!(wait);
    pending(wait.as_mut()).await;
    let inbox = driver.finalization_inbox.take().unwrap();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = inbox;
            panic!("loop unwind");
        }))
        .is_err()
    );
    assert!(wait.await.is_err());
    assert_eq!(
        receipt.await.unwrap(),
        Err(FinalizationError::OwnerStopping)
    );
    driver.restart.transition_to_shutdown();
    host.shutdown().await.unwrap(); // driver/handle remain alive
}
#[tokio::test]
async fn simultaneous_signal_and_mail_start_zero_and_release_both_waiters() {
    if masked_child().await {
        return;
    }
    let dir = test_socket_dir::SplitDir::new();
    let mut host = boot(&dir).await;
    let mut driver = LifecycleDriver::new(&host);
    let (context, signals, backend) = context(&dir, &host, &driver);
    let id = sealed(&driver).await;
    let handle = driver.finalization();
    handle.install_environment_builder(environment).unwrap();
    drop(handle.claim(id).unwrap());
    let (command, receipt) = command(&dir, &host, id).await;
    handle.send.send(command).await.ok().unwrap();
    let waiting = handle.send.reserve();
    tokio::pin!(waiting);
    pending(waiting.as_mut()).await;
    // Latch the same reader before starting: both select inputs are ready.
    signal();
    context.signals().clone().received().await;
    let listener = tokio::net::UnixListener::bind(dir.sockets().join("loop")).unwrap();
    driver
        .run_owned_with_web(&mut host, backend, listener, None, signals, &context)
        .await
        .unwrap();
    assert!(waiting.await.is_err());
    assert!(matches!(
        receipt.await,
        Ok(Err(FinalizationError::OwnerStopping)) | Err(_)
    ));
    assert!(matches!(
        handle.claim(id),
        Err(FinalizationError::OwnerStopping)
    ));
    assert_eq!(
        driver.restart.snapshot().phase,
        crate::restart::RestartPhase::ShuttingDown
    );
    host.shutdown().await.unwrap();
}
struct FailedAccept {
    send: mpsc::Sender<PreparedFinalization>,
    queued: Mutex<Option<PreparedFinalization>>,
}
impl crate::lifecycle::loop_host::Listener for FailedAccept {
    async fn accept(&self) -> io::Result<(tokio::net::UnixStream, tokio::net::unix::SocketAddr)> {
        self.send
            .try_send(self.queued.lock().unwrap().take().unwrap())
            .ok()
            .unwrap();
        Err(io::Error::other("accept fault"))
    }
}
#[tokio::test]
async fn accept_error_releases_inbox_even_with_driver_retained() {
    if masked_child().await {
        return;
    }
    let dir = test_socket_dir::SplitDir::new();
    let mut host = boot(&dir).await;
    let mut driver = LifecycleDriver::new(&host);
    let (context, signals, backend) = context(&dir, &host, &driver);
    let handle = driver.finalization();
    let id = sealed(&driver).await;
    let (command, receipt) = command(&dir, &host, id).await;
    let listener = FailedAccept {
        send: handle.send.clone(),
        queued: Mutex::new(Some(command)),
    };
    let loop_host = LoopHost::Owned {
        host: &mut host,
        context: &context,
        inbox: driver.finalization_inbox.take(),
    };
    assert!(
        driver
            .run_loop(
                loop_host,
                backend,
                listener,
                false,
                None,
                signals,
                &idle_clock::TokioClock
            )
            .await
            .is_err()
    );
    assert!(handle.send.is_closed());
    assert_eq!(
        receipt.await.unwrap(),
        Err(FinalizationError::OwnerStopping)
    );
    host.shutdown().await.unwrap();
}
#[tokio::test]
async fn db_worker_barrier_does_not_block_control_drain_or_signal() {
    if masked_child().await {
        return;
    }
    let dir = test_socket_dir::SplitDir::new();
    let mut host = boot(&dir).await;
    let mut driver = LifecycleDriver::new(&host);
    let (context, signals, backend) = context(&dir, &host, &driver);
    let id = sealed(&driver).await;
    let handle = driver.finalization();
    handle.install_environment_builder(environment).unwrap();
    let permit = handle.claim(id).unwrap();
    let (entered, ready) = oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let db = tokio::task::spawn_blocking(move || {
        entered.send(()).unwrap();
        held.recv().unwrap();
        drop(permit);
    });
    ready.await.unwrap();
    // A held join models a client still draining. This seam is test-only in
    // lifecycle, never in product entry/bootstrap; the real drain select runs.
    let (draining, entered_drain) = oneshot::channel();
    let (_release_drain, hold_drain) = oneshot::channel();
    driver.drain_barrier = Some((draining, hold_drain));
    let (command, receipt) = command(&dir, &host, id).await;
    let listener = tokio::net::UnixListener::bind(dir.sockets().join("loop")).unwrap();
    let control = driver.control_drain();
    let (reply, drained) = oneshot::channel();
    control
        .0
        .send(crate::lifecycle::control::Request {
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(120),
            reply,
        })
        .unwrap();
    // Hold mail delivery until the drain is entered; it remains queued then.
    let action = async {
        entered_drain.await.unwrap();
        handle.send.send(command).await.ok().unwrap();
        let waiting = handle.send.reserve();
        tokio::pin!(waiting);
        pending(waiting.as_mut()).await;
        signal();
        assert!(waiting.await.is_err());
    };
    let running = driver.run_owned_with_web(&mut host, backend, listener, None, signals, &context);
    let (outcome, ()) = tokio::join!(running, action);
    outcome.unwrap();
    assert!(drained.await.unwrap().is_err());
    assert!(matches!(
        receipt.await,
        Ok(Err(FinalizationError::OwnerStopping)) | Err(_)
    ));
    assert!(!db.is_finished());
    host.shutdown().await.unwrap();
    release.send(()).unwrap();
    db.await.unwrap();
}
#[tokio::test]
async fn owned_start_receipt_does_not_wait_for_stage_four() {
    owned_start(false).await;
}
#[tokio::test]
async fn capture_then_immediate_signal_retains_planned_cleanup() {
    owned_start(true).await;
}
async fn owned_start(immediate_signal: bool) {
    if masked_child().await {
        return;
    }
    let dir = test_socket_dir::SplitDir::new();
    let mut host = boot(&dir).await;
    let mut driver = LifecycleDriver::new(&host);
    let (context, signals, backend) = context(&dir, &host, &driver);
    let id = sealed(&driver).await;
    let handle = driver.finalization();
    handle.install_environment_builder(environment).unwrap();
    let (current, checked) = checked_target::checked_target(dir.path()).await;
    let transaction = tx(&host);
    let start = handle
        .claim(id)
        .unwrap()
        .start(transaction, current, checked);
    let listener = tokio::net::UnixListener::bind(dir.sockets().join("loop")).unwrap();
    {
        let running =
            driver.run_owned_with_web(&mut host, backend, listener, None, signals, &context);
        tokio::pin!(running);
        tokio::select! { result = &mut running => panic!("premature exit {result:?}"), receipt = start => assert!(matches!(receipt, Ok(AdminExecutionOutcome::AwaitRestartRecovery))) }
        assert!(matches!(handle.claim(id), Err(FinalizationError::Conflict)));
        assert_eq!(
            handle.core.snapshot().phase,
            crate::restart::RestartPhase::Capturing
        );
        if !immediate_signal {
            // No signal until the actual fixed-deadline worker join has completed.
            // Target metadata intentionally differs, so the finalizer fails later,
            // never execs, and must retain its planned cleanup ownership.
            tokio::select! { result = &mut running => panic!("premature exit {result:?}"), _ = handle.core.wait_finalization(id) => () }
            crate::restart::tests::finalization::assert_workers_joined(&handle.core);
        }
        signal();
        running.await.unwrap();
    }
    let shutdown = host.shutdown().await;
    if !immediate_signal {
        assert!(shutdown.is_err()); // known failure after WorkersJoined
    }
    // An immediately observed signal can finish cleanup normally before the
    // later target failure. Neither result permits aborting the finalizer:
    // durable invalidation and its completion must still be observable.
    assert!(handle.core.wait_finalization(id).await.is_err());
    let saved = storage::Database::open(host.storage_config())
        .unwrap()
        .restart_transaction("restart")
        .unwrap()
        .unwrap();
    assert_eq!(
        saved.phase,
        storage::recovery::restart::RestartPhase::Invalidated
    );
}

#[tokio::test]
async fn shared_loop_discards_unserviceable_receiver() {
    if masked_child().await {
        return;
    }
    let dir = test_socket_dir::SplitDir::new();
    let mut host = boot(&dir).await;
    let mut driver = LifecycleDriver::new(&host);
    let (_context, signals, backend) = context(&dir, &host, &driver);
    let handle = driver.finalization();
    let listener = tokio::net::UnixListener::bind(dir.sockets().join("loop")).unwrap();
    {
        let running = driver.run_with_web(&host, backend, listener, None, signals);
        tokio::pin!(running);
        pending(running.as_mut()).await;
        assert!(handle.send.is_closed());
    }
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn observed_signal_prevents_dispatch_even_when_mail_already_selected() {
    if masked_child().await {
        return;
    }
    let dir = test_socket_dir::SplitDir::new();
    let mut host = boot(&dir).await;
    let mut driver = LifecycleDriver::new(&host);
    let (context, mut signals, _backend) = context(&dir, &host, &driver);
    let handle = driver.finalization();
    handle.install_environment_builder(environment).unwrap();
    let id = sealed(&driver).await;
    let (command, receipt) = command(&dir, &host, id).await;
    signal();
    context.signals().clone().received().await;
    {
        let mut loop_host = LoopHost::Owned {
            host: &mut host,
            context: &context,
            inbox: driver.finalization_inbox.take(),
        };
        assert!(!loop_host.dispatch(command, &handle, &mut signals).await);
    }
    assert!(receipt.await.is_err());
    assert_eq!(
        driver.restart.snapshot().phase,
        crate::restart::RestartPhase::Sealed
    );
    driver.restart.transition_to_shutdown();
    host.shutdown().await.unwrap();
}
