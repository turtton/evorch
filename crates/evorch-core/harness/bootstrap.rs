//! Opt-in process fixture. Fault injection is not part of product argv.
#[cfg(target_os = "linux")]
fn main() {
    linux::run().unwrap();
}
#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
mod linux {
    use core_instance::{
        ConfigIdentity, Instance, LaunchGuard,
        handoff::{
            BlockedShutdownSignals, HandoffRecord,
            boot::{BootDeadline, BootFailure},
            entry::{BootMode, EntryOptions},
            receipt::RestartBootReceipt,
        },
    };
    use evorch_core::planned_bootstrap::PlannedBoot;
    use std::{
        collections::BTreeSet,
        ffi::OsString,
        io::{BufRead, Write},
        path::PathBuf,
        sync::Arc,
    };
    use storage::recovery::restart::*;
    use workbench_host::bootstrap::{BootstrapError, BootstrapOptions};

    fn say(value: serde_json::Value) {
        println!("{value}");
        std::io::stdout().flush().unwrap();
    }
    fn wait(automatic: bool) {
        if !automatic {
            let mut line = String::new();
            assert_ne!(std::io::stdin().lock().read_line(&mut line).unwrap(), 0);
        }
    }
    pub fn run() -> Result<(), BootstrapError> {
        let signals = BlockedShutdownSignals::at_entry()?;
        let entry = EntryOptions::parse(std::env::args_os().skip(1))?;
        let value = |key: &str| -> OsString {
            entry.args.windows(2).find(|a| a[0] == key).unwrap()[1].clone()
        };
        let config = ConfigIdentity::new(&PathBuf::from(value("--config-dir")))?;
        let options = BootstrapOptions {
            config_dir: config.config_dir().into(),
            ownership_root: value("--owners").into(),
            project_root: value("--project").into(),
        };
        let parent = PathBuf::from(value("--ipc-parent"));
        let scenario = value("--scenario").into_string().unwrap();
        let automatic = scenario == "measure";
        match entry.mode {
            BootMode::Cold => {
                let build = core_instance::handoff::running_elf_identity()?;
                let launch = LaunchGuard::acquire(&config)?;
                let instance = Instance::acquire(&launch, &parent)?;
                drop(launch);
                let database = storage::StorageConfig {
                    db_path: options.config_dir.join("evorch-events.db"),
                    ..Default::default()
                };
                let storage = storage::Storage::open(database)?;
                let writer = storage.handle();
                let metadata = RestartMetadata {
                    restart_id: "restart".into(),
                    capture_id: "capture".into(),
                    config_identity: config.identity().into(),
                    old_epoch: 1,
                    successor_epoch: 2,
                    old_build: build.clone(),
                    successor_build: build.clone(),
                    target_identity: "fixture".into(),
                    handoff_version: 1,
                    snapshot_version: 1,
                    storage_version: 65,
                    old_builtin_tool_names: BTreeSet::new(),
                    old_compiled_catalog_fingerprint: None,
                    new_compiled_catalog_fingerprint: None,
                    old_effective_tools_fingerprint: None,
                    new_effective_tools_fingerprint: None,
                };
                let mut tx = writer.mutate_restart(
                    "restart".into(),
                    None,
                    RestartMutation::Create(Box::new(metadata)),
                )?;
                tx = writer.mutate_restart(
                    "restart".into(),
                    Some(tx.revision),
                    RestartMutation::BeginCapture(vec![]),
                )?;
                tx = writer.mutate_restart(
                    "restart".into(),
                    Some(tx.revision),
                    RestartMutation::Commit,
                )?;
                let old = runtime::ownership::OwnerHost::open_planned(
                    &options.ownership_root,
                    config::OwnershipConfig::default(),
                    Arc::new(event_bus::EventBus::new(64)),
                )?;
                old.start("idle")?;
                if scenario == "partial_claim" {
                    old.start("z-idle")?;
                }
                for hold in old.hold_for_restart("restart", "capture", &Default::default())? {
                    tx = writer.mutate_restart(
                        "restart".into(),
                        Some(tx.revision),
                        RestartMutation::OwnerProgress(RestartOwnerProgress {
                            thread_id: hold.thread_id,
                            old_owner_id: hold.owner_id,
                            old_generation: hold.generation,
                            active_runs: hold.active_runs,
                            parked_runs: hold.parked_runs,
                            hold_saved: true,
                            hold_read_back: true,
                            worker_joined: false,
                            socket_unlinked: false,
                            hold_removed: false,
                        }),
                    )?;
                }
                writer.mutate_restart(
                    "restart".into(),
                    Some(tx.revision),
                    RestartMutation::Arm {
                        nonce_hash: "fixture".into(),
                    },
                )?;
                config.write_restart_receipt(&RestartBootReceipt::restarting(
                    &config,
                    "restart".into(),
                    1,
                    2,
                )?)?;
                storage.close();
                if scenario == "storage" {
                    std::fs::write(options.config_dir.join("config.toml"), "[invalid")?;
                }
                old.stop_worker_for_exec()?;
                drop(old);
                say(serde_json::json!({"status":"exec", "pid":std::process::id()}));
                let record = HandoffRecord {
                    restart_id: "restart".into(),
                    config_identity: config.identity().into(),
                    predecessor_epoch: if scenario == "epoch" { 0 } else { 1 },
                    successor_epoch: 2,
                    nonce_hash: if scenario == "rejected" {
                        "wrong"
                    } else {
                        "fixture"
                    }
                    .into(),
                    successor_build: build,
                    boot_deadline: BootDeadline::start()?,
                };
                // No live writer/owner workers or child processes remain.
                unsafe {
                    instance.exec_handoff(
                        &signals,
                        &std::env::current_exe()?,
                        &entry.args,
                        record,
                    )?;
                }
            }
            BootMode::PlannedHandoff(handoff) => {
                // Single-threaded process entry, before opening any other FD.
                let inherited = unsafe { handoff.inspect()? };
                if scenario == "physical_deadline" {
                    let deadline = inherited.record().boot_deadline;
                    let marker = options.config_dir.join("physical-dropped");
                    struct DropMarker(PathBuf);
                    impl Drop for DropMarker {
                        fn drop(&mut self) {
                            std::fs::write(&self.0, "dropped").unwrap();
                        }
                    }
                    let _ = PlannedBoot::adopt_entry_with_physical_hook(
                        config,
                        &parent,
                        inherited,
                        |timer| {
                            let _never_drop = DropMarker(marker);
                            say(serde_json::json!({
                                "status":"physical_blocked",
                                "threads":std::fs::read_dir("/proc/self/task")?.count(),
                                "restarting_ns":deadline.restarting_ns(),
                                "deadline_ns":deadline.deadline_ns(),
                            }));
                            // This substitutes an indefinitely blocked ELF/FS
                            // check. IPC advances injected time, not wall time.
                            wait(false);
                            timer.advance_clock_for_test(deadline.deadline_ns() - 1)?;
                            say(serde_json::json!({"status":"before_deadline"}));
                            wait(false);
                            timer.advance_clock_for_test(deadline.deadline_ns())?;
                            // No sender will release this read; only the real
                            // kernel timer's async-signal-safe _exit can finish.
                            loop {
                                wait(false);
                            }
                        },
                    )?;
                    unreachable!("physical Adopt must not return");
                }
                let (mut boot, proof) = PlannedBoot::adopt_entry(config, &parent, inherited)?;
                let inbox = signals.inbox()?;
                say(serde_json::json!({"status":"physical", "pid":std::process::id()}));
                wait(automatic);
                if scenario == "ack_unknown" {
                    boot.fail(BootFailure::AdoptAckUnknown, None);
                }
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(async move {
                    let stage = boot.open_storage(options, &inbox).await;
                    let writer = stage.writer();
                    if matches!(scenario.as_str(), "adopt_ack" | "adopt_retry") {
                        writer.inject_restart_write_fault(scenario == "adopt_ack")?;
                    }
                    let mut pending = boot.finish_bootstrap(stage, proof, &inbox).await;
                    say(serde_json::json!({
                        "status":"prepared",
                        "timings_ms":pending.measurements().0.iter()
                            .map(|(s,d)| (format!("{s:?}"),d.as_secs_f64()*1000.))
                            .collect::<Vec<_>>(),
                        "blocked":pending.blocked_threads,
                    }));
                    wait(automatic);
                    if scenario == "before_ready" {
                        pending.fail_boot(BootFailure::AdoptedBeforeReady);
                    }
                    if scenario == "ready_ack" { writer.inject_restart_write_fault(true)?; }
                    let listener = match pending.bind_control() {
                        Ok(listener) => listener,
                        Err(_) => pending.fail_boot(BootFailure::AdoptedBeforeReady),
                    };
                    let (mut host, instance, listener, measurements) =
                        pending.confirm_ready(listener, 123, &inbox).await;
                    say(serde_json::json!({
                        "status":"ready", "pid":std::process::id(), "epoch":2,
                        "services":host.services.names().collect::<Vec<_>>(),
                        "agents":host.runtime.list_agents().len(),
                        "probe":host.planned_probe_stats().map(|s| serde_json::json!({"registered":s.registered,"reaped":s.reaped,"unconfirmed":s.unconfirmed})),
                        "timings_ms":measurements.0.iter()
                            .map(|(s,d)|(format!("{s:?}"),d.as_secs_f64()*1000.))
                            .collect::<Vec<_>>(),
                    }));
                    wait(automatic);
                    // Activation is separate from Ready; this fixture invokes the
                    // production hook only after the parent has audited Ready.
                    host.activate_after_ready(&evorch_core::recovery_service::CoreGraphRecovery)
                        .await?;
                    say(serde_json::json!({
                        "status":"activated",
                        "services":host.services.names().collect::<Vec<_>>(),
                        "admin_read":host.admin.reader().read(|| Ok(123u64)).await.unwrap(),
                    }));
                    host.shutdown().await?;
                    drop(listener);
                    drop(host);
                    drop(instance);
                    Ok::<_, BootstrapError>(())
                })?;
            }
        }
        Ok(())
    }
}
