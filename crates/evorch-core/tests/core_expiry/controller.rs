//! External controller. Success is decided by pidfd/stdout events; the only
//! clock use is (a) the causal lower bound of the real ten-second watchdog and
//! (b) the observation windows that prove a negative, which cannot be an event.
use super::observer::{ProcessTree, bounded};
use rustix::process::Signal;
use serde_json::{Value, json};
use std::{
    io, os::unix::process::ExitStatusExt, path::Path, process::Stdio, time::Duration, time::Instant,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
    sync::mpsc,
};

/// Failure watchdogs only (never a success condition, never a speed gate).
const READY: Duration = Duration::from_secs(60);
const EXIT: Duration = Duration::from_secs(30);
const BACKSTOP: Duration = Duration::from_secs(30);
const CELL: Duration = Duration::from_secs(180);
const CLEANUP: Duration = Duration::from_secs(30);
/// The product constant under test.
const EXPIRY: Duration = Duration::from_secs(10);
/// A negative has no event: stay observably alive longer than the watchdog.
const WINDOW: Duration = Duration::from_secs(11);

fn check(ok: bool, what: impl Into<String>) -> io::Result<()> {
    if ok {
        Ok(())
    } else {
        Err(io::Error::other(what.into()))
    }
}

pub fn run_cell(cell: &str) -> io::Result<()> {
    check(
        !cfg!(debug_assertions),
        "core_expiry must be built with --release (the release test binary is the artifact)",
    )?;
    super::observer::ProcessTree::enable_controller_subreaper()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(bounded(CELL, "whole-cell failure watchdog", run(cell)))
    }));
    let cleanup = runtime.block_on(ProcessTree::cleanup_controller(CLEANUP));
    match result {
        Ok(result) => result.and(cleanup),
        Err(_) => {
            cleanup?;
            Err(io::Error::other(
                "cell panicked; controller reclaimed descendants",
            ))
        }
    }
}

fn emit(cell: &str, kind: &str, value: &Value) -> io::Result<()> {
    println!("{value}");
    if let Some(dir) = std::env::var_os("EVORCH_B3_EVIDENCE_DIR") {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            Path::new(&dir).join(format!("expiry-{cell}-{}-{kind}.json", std::process::id())),
            serde_json::to_vec_pretty(value)?,
        )?;
    }
    Ok(())
}

async fn peer_ready(listener: &UnixListener) -> io::Result<(i32, tokio::net::UnixStream)> {
    bounded(READY, "owned fixture READY", async {
        let (mut stream, _) = listener.accept().await?;
        let mut line = [0; 6];
        stream.read_exact(&mut line).await?;
        check(line == *b"READY\n", "invalid peer READY")?;
        let pid = stream
            .peer_cred()?
            .pid()
            .ok_or_else(|| io::Error::other("peer has no PID"))?;
        Ok((pid, stream))
    })
    .await
}

async fn event(lines: &mut mpsc::UnboundedReceiver<String>, prefix: &str) -> io::Result<()> {
    bounded(READY, &format!("worker event {prefix}"), async {
        loop {
            let line = lines
                .recv()
                .await
                .ok_or_else(|| io::Error::other(format!("worker ended before {prefix}")))?;
            if line.starts_with(prefix) {
                return Ok(());
            }
        }
    })
    .await
}

/// The target must sit directly under a guard launched by exactly this worker.
fn guard_owner(tree: &ProcessTree, root: i32) -> io::Result<(i32, Value)> {
    let mut pid = root;
    loop {
        let pin = tree.require(pid)?;
        if pin.identity.parent == tree.owner.identity.pid {
            let bytes = std::fs::read(format!("/proc/{pid}/cmdline"))?;
            let args = bytes
                .split(|b| *b == 0)
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect::<Vec<_>>();
            check(
                args.get(1).is_some_and(|a| a == "--launch"),
                format!("not guard-owned: {args:?}"),
            )?;
            check(
                args.get(4) == Some(&tree.owner.identity.pid.to_string()),
                "guard owner is not the worker",
            )?;
            return Ok((
                pid,
                json!({"identity":pin.identity,"mode":args[1],"owner_pid":args[4],"target_root":root}),
            ));
        }
        pid = pin.identity.parent;
    }
}

