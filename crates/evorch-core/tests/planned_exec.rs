#![cfg(target_os = "linux")]
//! Full M2 coordinator -> execve -> unmodified product argv/bootstrap. No public
//! restart operation or reconnect client is installed. Runner owns the 60s watchdog.
#[path = "../shared-test-support/exec_fixture.rs"]
mod exec_fixture;
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
    product_handoff(false, false, false);
}
#[test]
fn product_rejects_unexpected_inherited_fd_without_cold_fallback() {
    product_handoff(true, false, false);
}
#[test]
fn product_preflight_metadata_mismatch_invalidates_the_same_transaction() {
    product_handoff(false, true, false);
}
#[test]
fn product_owner_proof_mismatch_leaves_the_hold_and_checkpoint_intact() {
    product_handoff(false, false, true);
}
fn product_handoff(bad_fd: bool, metadata_mismatch: bool, owner_mismatch: bool) {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let config_dir = root.path().join("config");
    let state = root.path().join("state");
    let sockets = test_socket_dir::SocketDir::new();
    let product = exec_fixture::product_copy(root.path());
    let worker = exec_fixture::stripped_copy(
        &std::env::current_exe().unwrap(),
        &root.path().join("old-core-fixture"),
    );
    let mut child = Child(
        Command::new(&worker)
            .args(["--ignored", "--exact", WORKER, "--nocapture"])
            .env("EVORCH_MASKED_TEST_CHILD", "1")
            .env("EVORCH_D3B_ROOT", root.path())
            .env("EVORCH_D3B_BAD_FD", if bad_fd { "1" } else { "0" })
            .env(
                "EVORCH_D3B_METADATA_MISMATCH",
                if metadata_mismatch { "1" } else { "0" },
            )
            .env("EVORCH_D3B_PRODUCT", &product)
            .env("EVORCH_D3B_WORKER", &worker)
            .env(
                "EVORCH_D3B_OWNER_MISMATCH",
                if owner_mismatch { "1" } else { "0" },
            )
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
            assert!(
                bad_fd || metadata_mismatch || owner_mismatch,
                "product failed before Ready"
            );
            assert_eq!(child.0.wait().unwrap().success(), metadata_mismatch);
            assert!(!Path::new(&format!("/proc/{}", child.0.id())).exists());
            let db = storage::Database::open(&storage::StorageConfig {
                db_path: config_dir.join("evorch-events.db"),
                ..Default::default()
            })
            .unwrap();
            let tx = db.restart_transaction("d3b").unwrap().unwrap();
            assert_eq!(
                tx.phase,
                if metadata_mismatch {
                    RestartPhase::Invalidated
                } else if owner_mismatch {
                    RestartPhase::Adopted
                } else {
                    RestartPhase::Armed
                }
            );
            assert_eq!(tx.metadata.restart_id, "d3b");
            assert_eq!(tx.metadata.capture_id, "d3b-capture");
            assert_eq!(
                tx.adopted_epoch,
                owner_mismatch.then_some(tx.metadata.successor_epoch)
            );
            if owner_mismatch {
                let registry = runtime::ownership::Registry::open_readonly(
                    &state.join("evorch/threads/owners.db"),
                )
                .unwrap();
                let held = registry.attach("active").unwrap();
                let hold = held
                    .handoff_hold
                    .as_deref()
                    .expect("mismatched hold must not be cleared");
                assert_eq!(hold.capture_id, "mismatched-capture");
                assert_eq!(held.lease.owner_id, hold.owner_id);
                assert_eq!(held.lease.generation, hold.generation);
                assert!(held.planned_claim.is_none());
                let receipt = identity.read_restart_receipt().unwrap().unwrap();
                assert!(matches!(
                    receipt.status,
                    core_instance::handoff::receipt::BootStatus::Failed { .. }
                ));
            }
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
            assert!(
                !bad_fd && !metadata_mismatch && !owner_mismatch,
                "invalid entry or metadata must fail before Adopt"
            );
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
    let registry =
        runtime::ownership::Registry::open_readonly(&state.join("evorch/threads/owners.db"))
            .unwrap();
    for progress in &tx.owner_progress {
        let claimed = registry.attach(&progress.thread_id).unwrap();
        assert!(claimed.handoff_hold.is_none());
        assert_ne!(claimed.lease.owner_id, progress.old_owner_id);
        assert_eq!(claimed.lease.generation, progress.old_generation + 1);
        assert!(claimed.planned_claim.is_some());
        assert!(
            claimed.active_runs.is_empty(),
            "ownership claim grants no run activation"
        );
    }
    assert!(
        db.recovery_claims()
            .unwrap()
            .iter()
            .all(|claim| claim.state != storage::recovery::RecoveryClaimState::Activated),
        "the missing current thread directory must block graph execution despite owner adoption"
    );
    assert_unpublished_root(
        &identity.socket(sockets.path()).unwrap(),
        before["runs"][0].as_str().unwrap(),
    );
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
    for progress in &tx.owner_progress {
        let released = registry.attach(&progress.thread_id).unwrap();
        assert_eq!(
            released.state,
            runtime::ownership::OwnerState::Released,
            "parked-only and non-resumed graph owners participate in normal shutdown"
        );
        assert!(released.handoff_hold.is_none());
    }
    db.validate_restart_capture_digest("d3b").unwrap();
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
        eprintln!("D3B worker cold bootstrap complete");
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
                let event = events.recv().await.unwrap();
                if matches!(event.kind, event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged { run_id, to: event_bus::AgentRunPhase::Waiting, .. }) if run_id == run.to_string()) { break; }
            }
            eprintln!("D3B worker parked {index}: {run}");
            runs.push(run);
        }
        let id = core.request_planned(BTreeMap::from([(runs[0], runtime::quiesce::RecoveryRecipe {
            settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(), policy: runtime::quiesce::ResumePolicy::CoreOwned,
            goal_id: None, team: None, lane: None, descriptor: None,
        })])).unwrap();
        core.wait_sealed(id).await.unwrap();
        eprintln!("D3B worker sealed");
        let product = std::path::PathBuf::from(std::env::var_os("EVORCH_D3B_PRODUCT").unwrap());
        let build = core_instance::handoff::elf_identity(&product).unwrap();
        let described = Command::new(&product).arg("--describe-handoff").output().unwrap();
        assert!(described.status.success(), "{}", String::from_utf8_lossy(&described.stderr));
        let description: core_launcher::handoff::HandoffDescription = serde_json::from_slice(&described.stdout).unwrap();
        let schema = *storage::supported_schema_versions().unwrap().end();
        assert_eq!(description.protocol, workbench_protocol::ProtocolVersion::CURRENT.0);
        assert_eq!(description.build_identity, build);
        // Primary success probes and execs the unmodified product. Only this
        // negative fixture wrapper deliberately introduces a forbidden descriptor.
        let target_path = if std::env::var("EVORCH_D3B_BAD_FD").unwrap() == "1" {
            let path = root.join("bad-fd-entry");
            let quote = format!("'{}'", product.to_str().unwrap().replace('\'', "'\\''"));
            std::fs::write(&path, format!("#!/bin/sh\nif [ \"$1\" != --describe-handoff ]; then exec 9<>/dev/null; fi\nexec {quote} \"$@\"\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        } else if std::env::var("EVORCH_D3B_OWNER_MISMATCH").unwrap() == "1" {
            let path = root.join("mismatched-owner-entry");
            let quote = |path: &Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
            let worker = std::path::PathBuf::from(std::env::var_os("EVORCH_D3B_WORKER").unwrap());
            std::fs::write(&path, format!("#!/bin/sh\nif [ \"$1\" != --describe-handoff ]; then {} --ignored --exact mismatch_owner_worker --nocapture >&2 || exit 1; fi\nexec {} \"$@\"\n", quote(&worker), quote(&product))).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        } else { product.clone() };
        let current = Arc::new(core_launcher::handoff::CurrentElf::record().unwrap());
        let checked = core_launcher::handoff::targets::RestartTarget::preflight(core_launcher::handoff::targets::TargetSelection { id: "target", configured_path: Some(&target_path), timeout: std::time::Duration::from_secs(300) }, &current,
            core_launcher::handoff::targets::TargetRequirements { handoff: 1, snapshot: 1, storage_schema: schema, protocol: description.protocol, platform: &description.platform }, core_launcher::handoff::ProbeLimits::default(), std::future::pending()).await.unwrap()
            .before_capture(&current, core_launcher::handoff::ProbeLimits::default(), std::future::pending()).await.unwrap();
        let target = FinalizationTarget::new(current, checked).with_environment(FinalizationEnvironment::new(instance.clone(), signal_entry::mask().clone(), signals.clone(), driver.control_drain(), vec!["--config-dir".into(), config_dir.into_os_string()]));
        let metadata_mismatch = std::env::var("EVORCH_D3B_METADATA_MISMATCH").unwrap() == "1";
        let old_build = core_instance::handoff::running_elf_identity().unwrap();
        let successor_build = if metadata_mismatch { assert_ne!(old_build, build); old_build.clone() } else { build };
        let metadata = RestartMetadata { restart_id: "d3b".into(), capture_id: "d3b-capture".into(), config_identity: instance.identity.clone(), old_epoch: backend.hello().instance_epoch.0,
            successor_epoch: backend.hello().instance_epoch.0.checked_add(1).unwrap(), old_build, successor_build, target_identity: "target".into(), handoff_version: 1, snapshot_version: 1, storage_version: schema,
            old_builtin_tool_names: host.runtime.restart_builtin_tool_names(), old_compiled_catalog_fingerprint: None, new_compiled_catalog_fingerprint: None, old_effective_tools_fingerprint: None, new_effective_tools_fingerprint: None };
        let tx = host.composition.storage_writer.mutate_restart("d3b".into(), None, RestartMutation::Create(Box::new(metadata))).unwrap();
        println!("D3B-BEFORE {}", serde_json::json!({"pid":std::process::id(), "runs":runs.iter().map(ToString::to_string).collect::<Vec<_>>()})); std::io::stdout().flush().unwrap();
        tokio::task::spawn_blocking(|| { let mut line = String::new(); std::io::stdin().lock().read_line(&mut line).unwrap(); assert_eq!(line, "exec\n"); }).await.unwrap();
        core.start_finalization(id, &mut host, tx, target).unwrap();
        if metadata_mismatch {
            // Actual product preflight succeeded; only the durable successor
            // identity disagrees. The same M2 transaction must fail closed.
            assert!(core.wait_finalization(id).await.is_err());
            assert!(host.runtime.producer_gate().try_enter().is_none());
            assert!(core.cancel(id).is_err());
            let _ = host.shutdown().await;
            return;
        }
        // The lifecycle services the real listener/control drain until exec replaces
        // all threads. A returned finalization is a failure, never resumed business.
        tokio::select! {
            result = driver.run(&host, backend, listener, false, signals) => panic!("lifecycle returned before exec: {result:?}"),
            result = core.wait_finalization(id) => panic!("M2 returned instead of exec: {result:?}"),
        }
    });
}

