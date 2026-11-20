use super::*;
use tokio::sync::{Notify, mpsc};
use workbench_authority::IngressContext;
use workbench_protocol::{
    envelope::{ReplyEnvelope, RequestEnvelope},
    errors::WorkbenchError,
    events::Subscribe,
    handshake::ServerHello,
    ids::ClientId,
};
use workbench_web::{BackendSubscription, RestartTasks, WebSession, WorkbenchBackend};

struct HeldBackend {
    inner: Arc<HostBackend>,
    entered: mpsc::UnboundedSender<()>,
    release: Notify,
    request_done: std::sync::atomic::AtomicBool,
    drain_entered: mpsc::UnboundedSender<()>,
    deliveries: RestartTasks,
}
impl WorkbenchBackend for HeldBackend {
    fn protect_output(&self, value: serde_json::Value) -> serde_json::Value {
        self.inner.protect_output(value)
    }
    async fn handle_request(
        &self,
        session: &WebSession,
        request: RequestEnvelope,
        provenance: IngressContext,
    ) -> ReplyEnvelope {
        self.entered.send(()).unwrap();
        self.release.notified().await;
        let reply = self
            .inner
            .handle_request(session, request, provenance)
            .await;
        self.request_done.store(true, Ordering::SeqCst);
        reply
    }
    async fn subscribe(
        &self,
        client: ClientId,
        subscribe: Subscribe,
        _context: IngressContext,
    ) -> Result<BackendSubscription, WorkbenchError> {
        self.inner.subscribe(client, subscribe, _context).await
    }
    async fn server_hello(&self, _context: &IngressContext) -> Result<ServerHello, WorkbenchError> {
        self.inner.server_hello(_context).await
    }
    async fn drain_for_restart(&self) -> Result<(), WorkbenchError> {
        assert!(
            self.request_done.load(Ordering::SeqCst),
            "request must be joined before backend delivery drain"
        );
        self.inner.drain_for_restart().await?;
        self.drain_entered.send(()).unwrap();
        self.deliveries.drain().await.unwrap();
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Release,
    Deadline,
    Shutdown,
}
pub(super) async fn cell(mode: Mode) {
    let expire = mode == Mode::Deadline;
    eprintln!("held-web {mode:?}: boot");
    let model = Arc::new(Model::default());
    let (dir, mut host) = boot_composition(model.clone(), true).await;
    let config = ConfigIdentity::new(dir.path()).unwrap();
    let launch = LaunchGuard::acquire(&config).unwrap();
    let instance = Arc::new(Instance::acquire(&launch, Path::new("/tmp")).unwrap());
    drop(launch);
    let mut driver = LifecycleDriver::new(&host);
    let core = driver.restart.clone();
    let backend = Arc::new(HostBackend::new(
        host.host.clone(),
        instance.identity.clone(),
    ));
    let (entered, mut request_entered) = mpsc::unbounded_channel();
    let (drain_entered, mut draining) = mpsc::unbounded_channel();
    let web_backend = Arc::new(HeldBackend {
        inner: backend.clone(),
        entered,
        release: Notify::new(),
        request_done: false.into(),
        drain_entered,
        deliveries: RestartTasks::default(),
    });
    let delivery_release = Arc::new(Notify::new());
    let release = delivery_release.clone();
    let (delivery_done, finished) = tokio::sync::oneshot::channel();
    drop(
        web_backend
            .deliveries
            .spawn(async move {
                release.notified().await;
                delivery_done.send(()).unwrap();
            })
            .unwrap(),
    );
    let web = workbench_web::WebServer::start(
        workbench_web::WebServerConfig {
            enabled: true,
            bind: ([127, 0, 0, 1], 0).into(),
            // Production requires positive grace; await TCP EOF, not time.
            shutdown_grace: std::time::Duration::from_secs(1),
            ..Default::default()
        },
        web_backend.clone(),
    )
    .await
    .unwrap()
    .unwrap();
    let address = web.local_addr();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let body = serde_json::json!({
        "request_id":"held", "session_id":"web",
        "kind":"query", "payload":{"type":"lifecycle","payload":{"type":"get_core_status"}}
    })
    .to_string();
    client.write_all(format!(
        "POST /api/v1/request HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()
    ).as_bytes()).await.unwrap();
    request_entered.recv().await.unwrap();
    eprintln!("held-web {mode:?}: request accepted");
    let listener = instance.bind().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let signals = OperatorSignals::new(crate::signal_test_entry::mask()).unwrap();
    let (mut target, mut metadata) = target(dir.path()).await;
    metadata.config_identity = instance.identity.clone();
    metadata.old_epoch = backend.hello().instance_epoch.0;
    metadata.successor_epoch = metadata.old_epoch.checked_add(1).unwrap();
    target = target.with_environment(FinalizationEnvironment::new(
        instance.clone(),
        crate::signal_test_entry::mask().clone(),
        signals.clone(),
        driver.control_drain(),
        vec!["--config-dir".into(), dir.path().as_os_str().to_owned()],
    ));
    // This cell audits web drain and returned-failure cleanup, not successor
    // bootstrap. Preserve every preparation/drain stage, then force a real
    // execve ENOENT instead of replacing the worker with the describe-only mock.
    let entry_path = dir.path().join("target");
    target.after = Some(Arc::new(move |now| {
        if now == ExitStage::OwnerStopped {
            std::fs::remove_file(&entry_path).unwrap();
        }
    }));
    let reached = Arc::new(Mutex::new(Vec::new()));
    let trace = reached.clone();
    target.observe = Some(Arc::new(move |now| trace.lock().unwrap().push(now)));
    let (_, id) = owned_chat(&host, &core).await;
    let tx = create(&host, metadata);
    core.start_finalization(id, &mut host, tx, target).unwrap();
    let before_owner = || {
        assert_eq!(
            core.0.state.lock().unwrap().completed_stage,
            Some(ExitStage::Armed)
        );
        assert_eq!(
            *reached.lock().unwrap(),
            [
                ExitStage::TargetChecked,
                ExitStage::OwnersHeld,
                ExitStage::Armed,
                ExitStage::ExecPrepared
            ]
        );
        assert_eq!(model.0.load(Ordering::SeqCst), 1);
    };
    let check = async {
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.is_empty(), "connection ended without a reply");
        assert!(!web_backend.request_done.load(Ordering::SeqCst));
        before_owner();
        web_backend.release.notify_one();
        // This event can only follow connection + adapter task joins. Backend
        // delivery remains explicitly held, so a connection receipt is not enough.
        draining.recv().await.unwrap();
        eprintln!("held-web {mode:?}: delivery drain entered");
        before_owner();
        if expire {
            // Advance the ORIGINAL fixed boot budget, not a per-drain timeout.
            tokio::time::pause();
            tokio::time::advance(std::time::Duration::from_secs(
                core_instance::handoff::boot::BOOT_TIMEOUT_SECS,
            ))
            .await;
        } else if mode == Mode::Shutdown {
            signal("INT");
        } else {
            delivery_release.notify_one();
        }
        assert!(core.wait_finalization(id).await.is_err());
        eprintln!("held-web {mode:?}: failed closed");
        if expire {
            tokio::time::resume();
        }
        if mode != Mode::Release {
            before_owner();
            heartbeat(&host).await;
        } else {
            assert_eq!(
                core.0.state.lock().unwrap().completed_stage,
                Some(ExitStage::OwnerStopped)
            );
        }
        reclaimed(&host, &core, true).await;
        assert_eq!(
            core.snapshot().phase,
            if mode == Mode::Shutdown {
                RestartPhase::ShuttingDown
            } else {
                RestartPhase::FailedClosed
            }
        );
        assert!(
            !LaunchGuard::acquire(&config)
                .unwrap()
                .lifetime_available()
                .unwrap()
        );
        if mode != Mode::Shutdown {
            signal("TERM");
        }
    };
    let (outcome, ()) = tokio::join!(
        driver.run_with_web(&host, backend, listener, Some(web), signals),
        check
    );
    outcome.unwrap();
    if mode != Mode::Release {
        // Deadline/shutdown failure returned the signal owner while delivery was STILL
        // held. Normal shutdown did not abort the accepted detached task.
        delivery_release.notify_one();
    }
    web_backend.deliveries.drain().await.unwrap();
    finished.await.unwrap();
    assert!(host.shutdown().await.is_err());
    println!("M2-CELL held-web {mode:?}");
    std::io::stdout().flush().unwrap();
}
