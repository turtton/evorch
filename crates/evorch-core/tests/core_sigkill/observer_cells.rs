//! Negative controls exercise the observer independently of product ownership.
use super::{
    observer::{ProcessTree, bounded},
    product::{EXIT, check, peer_ready, quote},
};
use rustix::process::Signal;
use std::{io, path::Path, process::Stdio};
use tokio::{net::UnixListener, process::Command};
pub async fn run(cell: &str, own: &Path, socket: &Path, listener: &UnixListener) -> io::Result<()> {
    let mut command = Command::new(&own);
    command.args(["--peer", "root", socket.to_str().unwrap()]);
    if cell == "observer_escaped" {
        command.arg("escape");
    }
    if cell == "observer_stale_ancestor" {
        command = Command::new("/bin/sh");
        command.args([
            "-c",
            &format!("{} --peer root {} & wait", quote(own), quote(socket)),
        ]);
    }
    let mut child = command.stdout(Stdio::null()).spawn()?;
    let mut tree = ProcessTree::new(child.id().unwrap() as i32)?;
    let result = async {
        let peer = peer_ready(listener).await?;
        tree.refresh()?;
        check(
            !tree.descendants.is_empty(),
            "observer did not discover grandchild",
        )?;
        let mut wrong = tree.owner.identity.clone();
        wrong.starttime += 1;
        check(
            super::observer::PinnedProcess::pin(&wrong).is_err(),
            "stale starttime accepted",
        )?;
        if cell == "observer_escaped" {
            check(
                tree.descendants
                    .values()
                    .all(|p| p.identity.group != tree.owner.identity.group),
                "escaped negative control stayed in owner group",
            )?;
        }
        if cell == "observer_stale_ancestor" {
            let mut rows = tree
                .descendants
                .iter()
                .map(|(pid, pin)| (*pid, pin.identity.clone()))
                .collect::<std::collections::BTreeMap<_, _>>();
            check(
                rows.values().any(|row| row.parent == peer),
                "missing two-level discovery fixture",
            )?;
            rows.get_mut(&peer).unwrap().starttime += 1;
            tree.refresh_snapshot(&rows)?;
            check(
                tree.descendants.is_empty(),
                "stale ancestor attached a live descendant",
            )?;
            tree.refresh()?;
        }
        tree.owner.signal(Signal::KILL)?;
        bounded(EXIT, "owner death", tree.owner.exited()).await?;
        // Root-only kill is a negative control: the orphan MUST survive.
        let leaf = tree.descendants.values().next().unwrap();
        check(
            leaf.unchanged()
                || super::observer::Identity::read(leaf.identity.pid)
                    .is_ok_and(|p| p.starttime == leaf.identity.starttime && p.state != 'Z'),
            "negative-control grandchild died",
        )?;
        Ok(())
    }
    .await;
    let cleanup = tree.cleanup(EXIT).await;
    let _ = child.wait().await;
    for pin in tree.descendants.values() {
        bounded(EXIT, "observer cleanup failed", pin.exited()).await?;
    }
    return result.and(cleanup);
}
