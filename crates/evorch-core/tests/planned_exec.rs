#![cfg(target_os = "linux")]
//! Full M2 coordinator -> execve -> unmodified product argv/bootstrap. No public
//! restart operation or reconnect client is installed. Runner owns the 60s watchdog.
#[path = "../shared-test-support/restart_model.rs"]
mod restart_model;
#[path = "../shared-test-support/signal_entry.rs"]
mod signal_entry;
use core_instance::{ConfigIdentity, Instance, LaunchGuard};
use evorch_core::{
    backend::HostBackend,
    lifecycle::{LifecycleDriver, OperatorSignals},
    restart::{FinalizationEnvironment, FinalizationTarget},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    sync::Arc,
};
use storage::recovery::restart::*;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};

const WORKER: &str = "m2_product_worker";
struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn prepared_raw_exec_reaches_product_bootstrap_with_multiple_checkpoints() {
    product_handoff(false);
}
#[test]
fn product_rejects_unexpected_inherited_fd_without_cold_fallback() {
    product_handoff(true);
}
fn product_handoff(bad_fd: bool) {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let config_dir = root.path().join("config");
    let state = root.path().join("state");
    let sockets = test_socket_dir::SocketDir::new();
    let product = std::env::var("CARGO_BIN_EXE_evorch-core").unwrap();
    let mut child = Child(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", WORKER, "--nocapture"])
            .env("EVORCH_MASKED_TEST_CHILD", "1")
            .env("EVORCH_D3B_ROOT", root.path())
            .env("EVORCH_D3B_BAD_FD", if bad_fd { "1" } else { "0" })
            .env("EVORCH_D3B_PRODUCT", &product)
            .env("XDG_RUNTIME_DIR", sockets.path())
            .env("XDG_STATE_HOME", &state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let mut line = String::new();
    let before = loop {
        line.clear();
        assert_ne!(output.read_line(&mut line).unwrap(), 0);
        if let Some(json) = line.strip_prefix("D3B-BEFORE ") {
            break serde_json::from_str::<serde_json::Value>(json).unwrap();
        }
    };
    let identity = ConfigIdentity::new(&config_dir).unwrap();
    let excludes = || {
        let launch = LaunchGuard::acquire(&identity).unwrap();
        assert!(!launch.lifetime_available().unwrap());
        assert!(matches!(
            Instance::acquire(&launch, sockets.path()),
            Err(core_instance::InstanceError::AlreadyRunning)
        ));
    };
    excludes();
    input.write_all(b"exec\n").unwrap();
    input.flush().unwrap();
    loop {
        line.clear();
        let count = output.read_line(&mut line).unwrap();
        if count == 0 {
            assert!(bad_fd, "product failed before Ready");
            assert!(!child.0.wait().unwrap().success());
            let db = storage::Database::open(&storage::StorageConfig {
                db_path: config_dir.join("evorch-events.db"),
                ..Default::default()
            })
            .unwrap();
            let tx = db.restart_transaction("d3b").unwrap().unwrap();
            assert_eq!(tx.phase, RestartPhase::Armed);
            assert_eq!(tx.adopted_epoch, None);
            assert_eq!(tx.ready_at_ms, None);
            assert_eq!(tx.expected.len(), 3);
            db.validate_restart_capture_digest("d3b").unwrap();
            assert!(
                LaunchGuard::acquire(&identity)
                    .unwrap()
                    .lifetime_available()
                    .unwrap()
            );
            return;
        }
        if serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .is_some_and(|v| v["status"] == "ready")
        {
            assert!(!bad_fd, "unexpected inherited FD must fail before Adopt");
            break;
        }
    }
    excludes();
    assert_eq!(
        std::fs::canonicalize(format!("/proc/{}/exe", child.0.id())).unwrap(),
        std::fs::canonicalize(&product).unwrap(),
        "exec replaced the image, not a spawned successor"
    );
    assert_eq!(before["pid"].as_u64().unwrap(), u64::from(child.0.id()));
    let db = storage::Database::open(&storage::StorageConfig {
        db_path: config_dir.join("evorch-events.db"),
        ..Default::default()
    })
    .unwrap();
    let tx = db.restart_transaction("d3b").unwrap().unwrap();
    assert!(matches!(
        tx.phase,
        RestartPhase::Adopted | RestartPhase::Completed
    ));
    assert_eq!(tx.adopted_epoch, Some(tx.metadata.successor_epoch));
    assert!(tx.ready_at_ms.is_some());
    assert_eq!(tx.expected.len(), 3);
    let runs: BTreeSet<_> = before["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        tx.expected
            .iter()
            .map(|m| m.run_id.clone())
            .collect::<BTreeSet<_>>(),
        runs
    );
    assert!(tx.expected.iter().all(|m| m.snapshot.is_some()));
    assert_eq!(
        tx.owner_progress
            .iter()
            .find(|p| p.thread_id == "active")
            .unwrap()
            .parked_runs,
        runs
    );
    db.validate_restart_capture_digest("d3b").unwrap();
    let revision = tx.revision;
    // A second physical entry cannot be minted from this adopted row. Re-running
    // the product with the consumed/non-inherited FD numbers is fatal, not Cold.
    let status = Command::new(&product)
        .args([
            "--config-dir",
            config_dir.to_str().unwrap(),
            "--handoff-lifetime-fd",
            "999",
            "--handoff-ticket-fd",
            "1000",
        ])
        .env("XDG_RUNTIME_DIR", sockets.path())
        .status()
        .unwrap();
    assert!(!status.success());
    assert_eq!(
        db.restart_transaction("d3b").unwrap().unwrap().revision,
        revision
    );
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.0.id() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    assert!(child.0.wait().unwrap().success());
    assert!(
        !Path::new(&format!("/proc/{}", child.0.id())).exists(),
        "exec child was reaped"
    );
    assert!(
        LaunchGuard::acquire(&identity)
            .unwrap()
            .lifetime_available()
            .unwrap()
    );
}

#[test]
#[ignore = "single-threaded mask installed by the opt-in entry constructor"]
fn m2_product_worker() {
    let root = std::path::PathBuf::from(std::env::var_os("EVORCH_D3B_ROOT").unwrap());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let config_dir = root.join("config"); std::fs::create_dir(&config_dir).unwrap();
        let mut sidebar = workspace_state::SidebarState::default();
        sidebar.add_project(workspace_state::ProjectId::new("p"), "repo", &root).unwrap();
        std::fs::write(config_dir.join("sidebar.json"), serde_json::to_vec(&sidebar).unwrap()).unwrap();
        let model = Arc::new(restart_model::Model::default());
        let mut host = BootstrappedHost::open_for_core_with_test_seams(BootstrapOptions {
            config_dir: config_dir.clone(), project_root: root.clone(), ownership_root: root.join("state/evorch/threads"),
        }, TestSeams { model: model.clone(), sandbox: Arc::new(restart_model::Direct) }, &evorch_core::recovery_service::CoreGraphRecovery).await.unwrap();
        let config = ConfigIdentity::new(&config_dir).unwrap();
        let launch = LaunchGuard::acquire(&config).unwrap();
        let instance = Arc::new(Instance::acquire_default(&launch).unwrap()); drop(launch);
        let mut driver = LifecycleDriver::new(&host);
        let core = driver.restart.clone();
        let backend = Arc::new(HostBackend::new(host.host.clone(), instance.identity.clone()));
        let listener = instance.bind().unwrap(); listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        let signals = OperatorSignals::new(signal_entry::mask()).unwrap();
        let permit = host.owner.start("active").unwrap(); host.owner.start("idle").unwrap();
        let mut runs = Vec::new();
        for index in 0..3 {
            let mut events = host.runtime.event_bus().subscribe();
            let config = runtime::RunConfig { ownership: Some(permit.clone()), interactive: true, keep_alive: true, ..Default::default() };
            let run = if index == 0 { host.runtime.delegate_chat("active", runtime::Role::Orchestrator, "park".into(), config).unwrap() }
                else { host.runtime.delegate_background_as_child(runs[0], runtime::Role::Worker, "park", config).unwrap() };
            loop {
                if matches!(events.recv().await.unwrap().kind, event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged { run_id, to: event_bus::AgentRunPhase::Waiting, .. }) if run_id == run.to_string()) { break; }
            }
            runs.push(run);
        }
        let id = core.request_planned(BTreeMap::from([(runs[0], runtime::quiesce::RecoveryRecipe {
            settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(), policy: runtime::quiesce::ResumePolicy::CoreOwned,
            goal_id: None, team: None, lane: None, descriptor: None,
        })])).unwrap();
        core.wait_sealed(id).await.unwrap();
        let product = std::path::PathBuf::from(std::env::var_os("EVORCH_D3B_PRODUCT").unwrap());
        let build = core_instance::handoff::elf_identity(&product).unwrap();
        let description = core_launcher::handoff::HandoffDescription { version: 1, elf_path: product.clone(), build_identity: build.clone(), handoff_format: 1, snapshot_format: 1,
            schema: core_launcher::handoff::SchemaRange { minimum: 1, maximum: 1000 }, protocol: 1, platform: format!("linux-{}", std::env::consts::ARCH), tool_catalog: "process-fixture".into() };
        let target_path = root.join("entry");
        let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        std::fs::write(&target_path, format!(
            "#!/bin/sh\nif [ \"$1\" = --describe-handoff ]; then printf '%s\\n' {}; else {} exec {} \"$@\"; fi\n",
            quote(&serde_json::to_string(&description).unwrap()),
            if std::env::var("EVORCH_D3B_BAD_FD").unwrap() == "1" { "exec 9<>/dev/null;" } else { "" },
            quote(product.to_str().unwrap())
        )).unwrap();
        std::fs::set_permissions(&target_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let current = Arc::new(core_launcher::handoff::CurrentElf::record().unwrap());
        let checked = core_launcher::handoff::targets::RestartTarget::preflight(core_launcher::handoff::targets::TargetSelection { id: "target", configured_path: Some(&target_path), timeout: std::time::Duration::from_secs(300) }, &current,
            core_launcher::handoff::targets::TargetRequirements { handoff: 1, snapshot: 1, storage_schema: 65, protocol: 1, platform: &description.platform }, core_launcher::handoff::ProbeLimits::default(), std::future::pending()).await.unwrap()
            .before_capture(&current, core_launcher::handoff::ProbeLimits::default(), std::future::pending()).await.unwrap();
        let target = FinalizationTarget::new(current, checked).with_environment(FinalizationEnvironment::new(instance.clone(), signal_entry::mask().clone(), signals.clone(), driver.control_drain(), vec!["--config-dir".into(), config_dir.into_os_string()]));
        let metadata = RestartMetadata { restart_id: "d3b".into(), capture_id: "d3b-capture".into(), config_identity: instance.identity.clone(), old_epoch: backend.hello().instance_epoch.0,
            successor_epoch: backend.hello().instance_epoch.0.checked_add(1).unwrap(), old_build: core_instance::handoff::running_elf_identity().unwrap(), successor_build: build, target_identity: "target".into(), handoff_version: 1, snapshot_version: 1, storage_version: 65,
            old_builtin_tool_names: BTreeSet::new(), old_compiled_catalog_fingerprint: None, new_compiled_catalog_fingerprint: None, old_effective_tools_fingerprint: None, new_effective_tools_fingerprint: None };
        let tx = host.composition.storage_writer.mutate_restart("d3b".into(), None, RestartMutation::Create(Box::new(metadata))).unwrap();
        println!("D3B-BEFORE {}", serde_json::json!({"pid":std::process::id(), "runs":runs.iter().map(ToString::to_string).collect::<Vec<_>>()})); std::io::stdout().flush().unwrap();
        tokio::task::spawn_blocking(|| { let mut line = String::new(); std::io::stdin().lock().read_line(&mut line).unwrap(); assert_eq!(line, "exec\n"); }).await.unwrap();
        core.start_finalization(id, &mut host, tx, target).unwrap();
        // The lifecycle services the real listener/control drain until exec replaces
        // all threads. A returned finalization is a failure, never resumed business.
        tokio::select! {
            result = driver.run(&host, backend, listener, false, signals) => panic!("lifecycle returned before exec: {result:?}"),
            result = core.wait_finalization(id) => panic!("M2 returned instead of exec: {result:?}"),
        }
    });
}
