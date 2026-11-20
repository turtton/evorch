//! A stand-in for the OLD core, so that a real `execve` into the product can be
//! timed before 5a exists: the test binary itself, entered through its
//! `--ignored` worker test, bootstraps the production composition (only the
//! model and sandbox factories are test seams), parks three runs, runs the
//! real preflight, capture and raw exec stages, and execs into the product
//! artifact through the unmodified `--handoff-*` argv the coordinator builds.
//!
//! EVIDENCE CLASS: harness -> product. This measures what the PRODUCT does
//! after an exec. It is not a product -> product restart and must not be
//! counted as one: the old side is not the product (different binary, no
//! public request, test seams in the composition).
//!
//! The exec instant is observed from outside without any hook in either
//! binary: the worker holds a pipe whose both ends are close-on-exec, and the
//! parent reads the other end of that same pipe through `/proc`. The kernel
//! closes the worker's ends during `execve`, which is the parent's EOF.
use super::{
    artifact::ProductArtifact,
    env::IsolatedEnv,
    launch::EXIT_WATCHDOG,
    observe,
    process::{LaunchSpec, ManagedProcess, Report, WaitError},
};
use core_instance::{ConfigIdentity, Instance, LaunchGuard};
use evorch_core::{
    backend::HostBackend,
    lifecycle::{LifecycleDriver, OperatorSignals},
    restart::{FinalizationEnvironment, FinalizationTarget},
};
use std::{
    collections::BTreeMap,
    io::{BufRead, Read, Write},
    path::PathBuf,
    process::Command,
    sync::{Arc, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use storage::recovery::restart::{RestartMetadata, RestartMutation};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};

#[path = "../restart_model.rs"]
pub mod restart_model;
#[path = "../signal_entry.rs"]
pub mod signal_entry;

pub const WORKER_TEST: &str = "f4p_old_core_worker";
pub const ENV_PRODUCT: &str = "EVORCH_F4P_WORKER_PRODUCT";
const BEFORE: &str = "F4P-BEFORE ";
const READY_WATCHDOG: Duration = Duration::from_secs(120);

/// Entered through the ignored worker test of the binary that includes this
/// module. `EVORCH_MASKED_TEST_CHILD` must be set so that the signal mask of
/// production's entry is installed before libtest starts threads.
pub fn run_worker() {
    let product = PathBuf::from(std::env::var_os(ENV_PRODUCT).expect("worker product"));
    let state = PathBuf::from(std::env::var_os("XDG_STATE_HOME").expect("isolated state"));
    let config_dir = PathBuf::from(std::env::var_os("F4P_CONFIG_DIR").expect("config dir"));
    let root = std::env::current_dir().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut sidebar = workspace_state::SidebarState::default();
        sidebar
            .add_project(workspace_state::ProjectId::new("p"), "repo", &root)
            .unwrap();
        std::fs::write(
            config_dir.join("sidebar.json"),
            serde_json::to_vec(&sidebar).unwrap(),
        )
        .unwrap();
        let model = Arc::new(restart_model::Model::default());
        let mut host = BootstrappedHost::open_for_core_with_test_seams(
            BootstrapOptions {
                config_dir: config_dir.clone(),
                project_root: root.clone(),
                ownership_root: state.join("evorch/threads"),
            },
            TestSeams {
                model: model.clone(),
                sandbox: Arc::new(restart_model::Direct),
            },
            &evorch_core::recovery_service::CoreGraphRecovery,
        )
        .await
        .unwrap();
        let config = ConfigIdentity::new(&config_dir).unwrap();
        let launch = LaunchGuard::acquire(&config).unwrap();
        let instance = Arc::new(Instance::acquire_default(&launch).unwrap());
        drop(launch);
        let mut driver = LifecycleDriver::new(&host);
        let core = driver.restart.clone();
        let backend = Arc::new(HostBackend::new(
            host.host.clone(),
            instance.identity.clone(),
        ));
        let listener = instance.bind().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        let signals = OperatorSignals::new(signal_entry::mask()).unwrap();
        let permit = host.owner.start("active").unwrap();
        host.owner.start("idle").unwrap();
        let mut runs = Vec::new();
        for index in 0..3 {
            let mut events = host.runtime.event_bus().subscribe();
            let config = runtime::RunConfig {
                ownership: Some(permit.clone()),
                interactive: true,
                keep_alive: true,
                ..Default::default()
            };
            let run = if index == 0 {
                host.runtime
                    .delegate_chat("active", runtime::Role::Orchestrator, "park".into(), config)
                    .unwrap()
            } else {
                host.runtime
                    .delegate_background_as_child(runs[0], runtime::Role::Worker, "park", config)
                    .unwrap()
            };
            loop {
                let event = events.recv().await.unwrap();
                if matches!(event.kind, event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged { run_id, to: event_bus::AgentRunPhase::Waiting, .. }) if run_id == run.to_string())
                {
                    break;
                }
            }
            runs.push(run);
        }
        let id = core
            .request_planned(BTreeMap::from([(
                runs[0],
                runtime::quiesce::RecoveryRecipe {
                    settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                    policy: runtime::quiesce::ResumePolicy::CoreOwned,
                    goal_id: None,
                    team: None,
                    lane: None,
                    descriptor: None,
                },
            )]))
            .unwrap();
        core.wait_sealed(id).await.unwrap();
        let build = core_instance::handoff::elf_identity(&product).unwrap();
        let described = Command::new(&product)
            .arg("--describe-handoff")
            .output()
            .unwrap();
        assert!(described.status.success());
        let description: core_launcher::handoff::HandoffDescription =
            serde_json::from_slice(&described.stdout).unwrap();
        let schema = *storage::supported_schema_versions().unwrap().end();
        let current = Arc::new(core_launcher::handoff::CurrentElf::record().unwrap());
        let checked = core_launcher::handoff::targets::RestartTarget::preflight(
            core_launcher::handoff::targets::TargetSelection {
                id: "target",
                configured_path: Some(&product),
                timeout: Duration::from_secs(300),
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
        .await
        .unwrap()
        .before_capture(
            &current,
            core_launcher::handoff::ProbeLimits::default(),
            std::future::pending(),
        )
        .await
        .unwrap();
        let target = FinalizationTarget::new(current, checked).with_environment(
            FinalizationEnvironment::new(
                instance.clone(),
                signal_entry::mask().clone(),
                signals.clone(),
                driver.control_drain(),
                vec!["--config-dir".into(), config_dir.clone().into_os_string()],
            ),
        );
        let old_build = core_instance::handoff::running_elf_identity().unwrap();
        let metadata = RestartMetadata {
            restart_id: "f4p".into(),
            capture_id: "f4p-capture".into(),
            config_identity: instance.identity.clone(),
            old_epoch: backend.hello().instance_epoch.0,
            successor_epoch: backend.hello().instance_epoch.0.checked_add(1).unwrap(),
            old_build,
            successor_build: build,
            target_identity: "target".into(),
            handoff_version: 1,
            snapshot_version: 1,
            storage_version: schema,
            old_builtin_tool_names: host.runtime.restart_builtin_tool_names(),
            old_compiled_catalog_fingerprint: None,
            new_compiled_catalog_fingerprint: None,
            old_effective_tools_fingerprint: None,
            new_effective_tools_fingerprint: None,
        };
        let tx = host
            .composition
            .storage_writer
            .mutate_restart("f4p".into(), None, RestartMutation::Create(Box::new(metadata)))
            .unwrap();
        // Both ends close-on-exec: the kernel closes them during execve.
        let (reader, writer) = std::io::pipe().unwrap();
        {
            use std::os::fd::AsRawFd;
            println!(
                "{BEFORE}{}",
                serde_json::json!({
                    "pid": std::process::id(),
                    "exec_pipe_fd": reader.as_raw_fd(),
                    "old_epoch": backend.hello().instance_epoch.0,
                })
            );
        }
        std::io::stdout().flush().unwrap();
        // Held until exec replaces the process; never closed by us.
        std::mem::forget(reader);
        std::mem::forget(writer);
        tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line).unwrap();
            assert_eq!(line, "exec\n");
        })
        .await
        .unwrap();
        core.start_finalization(id, &mut host, tx, target).unwrap();
        tokio::select! {
            result = driver.run(&host, backend, listener, false, signals) => panic!("lifecycle returned before exec: {result:?}"),
            result = core.wait_finalization(id) => panic!("M2 returned instead of exec: {result:?}"),
        }
    });
}

