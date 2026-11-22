//! Harness controls: they test the cell harness, not the product, and are
//! labelled T. `classify_control_t` pins that a live process whose parent
//! changed is not mistaken for a dead one; the two recovery controls run the
//! real `chromium_p` flow in a child controller with an injected harness
//! failure (whole-cell timeout, panic) while the provider's response is held
//! at its gate, and require the child controller to finish in bounded time and
//! every pinned process to be dead.
use super::{
    evidence,
    observer::{Identity, PinnedProcess, ProcessTree, bounded},
    procs::{self, check},
};
use rustix::process::Signal;
use serde_json::{Value, json};
use std::{io, process::Stdio, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};

const EXIT: Duration = Duration::from_secs(30);

pub async fn classify() -> io::Result<()> {
    let mut owner = tokio::process::Command::new("sh")
        .args(["-c", "sleep 1000 & echo READY; wait"])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut tree = ProcessTree::new(owner.id().unwrap() as i32)?;
    let result = async {
        let mut lines = BufReader::new(owner.stdout.take().unwrap()).lines();
        let line = bounded(EXIT, "fixture READY", async {
            lines
                .next_line()
                .await?
                .ok_or_else(|| io::Error::other("fixture ended before READY"))
        })
        .await?;
        check(line == "READY", "fixture READY line")?;
        tree.refresh()?;
        let pids: Vec<i32> = tree.descendants.keys().copied().collect();
        check(
            pids.len() == 1,
            format!("expected one fixture child: {pids:?}"),
        )?;
        let child = pids[0];
        // Re-parent the live child: its owner dies, the controller adopts it.
        tree.owner.signal(Signal::KILL)?;
        bounded(EXIT, "fixture owner death", tree.owner.exited()).await?;
        let _ = bounded(EXIT, "fixture owner reap", async { owner.wait().await }).await?;
        check(
            !tree.require(child)?.unchanged(),
            "the re-parented child still matches its pinned identity (the control is void)",
        )?;
        let moved = procs::classify(&tree, &[child]).await?;
        check(
            moved.alive == [child] && moved.relation_changed == [child] && moved.gone.is_empty(),
            format!("a live re-parented process was mis-classified: {moved:?}"),
        )?;
        tree.require(child)?.signal(Signal::KILL)?;
        bounded(EXIT, "fixture child death", tree.require(child)?.exited()).await?;
        let dead = procs::classify(&tree, &[child]).await?;
        check(
            dead.gone == [child] && dead.alive.is_empty(),
            format!("a dead process was mis-classified: {dead:?}"),
        )?;
        evidence::emit(
            "classify_control_t",
            "death",
            &json!({"artifact": "T", "reparented_alive": moved.alive, "then_dead": dead.gone}),
        )
    }
    .await;
    let cleanup = tree.cleanup(EXIT).await;
    result.and(cleanup)
}

fn identity(value: &Value) -> Option<Identity> {
    let number = |key: &str| value[key].as_i64().and_then(|v| i32::try_from(v).ok());
    Some(Identity {
        pid: number("pid")?,
        parent: number("parent")?,
        group: number("group")?,
        session: number("session")?,
        starttime: value["starttime"].as_u64()?,
        state: value["state"].as_str()?.chars().next()?,
    })
}

pub async fn recovery(cell: &str) -> io::Result<()> {
    let (failure, expected) = if cell.contains("timeout") {
        ("timeout", "whole-cell failure watchdog")
    } else {
        ("panic", "cell panicked")
    };
    let stderr_path =
        std::env::temp_dir().join(format!("b3-08-{cell}-{}.stderr", std::process::id()));
    let mut inner = tokio::process::Command::new(std::env::current_exe()?)
        .args(["--ignored", "--exact", "chromium_p"])
        .env("EVORCH_B3_FAILURE", failure)
        .env("EVORCH_B3_WATCHDOG_SECS", "25")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&stderr_path)?)
        .kill_on_drop(true)
        .spawn()?;
    let tree = ProcessTree::new(inner.id().unwrap() as i32)?;
    let result = async {
        let mut lines = BufReader::new(inner.stdout.take().unwrap()).lines();
        let mut pins: Vec<PinnedProcess> = Vec::new();
        bounded(EXIT, "inner controller reached the held point", async {
            while let Some(line) = lines.next_line().await? {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(held) = value["held"].as_array() {
                    for item in held {
                        let id =
                            identity(item).ok_or_else(|| io::Error::other("bad held identity"))?;
                        pins.push(PinnedProcess::pin(&id)?);
                    }
                    return Ok(());
                }
            }
            Err(io::Error::other(
                "inner controller ended before the held point",
            ))
        })
        .await?;
        check(
            pins.len() >= 4,
            "too few processes held (core, guard, browser...)",
        )?;
        // Every held process is pinned: the failure may now be injected.
        {
            use tokio::io::AsyncWriteExt;
            let mut stdin = inner.stdin.take().unwrap();
            stdin.write_all(b"pinned\n").await?;
            stdin.flush().await?;
        }
        // The inner watchdog (25 s) + cleanup (30 s) + shutdown (5 s) bound it.
        let status = bounded(
            Duration::from_secs(70),
            "inner controller finished",
            async { inner.wait().await },
        )
        .await?;
        check(
            !status.success(),
            format!("inner controller succeeded: {status}"),
        )?;
        let stderr = std::fs::read_to_string(&stderr_path)?;
        check(
            stderr.contains(expected),
            format!(
                "inner failure was not '{expected}': {}",
                stderr.chars().take(400).collect::<String>()
            ),
        )?;
        for pin in &pins {
            bounded(EXIT, "held process death after recovery", pin.exited()).await?;
        }
        evidence::emit(
            cell,
            "death",
            &json!({"artifact": "T", "failure": failure, "inner_exit": status.to_string(),
                "pinned_dead": pins.iter().map(|p| p.identity.pid).collect::<Vec<_>>()}),
        )
    }
    .await;
    let cleanup = tree.cleanup(EXIT).await;
    let _ = std::fs::remove_file(&stderr_path);
    result.and(cleanup)
}