#[test]
#[ignore = "fixture mutation runs only between raw exec and product entry"]
fn mismatch_owner_worker() {
    let root = std::path::PathBuf::from(std::env::var_os("EVORCH_D3B_ROOT").unwrap());
    let mut registry =
        runtime::ownership::Registry::open_existing(&root.join("state/evorch/threads/owners.db"))
            .unwrap();
    registry
        .update("active", |owner| {
            let hold = owner
                .handoff_hold
                .as_mut()
                .expect("M2 must have installed the hold");
            hold.capture_id = "mismatched-capture".into();
            Ok(())
        })
        .unwrap();
}

// A read of history can still show a quarantined run. Probe command routing
// instead: a nonexistent withdrawal must fail at the missing-root fence, not
// reach the captured run's input queue. This sends no provider input.
fn assert_unpublished_root(socket: &Path, run: &str) {
    use workbench_protocol::{
        commands::{Command, WithdrawFollowUp},
        handshake::BuildInfo,
        ids::{ClientId, RunId, SubmissionId, ThreadId},
        namespaces::conversation::ConversationCommand,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let client = workbench_client::WorkbenchClient::connect(
                workbench_client::UnixTransport::connect(socket)
                    .await
                    .unwrap(),
                workbench_client::ClientConfig::new(
                    ClientId::new("d3b-no-binding").unwrap(),
                    BuildInfo {
                        name: "d3b-no-binding".into(),
                        version: "1".into(),
                        revision: None,
                    },
                ),
            )
            .await
            .unwrap();
            let error = client
                .command(Command::Conversation(
                    ConversationCommand::WithdrawFollowUp(WithdrawFollowUp {
                        thread: ThreadId::new("active").unwrap(),
                        run: RunId::new(run).unwrap(),
                        submission_id: SubmissionId::new("d3b-never-submitted").unwrap(),
                    }),
                ))
                .await
                .unwrap_err();
            assert!(
                matches!(error, workbench_client::ClientError::Server(ref error)
            if error.message == "the run is not the active conversation root of this thread"),
                "{error:?}"
            );
        });
}