#[derive(Debug, Clone)]
pub struct Cycle {
    /// From the observed exec (pipe EOF read by the watcher thread, not the
    /// kernel's instant) to the readiness line received by the reader thread.
    /// Both ends are observation instants, each later than the event by a
    /// wake-up latency that is not measured here.
    pub exec_to_ready: Duration,
    /// Wall-clock milliseconds from the OBSERVED exec (the instant the watcher
    /// thread had read the pipe EOF, which is later than the kernel's close by
    /// the watcher's wake-up latency) to the claim time derived from the
    /// successor's lease. The first makes the value an under-measure by that
    /// latency; the second is an upper bound of the claim instant only while
    /// `claim_stable` holds. No bound of the true interval is claimed.
    pub exec_to_claim_ms: i64,
    /// The lease expiry did not move between the first readback and the last:
    /// no heartbeat has shifted the claim time derived from it.
    pub claim_stable: bool,
    pub lease_ms: u64,
    pub grace_ms: u64,
    pub pid_before: i32,
    pub old_epoch: u64,
    pub exe_replaced: bool,
}

#[derive(Debug)]
pub enum CycleError {
    Worker(String),
    Exec(String),
    Ready(String),
    Cleanup(String),
    /// Two observations disagree about their order.
    Invalid(String),
}
impl std::fmt::Display for CycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

