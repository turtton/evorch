use super::*;
use crate::{
    backend::HostBackend,
    lifecycle::{LifecycleDriver, OperatorSignals},
    restart::FinalizationEnvironment,
};
use core_instance::{ConfigIdentity, Instance, LaunchGuard};
use std::{
    io::{BufRead, Write},
    os::unix::fs::FileTypeExt,
    process::Stdio,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
const WORKER: &str = "restart::tests::finalization::later::last::final_three_process_worker";
mod composition;
mod web;

#[derive(Clone, Copy, Debug)]
enum Action {
    Race(Race),
    Signal(&'static str),
    Success,
}
fn signal(name: &str) {
    rustix::process::kill_process(
        rustix::process::getpid(),
        if name == "INT" {
            rustix::process::Signal::INT
        } else {
            rustix::process::Signal::TERM
        },
    )
    .unwrap();
}
// Each group retains every cell and uses a fresh masked subprocess. Keeping
// independent stages in separate runner cases avoids serial ELF/SQLite fixture
// work accumulating against nextest's centralized hang watchdog.
#[tokio::test]
async fn stage_nine_failure_stop_shutdown_matrix() {
    matrix("9", 5, 0).await;
}
#[tokio::test]
async fn stage_ten_failure_stop_shutdown_matrix() {
    matrix("10", 5, 0).await;
}
#[tokio::test]
async fn stage_eleven_partial_stop_failure_stop_shutdown_matrix() {
    matrix("11", 5, 0).await;
}
#[tokio::test]
async fn final_signal_restore_and_web_drain_matrix() {
    matrix("boundaries", 10, 6).await;
}
async fn matrix(group: &str, expected_cells: usize, expected_signals: usize) {
    let repeats = if group.starts_with("composition:") {
        std::env::var("EVORCH_M2_SAMPLES")
            .ok()
            .map(|n| n.parse::<usize>().unwrap())
            .unwrap_or(1)
    } else {
        1
    };
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", WORKER, "--nocapture"])
        .env("EVORCH_MASKED_TEST_CHILD", "1")
        .env("EVORCH_M2_MATRIX_GROUP", group)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut cells = 0;
    let mut signals = 0;
    while let Some(line) = lines.next_line().await.unwrap() {
        eprintln!("M2 child: {line}");
        if let Some(name) = line.strip_prefix("M2-SIGNAL ") {
            rustix::process::kill_process(
                rustix::process::Pid::from_raw(child.id().unwrap() as i32).unwrap(),
                if name == "INT" {
                    rustix::process::Signal::INT
                } else {
                    rustix::process::Signal::TERM
                },
            )
            .unwrap();
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"delivered\n")
                .await
                .unwrap();
            signals += 1;
        }
        if line.starts_with("M2-CELL ") {
            cells += 1;
        }
    }
    drop(child.stdin.take());
    assert!(child.wait().await.unwrap().success());
    assert_eq!(cells, expected_cells * repeats);
    assert_eq!(signals, expected_signals);
}
#[tokio::test]
#[ignore = "single-threaded-entry subprocess, invoked by the matrix parent"]
async fn final_three_process_worker() {
    let group = std::env::var("EVORCH_M2_MATRIX_GROUP").unwrap();
    #[cfg(feature = "bootstrap-test-support")]
    if group == "composition_drain_deadlines" {
        composition::drain::run().await;
        return;
    }
    if group == "composition_faults" {
        composition::faults().await;
        return;
    }
    if group.starts_with("composition") {
        composition::run().await;
        return;
    }
    if group != "boundaries" {
        let stage = match group.as_str() {
            "9" => ExitStage::ExecPrepared,
            "10" => ExitStage::SignalsStopped,
            "11" => ExitStage::OwnerStopped,
            _ => panic!("unknown matrix group"),
        };
        for race in [
            Race::Failure,
            Race::Stop,
            Race::Shutdown,
            Race::FailureStop,
            Race::FailureShutdown,
        ] {
            cell(stage, Action::Race(race), false).await;
        }
        return;
    }
    cell(ExitStage::OwnerStopped, Action::Success, true).await;
    web::cell(web::Mode::Release).await;
    web::cell(web::Mode::Deadline).await;
    web::cell(web::Mode::Shutdown).await;
    for (stage, after) in [
        (ExitStage::SignalsStopped, false),
        (ExitStage::OwnerStopped, false),
        (ExitStage::OwnerStopped, true),
    ] {
        for name in ["INT", "TERM"] {
            cell(stage, Action::Signal(name), after).await;
        }
    }
}
async fn cell(stage: ExitStage, action: Action, after_owner: bool) {
    cell_loaded(stage, action, after_owner, None).await;
}
async fn cell_loaded(
    stage: ExitStage,
    action: Action,
    after_owner: bool,
    load: Option<&composition::Load>,
) {
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
    let web = if matches!(action, Action::Success) {
        workbench_web::WebServer::start(
            workbench_web::WebServerConfig {
                enabled: true,
                bind: ([127, 0, 0, 1], 0).into(),
                ..Default::default()
            },
            backend.clone(),
        )
        .await
        .unwrap()
    } else {
        None
    };
    #[cfg(feature = "bootstrap-test-support")]
    if let (Some(load), Some(web)) = (load, web.as_ref()) {
        load.observe_web(web);
    }
    let web_address = web.as_ref().map(|s| s.local_addr());
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
    let web_clients = if let Some(load) = load {
        Some(load.open_web(web_address.unwrap()).await)
    } else {
        None
    };
    let runs = owner_runs(&host, if load.is_some() { 3 } else { 1 }).await;
    if let Some(load) = load {
        load.prepare(&host).await;
    }
    let id = seal_runs(&core, &runs).await;
    let run = runs[0];
    let (arrived, ready) = tokio::sync::oneshot::channel();
    let (release, receive) = std::sync::mpsc::channel();
    let barrier = Mutex::new(Some((arrived, receive)));
    let hook = Arc::new(move |now| {
        if now == stage {
            let (arrived, receive) = barrier.lock().unwrap().take().unwrap();
            arrived.send(()).unwrap();
            receive.recv().unwrap();
        }
    });
    if after_owner {
        target.after = Some(hook);
    } else {
        target.observe = Some(hook);
    }
    let control_client = if web_address.is_some() {
        let stream = tokio::net::UnixStream::connect(&instance.socket)
            .await
            .unwrap();
        let (send, ready) = tokio::sync::oneshot::channel();
        let (release, receive) = std::sync::mpsc::channel();
        let barrier = Mutex::new(Some((send, receive)));
        // Force a real accepted connection to precede control drain, rather
        // than accepting a receipt from an empty connection JoinSet.
        target.observe = Some(Arc::new(move |stage| {
            if stage == ExitStage::ExecPrepared {
                let (send, receive) = barrier.lock().unwrap().take().unwrap();
                send.send(()).unwrap();
                receive.recv().unwrap();
            }
        }));
        Some((stream, ready, release))
    } else {
        None
    };
    let tx = create(&host, metadata);
    let reached = Arc::new(Mutex::new(Vec::new()));
    let previous = target.observe.take();
    let trace = reached.clone();
    target.observe = Some(Arc::new(move |now| {
        trace.lock().unwrap().push(now);
        if let Some(previous) = &previous {
            previous(now);
        }
    }));
    core.start_finalization(id, &mut host, tx, target).unwrap();
    let check = async {
        if let Some(load) = load {
            load.release(&core, &host).await;
        }
        let mut control_client = if let Some((mut stream, ready, release)) = control_client {
            use workbench_protocol::{handshake::*, ids::ClientId};
            ready.await.unwrap_or_else(|_| {
                panic!(
                    "finalization ended before control checkpoint: {:?}",
                    saved(&host)
                )
            });
            let hello = ClientHello {
                supported_versions: vec![ProtocolVersion::CURRENT],
                required_capabilities: vec![],
                optional_capabilities: vec![],
                client_build: BuildInfo {
                    name: "m2-drain".into(),
                    version: "1".into(),
                    revision: None,
                },
                client_instance_id: ClientId::new("m2-drain").unwrap(),
                desired_core: None,
            };
            stream
                .write_all(&workbench_protocol::framing::encode_frame(&hello, 16 * 1024).unwrap())
                .await
                .unwrap();
            let len = stream.read_u32().await.unwrap();
            assert!(len <= 16 * 1024);
            let mut reply = vec![0; len as usize];
            stream.read_exact(&mut reply).await.unwrap();
            assert!(matches!(
                serde_json::from_slice::<HandshakeReply>(&reply).unwrap(),
                HandshakeReply::Accepted(_)
            ));
            release.send(()).unwrap();
            Some(stream)
        } else {
            None
        };
        assert!(
            ready.await.is_ok(),
            "{stage:?}/{action:?}/{after_owner}: {:?}",
            saved(&host)
        );
        assert_eq!(model.0.load(Ordering::SeqCst), runs.len());
        assert!(
            !LaunchGuard::acquire(&config)
                .unwrap()
                .lifetime_available()
                .unwrap()
        );
        let mut withheld_reader = None;
        let socket = dir
            .path()
            .join("o")
            .join(format!("{}.sock", host.owner.owner_id()));
        let displaced = socket.with_extension("saved");
        let failure = matches!(
            action,
            Action::Race(Race::Failure | Race::FailureStop | Race::FailureShutdown)
        );
        if failure {
            match stage {
                ExitStage::ExecPrepared => {
                    std::fs::create_dir(config.restart_receipt_path()).unwrap()
                }
                ExitStage::SignalsStopped => withheld_reader = Some(signals.stop().unwrap()),
                ExitStage::OwnerStopped => {
                    std::fs::rename(&socket, &displaced).unwrap();
                    std::fs::write(&socket, "not the owned socket").unwrap();
                }
                _ => unreachable!(),
            }
        }
        match action {
            Action::Race(Race::Shutdown | Race::FailureShutdown) => {
                core.transition_to_shutdown();
                host.runtime.stop(run, StopScope::SelfOnly).unwrap();
                host.runtime.wait(run).await.unwrap();
                assert_eq!(core.snapshot().phase, RestartPhase::ShuttingDown);
            }
            Action::Race(Race::Stop | Race::FailureStop) if !after_owner => {
                assert!(matches!(
                    host.runtime.stop(run, StopScope::SelfOnly),
                    Err(RuntimeError::RestartBusy)
                ));
                assert!(matches!(
                    RestartStopHandler::stop(&core, run, StopScope::SelfOnly),
                    Err(RuntimeError::RestartBusy)
                ));
                assert!(core.cancel(id).is_err());
            }
            Action::Signal(name) => {
                println!("M2-SIGNAL {name}");
                std::io::stdout().flush().unwrap();
                tokio::task::spawn_blocking(|| {
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line).unwrap();
                    assert_eq!(line, "delivered\n");
                })
                .await
                .unwrap();
            }
            _ => (),
        }
        release.send(()).unwrap();
        assert!(core.wait_finalization(id).await.is_err());
        drop(withheld_reader);
        assert!(matches!(
            core.snapshot().phase,
            RestartPhase::FailedClosed | RestartPhase::ShuttingDown
        ));
        if let Some(load) = load {
            load.verify(&host, &runs);
        }
        reclaimed(&host, &core, true).await;
        let completed = core.0.state.lock().unwrap().completed_stage.unwrap();
        let interrupted =
            failure || matches!(action, Action::Race(Race::Shutdown) | Action::Signal(_));
        let expected = if matches!(action, Action::Race(Race::Shutdown))
            && stage == ExitStage::SignalsStopped
        {
            // Reader transfer completes before its prohibition checkpoint. The
            // factual stage is retained, but stage 11 must never be entered.
            ExitStage::SignalsStopped
        } else if interrupted && !after_owner {
            match stage {
                ExitStage::ExecPrepared => ExitStage::Armed,
                ExitStage::SignalsStopped => ExitStage::ExecPrepared,
                ExitStage::OwnerStopped => ExitStage::SignalsStopped,
                _ => unreachable!(),
            }
        } else {
            ExitStage::OwnerStopped
        };
        assert_eq!(completed, expected, "{stage:?}/{action:?}/{after_owner}");
        let last_reached = if interrupted && !after_owner {
            stage
        } else {
            ExitStage::OwnerStopped
        };
        assert_eq!(
            *reached.lock().unwrap(),
            [
                ExitStage::TargetChecked,
                ExitStage::OwnersHeld,
                ExitStage::Armed,
                ExitStage::ExecPrepared,
                ExitStage::SignalsStopped,
                ExitStage::OwnerStopped,
            ]
            .into_iter()
            .filter(|now| *now <= last_reached)
            .collect::<Vec<_>>(),
            "failure must not enter a later stage"
        );
        if matches!(action, Action::Race(Race::Stop) | Action::Success) {
            assert_eq!(completed, ExitStage::OwnerStopped);
            assert!(
                std::fs::symlink_metadata(&socket)
                    .unwrap()
                    .file_type()
                    .is_socket()
            );
            let receipt = config.read_restart_receipt().unwrap().unwrap();
            assert_eq!(receipt.transaction_id, "restart");
            assert_eq!(
                receipt.status,
                core_instance::handoff::receipt::BootStatus::Restarting
            );
            if let Some(address) = web_address {
                assert!(
                    std::net::TcpStream::connect(address).is_err(),
                    "web listener was joined before owner stop"
                );
            }
            if let Some(client) = &mut control_client {
                assert_eq!(client.read(&mut [0; 1]).await.unwrap(), 0);
                assert!(
                    tokio::net::UnixStream::connect(&instance.socket)
                        .await
                        .is_err()
                );
            }
        }
        if failure && stage == ExitStage::OwnerStopped && displaced.exists() {
            // Only remove our own injected replacement, never the worker's socket.
            assert_eq!(
                std::fs::read_to_string(&socket).unwrap(),
                "not the owned socket"
            );
            std::fs::remove_file(&socket).unwrap();
            std::fs::rename(&displaced, &socket).unwrap();
        }
        let partial_owner_stop = failure
            && stage == ExitStage::OwnerStopped
            && !matches!(action, Action::Race(Race::FailureShutdown));
        if partial_owner_stop {
            // The worker joined, but identity-guarded unlink refused our
            // replacement. This is NOT a pre-stop failure: recovery must stay
            // sticky/unconfirmed, without dropping lifetime or reopening gates.
            assert!(matches!(
                host.owner.restart_worker_failed_closed(),
                Err(runtime::ownership::RegistryError::Preparation(_))
            ));
            assert_eq!(completed, ExitStage::SignalsStopped);
        } else if completed < ExitStage::OwnerStopped {
            heartbeat(&host).await;
        }
        assert_eq!(model.0.load(Ordering::SeqCst), runs.len());
        assert!(host.restart_producers().iter().all(|gate| !gate.is_open()));
        assert!(
            !LaunchGuard::acquire(&config)
                .unwrap()
                .lifetime_available()
                .unwrap()
        );
        // Every failure path must restore a functioning normal signal reader.
        // No timeout or sleep: the lifecycle itself acknowledges the shutdown.
        if !matches!(action, Action::Signal(_)) {
            signal("TERM");
        }
    };
    let (outcome, ()) = tokio::join!(
        driver.run_with_web(&host, backend, listener, web, signals.clone()),
        check
    );
    outcome.unwrap();
    if let Some(client) = web_clients {
        client.await.unwrap();
    }
    assert_eq!(core.snapshot().phase, RestartPhase::ShuttingDown);
    assert!(host.shutdown().await.is_err());
    drop(instance);
    println!("M2-CELL {stage:?} {action:?} {after_owner}");
    std::io::stdout().flush().unwrap();
}

async fn heartbeat(host: &BootstrappedHost) {
    // Subscribe AFTER failure cleanup: a stale pre-failure event cannot prove
    // that the worker continues to renew the same held generation.
    let mut events = host.runtime.event_bus().subscribe();
    let idle = host.owner.attach("idle").unwrap();
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(event_bus::RecvError::Lagged(_)) => continue,
            Err(error) => panic!("heartbeat stream ended: {error:?}"),
        };
        if matches!(event.kind, event_bus::EventKind::Ownership(event)
            if event.thread_id == "idle" && event.owner_id == idle.lease.owner_id
                && event.generation == idle.lease.generation
                && event.action == event_bus::OwnershipAction::Heartbeat)
        {
            break;
        }
    }
}
