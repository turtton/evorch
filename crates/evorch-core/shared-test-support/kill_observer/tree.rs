use super::{Identity, PinnedProcess, bounded};
use rustix::process::{Signal, WaitOptions, waitpid};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    time::Duration,
};

/// Use only in a dedicated controller, not in a concurrent libtest process.
pub struct ProcessTree {
    pub owner: PinnedProcess,
    pub descendants: BTreeMap<i32, PinnedProcess>,
    kill_owner_on_drop: bool,
    retired: Vec<PinnedProcess>,
}
impl ProcessTree {
    pub fn new(owner: i32) -> io::Result<Self> {
        Ok(Self {
            owner: PinnedProcess::capture(owner)?,
            descendants: BTreeMap::new(),
            kill_owner_on_drop: true,
            retired: Vec::new(),
        })
    }
    pub fn enable_controller_subreaper() -> io::Result<()> {
        rustix::process::set_child_subreaper(Some(rustix::process::getpid()))?;
        Ok(())
    }
    /// Discovery is not authority: pin, revalidate, and recompute reachability
    /// ONLY from validated rows, so a stale ancestor cannot attach a stranger.
    pub fn refresh(&mut self) -> io::Result<()> {
        if !self.owner.unchanged() {
            return Err(io::Error::other("owner changed before tree capture"));
        }
        let mut rows = BTreeMap::new();
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|value| value.parse::<i32>().ok())
            else {
                continue;
            };
            if let Ok(identity) = Identity::read(pid) {
                rows.insert(pid, identity);
            }
        }
        self.refresh_snapshot(&rows)
    }
    /// An untrusted discovery snapshot, injectable to reproduce stale ancestors.
    /// Pinning and reachability validation are identical to `refresh`.
    pub fn refresh_snapshot(&mut self, rows: &BTreeMap<i32, Identity>) -> io::Result<()> {
        if !self.owner.unchanged() {
            return Err(io::Error::other("owner changed before validation"));
        }
        let reachable = |rows: &BTreeMap<i32, Identity>| {
            let mut reached = BTreeSet::from([self.owner.identity.pid]);
            loop {
                let before = reached.len();
                for row in rows.values() {
                    if reached.contains(&row.parent) {
                        reached.insert(row.pid);
                    }
                }
                if before == reached.len() {
                    return reached;
                }
            }
        };
        let candidates = reachable(&rows);
        let mut pins = BTreeMap::new();
        for pid in candidates {
            if pid != self.owner.identity.pid
                && let Some(row) = rows.get(&pid)
                && let Ok(pin) = PinnedProcess::pin(row)
            {
                pins.insert(pid, pin);
            }
        }
        pins.retain(|_, pin| pin.unchanged());
        let validated = pins
            .iter()
            .map(|(pid, pin)| (*pid, pin.identity.clone()))
            .collect();
        if !self.owner.unchanged() {
            return Err(io::Error::other("owner changed during tree capture"));
        }
        let reached = reachable(&validated);
        let mut current = BTreeMap::new();
        for (pid, pin) in pins {
            if reached.contains(&pid) {
                current.insert(pid, pin);
            }
        }
        // Previous pidfds stay pinned for cleanup, never as graph authority.
        self.retired
            .extend(std::mem::replace(&mut self.descendants, current).into_values());
        Ok(())
    }
    pub fn require(&self, pid: i32) -> io::Result<&PinnedProcess> {
        self.descendants
            .get(&pid)
            .ok_or_else(|| io::Error::other(format!("PID {pid} outside validated owner tree")))
    }
    pub async fn expect_dead(&self, pids: &[i32], limit: Duration) -> io::Result<()> {
        use std::{
            future::{Future, poll_fn},
            task::Poll,
        };
        let mut waits = pids
            .iter()
            .map(|pid| self.require(*pid).map(|pin| Box::pin(pin.exited())))
            .collect::<io::Result<Vec<_>>>()?;
        let mut dead = vec![false; pids.len()];
        let result = bounded(
            limit,
            "owned processes survived owner death",
            poll_fn(|cx| {
                for (wait, dead) in waits.iter_mut().zip(&mut dead) {
                    if !*dead {
                        match wait.as_mut().poll(cx) {
                            Poll::Ready(Ok(())) => *dead = true,
                            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                            Poll::Pending => {}
                        }
                    }
                }
                if dead.iter().all(|dead| *dead) {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            }),
        )
        .await;
        result.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "{error}: {:?}",
                    pids.iter()
                        .zip(dead)
                        .filter_map(|(pid, dead)| (!dead).then_some(pid))
                        .collect::<Vec<_>>()
                ),
            )
        })
    }
    /// Run AFTER the assertions, on success and failure. Cleanup isn't evidence.
    /// Escaped PGIDs are included, not dismissed as out of the guarantee.
    pub async fn cleanup(&self, limit: Duration) -> io::Result<()> {
        self.owner.signal(Signal::KILL)?;
        for pin in self.descendants.values().chain(self.retired.iter()) {
            pin.signal(Signal::KILL)?;
        }
        bounded(limit, "controller cleanup", async {
            self.owner.exited().await?;
            for pin in self.descendants.values().chain(self.retired.iter()) {
                pin.exited().await?;
            }
            Self::cleanup_controller(limit).await?;
            Ok(())
        })
        .await
    }
    /// Final controller fence, including panic/failed-start paths before a root
    /// was pinned. This must run in the dedicated subreaper controller only.
    pub async fn cleanup_controller(limit: Duration) -> io::Result<()> {
        let mut child_events =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
        bounded(limit, "controller descendants not reaped", async {
            loop {
                let mut adopted =
                    ProcessTree::new(rustix::process::getpid().as_raw_nonzero().get())?;
                adopted.kill_owner_on_drop = false;
                adopted.refresh()?;
                for pin in adopted.descendants.values() {
                    pin.signal(Signal::KILL)?;
                }
                drop(adopted);
                match waitpid(None, WaitOptions::NOHANG) {
                    Ok(Some(_)) => continue,
                    Ok(None) => {
                        child_events.recv().await;
                    }
                    Err(rustix::io::Errno::CHILD) => break,
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(())
        })
        .await
    }
}
impl Drop for ProcessTree {
    fn drop(&mut self) {
        // Unwind backstop. Normal Result paths additionally await and reap.
        if self.kill_owner_on_drop {
            let _ = self.owner.signal(Signal::KILL);
        }
        for pin in self.descendants.values().chain(self.retired.iter()) {
            let _ = pin.signal(Signal::KILL);
        }
    }
}