fn wall_ms(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// One full old-core -> exec -> product Ready cycle in a fresh isolated
/// environment. Every wait is an event: the worker's line, the pipe EOF, the
/// product's readiness line, the exit notification.
/// A failed cycle carries the time from its start to the failure, so that a
/// failure is recorded as a failure with its own duration, never as a zero.
pub fn exec_cycle(
    artifact: &ProductArtifact,
    harness: &std::path::Path,
) -> Result<Cycle, (CycleError, Duration)> {
    let started = Instant::now();
    exec_cycle_inner(artifact, harness).map_err(|e| (e, started.elapsed()))
}

fn exec_cycle_inner(
    artifact: &ProductArtifact,
    harness: &std::path::Path,
) -> Result<Cycle, CycleError> {
    artifact.assert_unchanged();
    let env = IsolatedEnv::new();
    let mut command = LaunchSpec::new(harness);
    command.args(["--ignored", "--exact", WORKER_TEST, "--nocapture"]);
    env.apply(
        &mut command,
        &[
            ("EVORCH_MASKED_TEST_CHILD", "1".into()),
            (ENV_PRODUCT, artifact.path.clone().into_os_string()),
            ("F4P_CONFIG_DIR", env.config_dir().into_os_string()),
        ],
    );
    let mut worker =
        ManagedProcess::spawn(command).map_err(|e| CycleError::Worker(e.to_string()))?;
    let (before, _) = worker
        .wait_for(READY_WATCHDOG, |line| {
            serde_json::from_str::<serde_json::Value>(line.strip_prefix(BEFORE)?).ok()
        })
        .map_err(|e| CycleError::Worker(describe(e)))?;
    let pid = before["pid"].as_i64().unwrap() as i32;
    assert_eq!(pid, worker.pid(), "the worker reports its own pid");
    let fd = before["exec_pipe_fd"].as_i64().unwrap();
    let mut pipe = std::fs::File::open(format!("/proc/{pid}/fd/{fd}"))
        .map_err(|e| CycleError::Exec(format!("open exec pipe: {e}")))?;
    let (exec_tx, exec_rx) = mpsc::channel();
    let watcher = std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        // Blocks until every write end is closed: the exec (or the worker's death).
        let result = pipe.read(&mut byte);
        let _ = exec_tx.send((
            Instant::now(),
            SystemTime::now(),
            result.map_err(|e| e.to_string()),
        ));
    });
    worker
        .send_line("exec")
        .map_err(|e| CycleError::Exec(e.to_string()))?;
    let (exec_at, exec_wall, eof) = exec_rx
        .recv_timeout(READY_WATCHDOG)
        .map_err(|_| CycleError::Exec("no exec within the watchdog".into()))?;
    match eof {
        Ok(0) => (),
        other => return Err(CycleError::Exec(format!("unexpected pipe event {other:?}"))),
    }
    let (_, ready_at) = worker
        .wait_for(READY_WATCHDOG, |line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .filter(|v| v["status"] == "ready")
        })
        .map_err(|e| CycleError::Ready(describe(e)))?;
    let exe_replaced = observe::same_file(
        std::path::Path::new(&format!("/proc/{pid}/exe")),
        &artifact.path,
    );
    let (claim_a, lease_ms, grace_ms) = read_claim(&env);
    let (claim_b, _, _) = read_claim(&env);
    worker
        .terminate(EXIT_WATCHDOG)
        .map_err(|e| CycleError::Cleanup(describe(e)))?;
    let Report {
        status,
        stderr,
        orphans,
        unjoined_readers,
        reaper_lost,
        reaper_stderr,
    } = worker.join();
    watcher.join().unwrap();
    if !status.is_some_and(|s| s.success())
        || !orphans.is_empty()
        || unjoined_readers != 0
        || reaper_lost
    {
        return Err(CycleError::Cleanup(format!(
            "{status:?} {orphans:?} unjoined_readers={unjoined_readers} reaper_lost={reaper_lost} {stderr} {reaper_stderr}"
        )));
    }
    // An interval that comes out negative is not a sample of zero: the two
    // observations disagree about the order, so the sample is invalid.
    let exec_to_ready = ready_at.checked_duration_since(exec_at).ok_or_else(|| {
        CycleError::Invalid("the readiness line was received before the exec EOF".into())
    })?;
    let exec_to_claim_ms = (claim_a - lease_ms as i64) - wall_ms(exec_wall);
    if exec_to_claim_ms < 0 {
        return Err(CycleError::Invalid(format!(
            "the claim time is before the observed exec ({exec_to_claim_ms} ms)"
        )));
    }
    Ok(Cycle {
        exec_to_ready,
        exec_to_claim_ms,
        claim_stable: claim_a == claim_b,
        lease_ms,
        grace_ms,
        pid_before: pid,
        old_epoch: before["old_epoch"].as_u64().unwrap(),
        exe_replaced,
    })
}

/// (lease expiry in wall-clock ms, lease_ms, grace_ms) of the claimed owner,
/// read through the registry's own read-only open.
fn read_claim(env: &IsolatedEnv) -> (i64, u64, u64) {
    let registry = runtime::ownership::Registry::open_readonly(&env.ownership_db()).unwrap();
    let owner = registry.attach("active").unwrap();
    assert!(
        owner.planned_claim.is_some(),
        "the successor claimed the owner"
    );
    (
        owner.lease.expires_at as i64,
        owner.settings.lease_ms.get(),
        owner.settings.grace_ms.get(),
    )
}

fn describe(e: WaitError) -> String {
    format!("{e:?}")
}
