use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Command, Stdio},
};
#[allow(dead_code)]
#[path = "../../shared-test-support/exec_fixture.rs"]
mod exec_fixture;

#[path = "../planned_shutdown_watchdog/support.rs"]
mod product;
use product::Child;
#[path = "runner_cleanup.rs"]
mod runner_cleanup;
fn owners(path: &Path) -> serde_json::Value {
    let registry = runtime::ownership::Registry::open_existing(path).unwrap();
    let rows = registry.list().unwrap();
    assert!(!rows.is_empty());
    serde_json::Value::Array(
        rows.into_iter()
            .map(|row| {
                let mut value = serde_json::to_value(row).unwrap();
                // Heartbeat renewal is permitted; ownership/release is not.
                value["lease"].as_object_mut().unwrap().remove("expires_at");
                value
            })
            .collect(),
    )
}
pub(super) fn run(case: &str) -> Result<(), workbench_host::bootstrap::BootstrapError> {
    let product_case = case.starts_with("product_");
    if product_case {
        let _artifact = test_support::product_artifact::require_from_env();
    }
    let root = tempfile::tempdir()?;
    let sockets = test_socket_dir::SocketDir::new();
    std::fs::create_dir(sockets.path().join("ipc"))?;
    let exe = exec_fixture::stripped_copy(
        &std::env::current_exe()?,
        &root.path().join("composition-worker"),
    );
    let (child, _runner_cleanup) = runner_cleanup::spawn(
        Command::new(exe)
            .arg("--watchdog-child")
            .env("EVORCH_WATCHDOG_CASE", case)
            .env("EVORCH_WATCHDOG_ROOT", root.path())
            .env("EVORCH_WATCHDOG_SOCKETS", sockets.path())
            .env("XDG_RUNTIME_DIR", sockets.path())
            .env(
                "XDG_STATE_HOME",
                if product_case {
                    sockets.path().to_path_buf()
                } else {
                    root.path().join("state")
                },
            )
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("TOKIO_WORKER_THREADS", "2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )?;
    let mut child = Child(child);
    let mut stdout = BufReader::new(child.0.stdout.take().unwrap());
    let mut adopted = false;
    let mut scratch = None;
    loop {
        let mut line = String::new();
        if stdout.read_line(&mut line)? == 0 {
            let mut stderr = String::new();
            child.0.stderr.take().unwrap().read_to_string(&mut stderr)?;
            return Err(format!("worker ended before checkpoint: {stderr}").into());
        }
        if line.starts_with("PREEXEC") {
            assert_watchdog_thread(child.0.id());
            child.0.stdin.as_mut().unwrap().write_all(b"exec\n")?;
            child.0.stdin.take();
        }
        if matches!(case, "product_exec" | "product_planned_idle")
            && serde_json::from_str::<serde_json::Value>(&line)
                .ok()
                .is_some_and(|v| v["status"] == "ready")
        {
            return product_success(case, child, root.path());
        }
        if let Some(path) = line.strip_prefix("SCRATCH ") {
            scratch = Some(std::path::PathBuf::from(path.trim_end()));
        }
        if line.starts_with("ADOPTED ") {
            adopted = true;
        }
        if line.starts_with("BLOCKED ") {
            break;
        }
    }
    assert_eq!(adopted, case.starts_with("planned_"));
    let db = if product_case {
        sockets.path().join("evorch/threads/owners.db")
    } else {
        sockets.path().join("owners/owners.db")
    };
    let pending_only = matches!(
        case,
        "cold_executor" | "cold_host_lock" | "cold_stopped_join" | "cold_control_web"
    );
    let mut before = pending_only.then(|| owners(&db));
    let pid = rustix::process::Pid::from_raw(child.0.id() as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM)?;
    // Ordinary shutdown may legitimately mark owners Quiescing before waiting
    // for planned cleanup. Snapshot only after that event, not before SIGTERM.
    if !pending_only {
        let mut line = String::new();
        assert_ne!(stdout.read_line(&mut line)?, 0);
        assert_eq!(line, "SHUTDOWN_CLEANUP\n");
        before = Some(owners(&db));
    }
    let before = before.unwrap();
    let transaction_before = transaction(root.path());
    let status = child.0.wait()?;
    let mut stderr = String::new();
    child.0.stderr.take().unwrap().read_to_string(&mut stderr)?;
    let scratch = scratch.expect("scratch event");
    if case == "cold_cleanup_returns" {
        assert_eq!(status.code(), Some(1), "{stderr}");
        assert!(
            stderr.contains("planned exit prohibited by lifecycle latch"),
            "{stderr}"
        );
        assert!(!stderr.contains("planned shutdown exceeded 10s"));
        // Preserve the existing canceled-capture latch error, not a successful
        // owner/scratch release. OUTER_COMPLETED proves Drop and watcher join.
        let mut output = String::new();
        stdout.read_to_string(&mut output)?;
        assert!(
            output.contains("OUTER_COMPLETED"),
            "outer lifetime did not complete/join: {output}"
        );
    } else {
        assert_eq!(status.code(), Some(74), "{case}: {stderr}");
        assert!(stderr.contains("planned shutdown exceeded 10s"), "{stderr}");
        assert_eq!(before, owners(&db), "expiry changed owner/generation/hold");
        assert_transaction_retained(transaction_before, transaction(root.path()));
        assert_eq!(std::fs::read(&scratch)?, b"watchdog-retained");
    }
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
    if case == "product_after_expiry" {
        cold_after_expiry(root.path(), sockets.path(), &scratch, &db, &before)?;
    }
    println!(
        "{case}: exit {}; completion/expiry disposition checked",
        status.code().unwrap()
    );
    Ok(())
}

fn assert_watchdog_thread(pid: u32) {
    let mut found = false;
    for task in std::fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
        let task = task.unwrap().path();
        if std::fs::read_to_string(task.join("comm"))
            .unwrap()
            .starts_with("planned-shut")
        {
            let status = std::fs::read_to_string(task.join("status")).unwrap();
            let mask = status
                .lines()
                .find_map(|l| l.strip_prefix("SigBlk:"))
                .unwrap();
            let mask = u64::from_str_radix(mask.trim(), 16).unwrap();
            assert_eq!(mask & 0x5002, 0x5002, "watcher blocks INT/TERM/PIPE");
            found = true;
        }
    }
    assert!(found, "OS watchdog must exist before exec");
}
fn product_success(
    case: &str,
    mut child: Child,
    root: &Path,
) -> Result<(), workbench_host::bootstrap::BootstrapError> {
    let identity = core_instance::ConfigIdentity::new(&root.join("config"))?;
    let database = storage::Database::open(&storage::StorageConfig {
        db_path: root.join("config/evorch-events.db"),
        ..Default::default()
    })?;
    let tx = database.restart_transaction("predecessor")?.unwrap();
    assert!(tx.ready_at_ms.is_some());
    assert_eq!(tx.adopted_epoch, Some(tx.metadata.successor_epoch));
    if case == "product_exec" {
        let artifact = test_support::product_artifact::require_from_env();
        assert_eq!(
            std::fs::canonicalize(format!("/proc/{}/exe", child.0.id()))?,
            artifact.exe.canonicalize()?
        );
        let record = identity.read_record()?.unwrap();
        assert_eq!(record.epoch, tx.metadata.successor_epoch);
        product::hello(&record.socket, Some(record.epoch), identity.identity());
        // No capture in the successor incarnation: TERM remains ordinary exit0.
        product::term(&child);
    }
    product::wait(&mut child, 0);
    println!("{case}: raw exec / inherited FD and mask validation / actual planned entry / exit0");
    Ok(())
}
fn cold_after_expiry(
    root: &Path,
    sockets: &Path,
    marker: &Path,
    db: &Path,
    before: &serde_json::Value,
) -> Result<(), workbench_host::bootstrap::BootstrapError> {
    let identity = core_instance::ConfigIdentity::new(&root.join("config"))?;
    let old = identity.read_record()?.unwrap();
    assert!(
        old.socket.exists(),
        "expiry must retain the saved stale socket"
    );
    assert!(std::os::unix::net::UnixStream::connect(&old.socket).is_err());
    let (cold, _cold_cleanup) = runner_cleanup::spawn(&mut product::command(root, sockets))?;
    let mut cold = product::Child(cold);
    let (_out, ready) = product::ready(&mut cold);
    let new = identity.read_record()?.unwrap();
    assert_ne!(old.epoch, new.epoch);
    assert_eq!(ready["socket"].as_str(), new.socket.to_str());
    product::hello(&new.socket, Some(new.epoch), identity.identity());
    assert_eq!(
        owners(db),
        *before,
        "cold recovery must not release/reexecute retained owners"
    );
    assert_eq!(std::fs::read(marker)?, b"watchdog-retained");
    product::term(&cold);
    product::wait(&mut cold, 0);
    Ok(())
}

fn transaction(root: &Path) -> storage::recovery::restart::RestartTransaction {
    storage::Database::open(&storage::StorageConfig {
        db_path: root.join("config/evorch-events.db"),
        ..Default::default()
    })
    .unwrap()
    .restart_transaction("expiry")
    .unwrap()
    .unwrap()
}

fn assert_transaction_retained(
    mut before: storage::recovery::restart::RestartTransaction,
    after: storage::recovery::restart::RestartTransaction,
) {
    if before != after {
        // Existing M2 drain has its own async failure deadline, shorter than
        // watchdog expiry. Preserve it, but never allow adoption/re-exec/Ready.
        assert_eq!(
            after.phase,
            storage::recovery::restart::RestartPhase::Invalidated
        );
        assert!(
            after
                .failure_reason
                .as_deref()
                .unwrap()
                .contains("deadline exceeded")
        );
        assert!(after.revision > before.revision);
        before.phase = after.phase;
        before.revision = after.revision;
        before.failure_reason = after.failure_reason.clone();
    }
    assert_eq!(
        before, after,
        "expiry changed metadata/owner progress/checkpoints/Ready"
    );
}