async fn run(cell: &str) -> io::Result<()> {
    let artifact = test_support::product_artifact::require_from_env();
    let bundle = artifact
        .guard
        .as_ref()
        .ok_or_else(|| io::Error::other("sealed artifact has no guard bundle"))?
        .bundle
        .clone();
    let dir = test_socket_dir::SocketDir::new();
    let sockets = dir.path().to_path_buf();
    std::fs::create_dir(sockets.join("ipc"))?;
    let root = tempfile::tempdir()?;
    let peer_socket = sockets.join("peer");
    let listener = UnixListener::bind(&peer_socket)?;
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--watchdog-child")
        .env("EVORCH_WATCHDOG_CASE", cell)
        .env("EVORCH_WATCHDOG_ROOT", root.path())
        .env("EVORCH_WATCHDOG_SOCKETS", &sockets)
        .env("EVORCH_EXPIRY_PEER_SOCKET", &peer_socket)
        .env("EVORCH_PROCESS_GUARD_BUNDLE", &bundle)
        .env("XDG_RUNTIME_DIR", &sockets)
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("TOKIO_WORKER_THREADS", "2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut tree = ProcessTree::new(child.id().unwrap() as i32)?;
    let mut stdin = child.stdin.take().unwrap();
    let (tx, mut lines) = mpsc::unbounded_channel();
    let stdout = child.stdout.take().unwrap();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut stderr = child.stderr.take().unwrap();
    let stderr = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });
    let result = async {
        let first_event = if cell == "expiry_signal_first" {
            "PREEXEC"
        } else {
            "BLOCKED"
        };
        event(&mut lines, first_event).await?;
        let (root_pid, _peer_hold) = peer_ready(&listener).await?;
        tree.refresh()?;
        let target = tree.require(root_pid)?.identity.clone();
        let (guard_pid, guard) = guard_owner(&tree, root_pid)?;
        let mut owned = tree
            .descendants
            .values()
            .filter(|p| p.identity.group == target.group)
            .map(|p| p.identity.pid)
            .collect::<Vec<_>>();
        check(
            owned.len() >= 2
                && tree
                    .descendants
                    .values()
                    .any(|p| p.identity.parent == root_pid && p.identity.group == target.group),
            format!("missing same-PGID root/grandchild: {owned:?}"),
        )?;
        owned.push(guard_pid);
        let alive = |tree: &ProcessTree| -> io::Result<()> {
            check(tree.owner.unchanged(), "worker died/changed")?;
            for pid in &owned {
                check(
                    tree.require(*pid)?.unchanged(),
                    format!("owned {pid} died/changed"),
                )?;
            }
            Ok(())
        };
        alive(&tree)?;
        let anchor = match cell {
            "expiry_capture_first" => {
                // Irreversible capture is published (BLOCKED follows it), no
                // explicit shutdown signal yet.
                negative_window(&tree, "capture alone fired the watchdog").await?;
                alive(&tree)?;
                let sent = Instant::now();
                tree.owner.signal(Signal::TERM)?;
                sent
            }
            "expiry_signal_first" => {
                // Signal is pending before any capture exists.
                tree.owner.signal(Signal::TERM)?;
                negative_window(&tree, "signal alone fired the watchdog").await?;
                alive(&tree)?;
                let released = Instant::now();
                stdin.write_all(b"exec\n").await?;
                stdin.flush().await?;
                released
            }
            _ => unreachable!(),
        };
        bounded(BACKSTOP, "exit 74 not observed", tree.owner.exited()).await?;
        let dead_at = Instant::now();
        let elapsed = dead_at - anchor;
        check(
            elapsed >= EXPIRY,
            format!("worker ended {elapsed:?} after the anchor, before ten real seconds"),
        )?;
        let status = child.wait().await?;
        let text = stderr_text(stderr).await;
        check(
            status.code() == Some(74) && status.signal().is_none(),
            format!("expected exit 74, got {status}: {text}"),
        )?;
        check(
            text.contains("planned shutdown exceeded 10s"),
            format!("missing fixed diagnostic: {text}"),
        )?;
        // Owned children are reaped by the guard after the worker's _exit; the
        // controller has not killed anything yet.
        tree.expect_dead(&owned, EXIT).await?;
        emit(
            cell,
            "death",
            &json!({
                "cell":cell,"artifact":"release test binary (not the product ELF)",
                "profile":"release","exit":74,"elapsed_ms":elapsed.as_millis(),
                "worker":tree.owner.identity,"guard":guard,"pidfd_dead":owned,
                "cleanup_not_started":true
            }),
        )?;
        Ok(())
    }
    .await;
    if tree.owner.unchanged() {
        let _ = tree.refresh();
    }
    let cleanup = tree.cleanup(CLEANUP).await;
    let _ = child.wait().await;
    result.and(cleanup)
}

async fn stderr_text(task: tokio::task::JoinHandle<String>) -> String {
    bounded(EXIT, "worker stderr", async {
        task.await.map_err(io::Error::other)
    })
    .await
    .unwrap_or_default()
}

/// Timing out is the pass: the worker must stay alive for the whole window.
async fn negative_window(tree: &ProcessTree, what: &str) -> io::Result<()> {
    check(
        tokio::time::timeout(WINDOW, tree.owner.exited())
            .await
            .is_err(),
        what,
    )
}
