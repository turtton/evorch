use core_instance::{
    ConfigIdentity, Instance, LaunchGuard,
    handoff::{
        BlockedShutdownSignals,
        entry::{BootMode, EntryOptions},
    },
};
use evorch_core::{
    backend::HostBackend,
    lifecycle::{LifecycleDriver, OperatorSignals, shutdown_watchdog::run_with_runtime},
    planned_bootstrap::PlannedBoot,
    restart::{FinalizationEnvironment, FinalizationTarget},
};
use std::{
    collections::BTreeMap,
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use storage::recovery::restart::*;
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost};

struct Block {
    phase: &'static str,
    hold_checkpoint: bool,
    active: AtomicBool,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}
#[derive(Clone)]
struct Observe(Arc<Block>);
struct Fields(bool, bool, &'static str);
impl tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "phase" {
            self.0 |= value == self.2;
            self.1 |= value == "planned_shutdown_cleanup_begin";
        }
    }
    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}
impl<S: tracing::Subscriber> Layer<S> for Observe {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "restart_drain" {
            return;
        }
        let mut fields = Fields(false, false, self.0.phase);
        event.record(&mut fields);
        if fields.1 {
            say("SHUTDOWN_CLEANUP");
        }
        if fields.0 && self.0.active.swap(false, Ordering::AcqRel) {
            self.0
                .entered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            // Block the real worker that emitted this owned-drain checkpoint.
            if self.0.hold_checkpoint {
                self.0.release.lock().unwrap().recv().unwrap();
            }
        }
    }
}
fn say(line: &str) {
    println!("{line}");
    std::io::stdout().flush().unwrap();
}
/// Extension point of tests/core_browser_expiry (B3-08b): a leaf that includes
/// this module registers a fixture before `run`. It starts after bootstrap and
/// before the stall is armed, owns real children through the worker, and is
/// forgotten by the worker. Unset in every other includer (no-op).
#[allow(dead_code)]
pub(super) type FixtureFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Box<dyn std::any::Any + Send>, String>> + Send>,
>;
#[allow(dead_code)]
pub(super) type FixtureStart = fn(fn(&str)) -> FixtureFuture;
#[allow(dead_code)]
pub(super) static OWNED_FIXTURE: std::sync::OnceLock<FixtureStart> = std::sync::OnceLock::new();
pub(super) fn run(
    args: Vec<std::ffi::OsString>,
) -> Result<(), workbench_host::bootstrap::BootstrapError> {
    let mask = Arc::new(BlockedShutdownSignals::at_entry()?);
    let entry = EntryOptions::parse(args)?;
    // Inspect before opening configuration, DB, or any new descriptors.
    let inherited = match entry.mode {
        BootMode::Cold => None,
        BootMode::PlannedHandoff(handoff) => Some(unsafe { handoff.inspect()? }),
    };
    let root = PathBuf::from(std::env::var_os("EVORCH_WATCHDOG_ROOT").unwrap());
    let sockets = PathBuf::from(std::env::var_os("EVORCH_WATCHDOG_SOCKETS").unwrap());
    let config = root.join("config");
    let project = root.join("project");
    let case = std::env::var("EVORCH_WATCHDOG_CASE")?;
    // B3-07 (tests/core_expiry) drives unchanged cold stalls from its own controller:
    // stopped-join (stage 11, no storage-drain deadline) for the cells that need a
    // long capture-only window, flush for the pending-signal gate. The original
    // name selects the owned-child fixture and the gate.
    let case = match case.as_str() {
        "expiry_signal_first" => "cold_flush".to_owned(),
        other if other.starts_with("expiry_") => "cold_stopped_join".to_owned(),
        _ => case,
    };
    let product_case = case.starts_with("product_");
    let options = BootstrapOptions {
        config_dir: config.clone(),
        project_root: project.clone(),
        ownership_root: if product_case {
            sockets.join("evorch/threads")
        } else {
            sockets.join("owners")
        },
    };
    let identity = ConfigIdentity::new(&config)?;
    let cleanup_returns = case == "cold_cleanup_returns";
    let (entered, seen) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let block = Arc::new(Block {
        phase: match case.as_str() {
            "cold_final_usage" => "final_flush_begin",
            "cold_writer_barrier" => "writer_barrier_begin",
            "cold_stopped_join" => "owner_stop_begin",
            "cold_control_web" => "web-only",
            _ => "flush_begin",
        },
        hold_checkpoint: case != "cold_stopped_join",
        active: AtomicBool::new(false),
        entered: Mutex::new(Some(entered)),
        release: Mutex::new(blocked),
    });
    tracing_subscriber::registry()
        .with(Observe(block.clone()))
        .init();
    let ipc = if product_case {
        sockets.clone()
    } else {
        sockets.join("ipc")
    };
    let outcome = if let Some(inherited) = inherited {
        let (boot, proof) = PlannedBoot::adopt_entry(identity, &ipc, inherited)?;
        let inbox = mask.inbox()?;
        run_with_runtime(
            (boot, proof),
            inbox,
            |(boot, proof), runtime, inbox, observer| {
                runtime.block_on(async move {
                    let pending = boot.bootstrap(options, proof, &inbox).await;
                    let listener = pending.bind_control()?;
                    let at_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_millis()
                        .try_into()?;
                    let (mut host, instance, listener, _) =
                        pending.confirm_ready(listener, at_ms, &inbox).await;
                    host.activate_after_ready(&evorch_core::recovery_service::CoreGraphRecovery)
                        .await?;
                    say("ADOPTED 2");
                    operate(
                        host, instance, listener, inbox, observer, mask, true, block, seen,
                        release, case,
                    )
                    .await
                })
            },
        )
        .map_err(|failure| failure.error)?
    } else {
        std::fs::create_dir_all(&config)?;
        std::fs::create_dir_all(&project)?;
        let mut sidebar = workspace_state::SidebarState::default();
        sidebar.add_project(workspace_state::ProjectId::new("p"), "watchdog", &project)?;
        std::fs::write(config.join("sidebar.json"), serde_json::to_vec(&sidebar)?)?;
        let launch = LaunchGuard::acquire(&identity)?;
        let instance = Arc::new(Instance::acquire(&launch, &ipc)?);
        drop(launch);
        if product_case {
            let idle = if case == "product_planned_idle" {
                0
            } else {
                3600
            };
            std::fs::write(
                config.join("config.toml"),
                format!("[core]\nidle_grace_secs = {idle}\n"),
            )?;
        }
        let inbox = mask.inbox()?;
        run_with_runtime((), inbox, |(), runtime, inbox, observer| {
            runtime.block_on(async move {
                let host = BootstrappedHost::open_for_core(
                    options,
                    &evorch_core::recovery_service::CoreGraphRecovery,
                )
                .await?;
                let listener = instance.bind()?;
                let expires = !case.starts_with("planned_")
                    && !matches!(case.as_str(), "product_exec" | "product_planned_idle");
                operate(
                    host, instance, listener, inbox, observer, mask, expires, block, seen, release,
                    case,
                )
                .await
            })
        })
        .map_err(|failure| failure.error)?
    };
    if cleanup_returns {
        // Even an existing cleanup error must leave through Runtime Drop and
        // watchdog completion/join, without replacing it with status 74.
        say("OUTER_COMPLETED");
    }
    outcome?;
    Err("composition returned instead of expiry/exec".into())
}
#[allow(clippy::too_many_arguments)]
async fn operate(
    mut host: BootstrappedHost,
    instance: Arc<Instance>,
    listener: std::os::unix::net::UnixListener,
    inbox: core_instance::handoff::ShutdownInbox,
    observer: evorch_core::lifecycle::shutdown_watchdog::Observer,
    mask: Arc<BlockedShutdownSignals>,
    expires: bool,
    block: Arc<Block>,
    seen: tokio::sync::oneshot::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
    case: String,
) -> Result<(), workbench_host::bootstrap::BootstrapError> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let signals = OperatorSignals::from_inbox(inbox)?;
    let mut driver = LifecycleDriver::new(&host);
    driver.set_shutdown_watchdog(observer)?;
    let core = driver.restart.clone();
    let backend = Arc::new(HostBackend::new(
        host.host.clone(),
        instance.identity.clone(),
    ));
    if host.owner.attach("idle").is_err() {
        host.owner.start("idle")?;
    }
    let scratch = host.scratch.clone();
    let scratch_path = scratch.thread_dir("idle");
    // Saved scratch is fixture data in the real owner's documented public path.
    // OwnerHost::start has already recorded the real lease for this thread.
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&scratch_path)?;
    let marker = scratch_path.join("watchdog-retained");
    std::fs::write(&marker, b"watchdog-retained")?;
    say(&format!("SCRATCH {}", marker.display()));
    let expiry = std::env::var("EVORCH_WATCHDOG_CASE")
        .ok()
        .filter(|name| name.starts_with("expiry_"));
    if let Some(socket) = std::env::var_os("EVORCH_EXPIRY_PEER_SOCKET") {
        // Owned through the typed owner API (guard-supervised, same PGID root +
        // grandchild) but in no ledger the host cleanup drains: they model owned
        // children that a stalled cleanup never reached. The session is
        // forgotten, never dropped: Drop would kill the group itself, while only
        // the guard may reap it after this process exits.
        let session = tokio::task::spawn_blocking(move || {
            let mut command = sandbox::owned::OwnedCommand::new(std::env::current_exe()?);
            command
                .args(["--peer".into(), "root".into(), socket])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            sandbox::owned::OwnedSession::spawn_owned(
                None,
                sandbox::ChildKind::Other,
                Some("expiry-fixture"),
                &mut command,
            )
        })
        .await??;
        std::mem::forget(session);
    }
    if let Some(start) = OWNED_FIXTURE.get() {
        // Never dropped: Drop would reap the children itself, while only the
        // guard may do that after this process exits.
        std::mem::forget(start(say).await?);
    }
    let _archive_hold = if case == "cold_stopped_join" {
        let (entered, seen) = tokio::sync::oneshot::channel();
        let entered = Mutex::new(Some(entered));
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let blocked = Mutex::new(blocked);
        host.scratch_archive.set_checker(Arc::new(move |_| {
            entered.lock().unwrap().take().unwrap().send(()).unwrap();
            blocked.lock().unwrap().recv().unwrap();
            Ok(false)
        }));
        host.scratch_archive.hint("idle");
        seen.await?;
        Some(release)
    } else {
        None
    };
    let product_exec = matches!(case.as_str(), "product_exec" | "product_planned_idle");
    let target_path = if product_exec {
        test_support::product_artifact::require_from_env().exe
    } else {
        std::env::current_exe()?
    };
    let current = Arc::new(core_launcher::handoff::CurrentElf::record()?);
    let description = evorch_core::handoff_description::describe()?;
    let schema = *storage::supported_schema_versions()?.end();
    let checked = core_launcher::handoff::targets::RestartTarget::preflight(
        core_launcher::handoff::targets::TargetSelection {
            id: "watchdog",
            configured_path: Some(&target_path),
            timeout: std::time::Duration::from_secs(10),
        },
        &current,
        core_launcher::handoff::targets::TargetRequirements {
            handoff: 1,
            snapshot: 1,
            storage_schema: schema,
            protocol: description.protocol,
            platform: &description.platform,
        },
        core_launcher::handoff::ProbeLimits::default(),
        std::future::pending(),
    )
    .await?
    .before_capture(
        &current,
        core_launcher::handoff::ProbeLimits::default(),
        std::future::pending(),
    )
    .await?;
    let target =
        FinalizationTarget::new(current, checked).with_environment(FinalizationEnvironment::new(
            instance.clone(),
            mask,
            signals.clone(),
            driver.control_drain(),
            if product_exec {
                vec![
                    "--config-dir".into(),
                    PathBuf::from(std::env::var_os("EVORCH_WATCHDOG_ROOT").unwrap())
                        .join("config")
                        .into_os_string(),
                ]
            } else {
                vec!["--watchdog-child".into()]
            },
        ));
    let web = if case == "cold_control_web" {
        let config = workbench_web::WebServerConfig {
            enabled: true,
            bind: "127.0.0.1:0".parse()?,
            ..Default::default()
        };
        let web = workbench_web::WebServer::start(config, backend.clone())
            .await?
            .unwrap();
        let checkpoint = block.clone();
        web.set_restart_observer(Arc::new(move |phase| {
            if phase == "web_connections_begin" && checkpoint.active.swap(false, Ordering::AcqRel) {
                say("BLOCKED control_web");
                checkpoint.release.lock().unwrap().recv().unwrap();
            }
        }));
        Some(web)
    } else {
        None
    };
    let id = core.request_planned(BTreeMap::new())?;
    core.wait_sealed(id).await?;
    let build = core_instance::handoff::running_elf_identity()?;
    let epoch = backend.hello().instance_epoch.0;
    instance.publish(epoch)?;
    let metadata = RestartMetadata {
        restart_id: if expires { "expiry" } else { "predecessor" }.into(),
        capture_id: if expires {
            "expiry-capture"
        } else {
            "predecessor-capture"
        }
        .into(),
        config_identity: instance.identity.clone(),
        old_epoch: epoch,
        successor_epoch: epoch.checked_add(1).unwrap(),
        old_build: build.clone(),
        successor_build: core_instance::handoff::elf_identity(&target_path)?,
        target_identity: "watchdog".into(),
        handoff_version: 1,
        snapshot_version: 1,
        storage_version: schema,
        old_builtin_tool_names: host.runtime.restart_builtin_tool_names(),
        old_compiled_catalog_fingerprint: None,
        new_compiled_catalog_fingerprint: None,
        old_effective_tools_fingerprint: None,
        new_effective_tools_fingerprint: None,
    };
    let tx = host.composition.storage_writer.mutate_restart(
        metadata.restart_id.clone(),
        None,
        RestartMutation::Create(Box::new(metadata)),
    )?;
    block.active.store(expires, Ordering::Release);
    if product_exec || expiry.as_deref() == Some("expiry_signal_first") {
        say("PREEXEC");
        tokio::task::spawn_blocking(|| {
            use std::io::BufRead;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).unwrap();
            assert_eq!(line, "exec\n");
        })
        .await?;
    }
    core.start_finalization(id, &mut host, tx, target)?;
    if expires && matches!(case.as_str(), "cold_stopped_join" | "cold_control_web") {
        // The archive worker is known to be held. The real stage-11 call now
        // enters stop_worker_for_exec's archive.stop -> join, with its signal
        // reader transferred. Keep the actual lifecycle future alive as well.
        let running = driver.run_with_web(&host, backend, listener, web, signals);
        tokio::pin!(running);
        tokio::select! {
            event = seen => {
                event?;
                say("BLOCKED stopped_join");
                std::future::pending::<()>().await;
                unreachable!();
            }
            result = &mut running => return Err(format!("lifecycle returned before stopped join: {result:?}").into()),
            result = core.wait_finalization(id) => return Err(format!("finalization returned before stopped join: {result:?}").into()),
        }
    }
    if expires {
        seen.await?;
        if case == "cold_executor" {
            let start = Arc::new(std::sync::Barrier::new(3));
            let (keep, stop) = std::sync::mpsc::channel::<()>();
            let stop = Arc::new(Mutex::new(stop));
            for _ in 0..2 {
                let start = start.clone();
                let stop = stop.clone();
                tokio::spawn(async move {
                    start.wait();
                    stop.lock().unwrap().recv().unwrap();
                });
            }
            start.wait();
            say("BLOCKED executor");
            let (_main_keep, main_stop) = std::sync::mpsc::channel::<()>();
            let _keep = keep;
            main_stop.recv().unwrap();
            unreachable!("blocked executor resumed");
        }
        if case == "cold_host_lock" {
            return hold_host_lock(driver, &host, backend, listener, signals).await;
        }
        say(&format!("BLOCKED {}", block.phase));
    }
    let _keep_worker_blocked = release;
    tokio::select! {
        result = driver.run(&host, backend, listener, false, signals) => {
            result?;
            if case == "cold_cleanup_returns" {
                _keep_worker_blocked.send(())?;
                // This cell exercises shutdown with an existing finalizer error.
                // Publish that error before shutdown closes the runtime fence;
                // the opposite ordering may legitimately finish cleanup cleanly.
                assert!(core.wait_finalization(id).await.is_err());
            }
            host.shutdown().await?;
        }
        result = core.wait_finalization(id) => {
            return Err(format!("finalization returned before expiry/exec: {result:?}").into());
        }
    }
    Ok(())
}

// Intentional composition fault, isolated from the other async scenarios.
#[allow(
    clippy::await_holding_lock,
    reason = "prove the OS watchdog never needs the host mutex"
)]
async fn hold_host_lock(
    mut driver: LifecycleDriver,
    host: &BootstrappedHost,
    backend: Arc<HostBackend>,
    listener: tokio::net::UnixListener,
    signals: OperatorSignals,
) -> Result<(), workbench_host::bootstrap::BootstrapError> {
    let _held = host.host.lock().unwrap();
    say("BLOCKED host_lock");
    // The first host poll blocks before the signal future can consume TERM.
    driver.run(host, backend, listener, false, signals).await?;
    Ok(())
}
