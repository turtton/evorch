//! T/API owner process, deliberately not product-core evidence.
use super::{
    observer::{ProcessTree, bounded},
    product::{EXIT, READY, check, guard_owner, peer_ready, quote, script},
};
use rustix::process::Signal;
use serde_json::{Value, json};
use std::{fs, io, path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixListener,
};
pub async fn run(
    cell: &str,
    root: &Path,
    peer: &Path,
    socket: &Path,
    listener: &UnixListener,
) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;
    script(
        &root.join("external-peer"),
        &format!("exec {} --peer root {}", quote(peer), quote(socket)),
    )?;
    let artifact = test_support::product_artifact::require_from_env();
    let trace = root.join("exec-trace");
    let mut child = super::trace::command(&std::env::current_exe()?, Some(trace.as_path()))
        .arg("--t-owner")
        .arg(cell)
        .arg(root)
        .arg(peer)
        .arg(socket)
        .env(
            "EVORCH_PROCESS_GUARD_BUNDLE",
            artifact.guard.as_ref().unwrap().bundle.as_os_str(),
        )
        .env("HOME", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut tree = ProcessTree::new(child.id().unwrap() as i32)?;
    let result = async {
        let root_pid = peer_ready(listener).await?;
        tree.refresh()?;
        let root = tree.require(root_pid)?.identity.clone();
        let mut pids = tree.descendants.values()
            .filter(|p| p.identity.group == root.group)
            .map(|p| p.identity.pid).collect::<Vec<_>>();
        check(pids.len() >= 2 && tree.descendants.values().any(|p|
            p.identity.parent == root_pid && p.identity.group == root.group),
            "T root/grandchild missing")?;
        let guard = guard_owner(&tree, root_pid)?;
        let tracer = super::trace::tracer()?;
        let mut trace_worker = None;
        if cell == "js_direct_t" {
            let worker = tree.descendants.values().find(|p|
                fs::read(format!("/proc/{}/cmdline", p.identity.pid)).is_ok_and(|c|
                    c.split(|b| *b == 0).any(|a| a == b"js-worker")))
                .ok_or_else(|| io::Error::other("actual DirectSandbox js-worker missing"))?;
            super::trace::assert_no_process_spawn(&trace, worker.identity.pid)?;
            super::evidence::emit(cell, "worker", &json!({
                "worker":worker.identity,
                "sha256":super::evidence::digest(Path::new(&format!("/proc/{}/exe", worker.identity.pid)))?,
                "injected_direct":true
            }))?;
            pids.push(worker.identity.pid);
            trace_worker = Some(worker.identity.pid);
        }
        child.stdin.as_mut().unwrap().write_all(b"LEDGER\n").await?;
        let line = bounded(READY, "T ledger report",
            BufReader::new(child.stdout.take().unwrap()).lines().next_line())
            .await?.ok_or_else(|| io::Error::other("no ledger report"))?;
        let registered: Value = serde_json::from_str(&line).map_err(io::Error::other)?;
        let expected = match cell {
            "shell_direct_t" => Some("shell-job"),
            "js_direct_t" | "lsp_t" => Some("sandbox-session"),
            "quota_t" | "legacy_stdio_t" => Some("other"),
            "probe_t" => Some("probe"),
            "renderer_t" => Some("renderer"),
            "chromium_t" => None,
            _ => unreachable!(),
        };
        if let Some(kind) = expected {
            check(registered["children"].as_array().is_some_and(|children|
                children.iter().any(|child| child["kind"] == kind)),
                "T caller missing expected kind in its ledger")?;
        } else {
            check(registered["registered"] == 0, "Chromium unexpectedly registered in a ledger")?;
        }
        super::evidence::emit(cell, "ready", &json!({
            "cell":cell,"artifact":"T","core_owner":false,
            "owner":tree.owner.identity,"guard":guard,"ledger":registered,
            "tree":tree.descendants.values().map(|p| &p.identity).collect::<Vec<_>>()
        }))?;
        for pid in &pids {
            check(tree.require(*pid)?.unchanged(), "T target died/changed before owner KILL")?;
        }
        tree.owner.signal(Signal::KILL)?;
        bounded(EXIT, "T owner death", tree.owner.exited()).await?;
        tree.expect_dead(&pids, EXIT).await?;
        use std::os::unix::process::ExitStatusExt;
        let status=child.wait().await?;
        check(status.signal()==Some(9),format!("T owner not SIGKILL: {status}"))?;
        super::trace::finish(cell, &trace, trace_worker, &tracer, &guard).await?;
        super::evidence::emit(cell, "death", &json!({
            "cell":cell,"core_owner":false,"pidfd_dead":pids,"cleanup_not_started":true
        }))?;
        Ok(())
    }.await;
    if tree.owner.unchanged() {
        let _ = tree.refresh();
    }
    let cleanup = tree.cleanup(EXIT).await;
    let _ = child.wait().await;
    result.and(cleanup)
}
