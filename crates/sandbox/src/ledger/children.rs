//! The child half of the ledger: registration, explicit stop, and reaping.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::{Inner, SpawnLedger, UnknownChild, locked};
use crate::process::ProcessGroup;

/// What a registered child is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildKind {
    /// An MCP server over stdio.
    McpStdio,
    /// A sandboxed process: a stdio session or a wrapped command.
    SandboxSession,
    /// A pull request source such as `gh`.
    PullRequestSource,
    /// A shell job. Registered by the shell job runtime (07b-3), not here.
    ShellJob,
    /// An evaluation child.
    Eval,
    /// A target probe.
    Probe,
    /// The core-owned offline renderer helper.
    Renderer,
    Other,
}

impl ChildKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::McpStdio => "mcp-stdio",
            Self::SandboxSession => "sandbox-session",
            Self::PullRequestSource => "pull-request-source",
            Self::ShellJob => "shell-job",
            Self::Eval => "eval",
            Self::Probe => "probe",
            Self::Renderer => "renderer",
            Self::Other => "other",
        }
    }
}

/// Names one registered child. Never reused within a ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChildId(u64);

/// A registered child, as [`SpawnLedger::children`] lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildInfo {
    pub id: ChildId,
    pub kind: ChildKind,
    pub label: Option<String>,
    /// The leader's pid, until its owner has reaped it.
    pub leader: Option<i32>,
    /// Whether [`SpawnLedger::stop_child`] or `terminate_child` signalled it.
    pub stop_requested: bool,
}

/// How many children the ledger has seen leave, and how.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChildStats {
    pub registered: u64,
    /// The leader was reaped: by its owner, or by the ledger's own reaper after
    /// the owner was dropped.
    pub reaped: u64,
    /// The owner let go of the child without confirming a reap. Never a
    /// success: [`SpawnLedger::reap_all_children`] reports it.
    pub unconfirmed: u64,
}

/// Children left the ledger without a confirmed reap, so a restart cannot be
/// sure that none of them outlives the old image.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{} child(ren) left the ledger without a confirmed reap", .0.len())]
pub struct UnconfirmedReap(pub Vec<ChildId>);

/// The ledger's children as observers see them. `epoch` changes on every
/// registration and every departure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChildActivity {
    pub live: usize,
    pub epoch: u64,
}

#[derive(Debug, Clone)]
pub(super) enum ChildGroup {
    Raw(ProcessGroup),
    #[cfg(target_os = "linux")]
    Owned(crate::owned::OwnedGroup),
}
impl ChildGroup {
    fn leader(&self) -> Option<rustix::process::Pid> {
        match self {
            Self::Raw(group) => group.leader(),
            #[cfg(target_os = "linux")]
            Self::Owned(group) => group
                .id()
                .and_then(|pid| rustix::process::Pid::from_raw(pid as i32)),
        }
    }
    fn kill(&self) {
        match self {
            Self::Raw(group) => group.kill(),
            #[cfg(target_os = "linux")]
            Self::Owned(group) => {
                let _ = group.kill();
            }
        }
    }
    fn terminate(&self) {
        match self {
            Self::Raw(group) => group.terminate(),
            #[cfg(target_os = "linux")]
            Self::Owned(group) => {
                let _ = group.terminate();
            }
        }
    }
}
#[derive(Debug)]
pub(super) struct Record {
    kind: ChildKind,
    label: Option<String>,
    group: ChildGroup,
    stop_requested: bool,
}

#[derive(Debug, Default)]
pub(super) struct Table {
    next: u64,
    live: BTreeMap<ChildId, Record>,
    /// Children that left without a confirmed reap. Sticky: nothing here can
    /// be confirmed later, because the ledger holds no handle to the leader.
    unconfirmed: BTreeSet<ChildId>,
    stats: ChildStats,
    epoch: u64,
}

impl Table {
    fn activity(&self) -> ChildActivity {
        ChildActivity {
            live: self.live.len(),
            epoch: self.epoch,
        }
    }
}

pub(super) fn register(
    inner: &Arc<Inner>,
    kind: ChildKind,
    label: Option<&str>,
    group: ChildGroup,
) -> ChildEntry {
    let mut table = locked(&inner.children);
    let id = ChildId(table.next);
    table.next += 1;
    table.live.insert(
        id,
        Record {
            kind,
            label: label.map(str::to_owned),
            group,
            stop_requested: false,
        },
    );
    table.stats.registered += 1;
    table.epoch += 1;
    inner.children_activity.send_replace(table.activity());
    drop(table);
    ChildEntry {
        inner: Some(inner.clone()),
        id,
    }
}

fn depart(inner: &Inner, id: ChildId, reaped: bool) {
    let mut table = locked(&inner.children);
    if table.live.remove(&id).is_none() {
        return;
    }
    if reaped {
        table.stats.reaped += 1;
    } else {
        table.stats.unconfirmed += 1;
        table.unconfirmed.insert(id);
    }
    table.epoch += 1;
    inner.children_activity.send_replace(table.activity());
}

/// The registration of one child, held by whoever reaps it.
///
/// [`reaped`](Self::reaped) says the leader has been reaped. Dropping the
/// entry says the owner let go of the child without confirming that: it leaves
/// the ledger as *unconfirmed*, which [`SpawnLedger::reap_child`] and
/// [`SpawnLedger::reap_all_children`] report as an error. (A dropped
/// guard-backed owner never does this: it hands its guard to a reaper that
/// confirms the reap, unless the guard itself was lost.)
#[derive(Debug)]
pub struct ChildEntry {
    inner: Option<Arc<Inner>>,
    id: ChildId,
}

impl ChildEntry {
    pub const fn id(&self) -> ChildId {
        self.id
    }

    /// The owner reaped the leader: remove the child from the ledger.
    pub fn reaped(mut self) {
        if let Some(inner) = self.inner.take() {
            depart(&inner, self.id, true);
        }
    }
}

impl ChildEntry {
    /// The owner is dropping the child. Its group has been killed; reap the
    /// leader on a thread of the ledger and only then leave the ledger as
    /// reaped. If that cannot be done (no thread, or the leader cannot be
    /// observed) the entry is dropped, which the ledger reports as unconfirmed.
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn reap_in_background(self, mut child: tokio::process::Child) {
        let spawned = std::thread::Builder::new()
            .name("child-reaper".to_owned())
            .spawn(move || {
                let waited = match crate::process::unreaped_pid(&child) {
                    Some(leader) => crate::process::wait_for_exit(leader).is_ok(),
                    None => true,
                };
                if waited && matches!(child.try_wait(), Ok(Some(_))) {
                    self.reaped();
                }
            });
        // A failed spawn dropped the closure, and with it the entry (reported
        // as unconfirmed) and the child (left to Tokio's reaper).
        drop(spawned);
    }
}

impl Drop for ChildEntry {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            depart(&inner, self.id, false);
        }
    }
}

impl SpawnLedger {
    /// The children the ledger owns, oldest first.
    pub fn children(&self) -> Vec<ChildInfo> {
        locked(&self.inner.children)
            .live
            .iter()
            .map(|(id, record)| ChildInfo {
                id: *id,
                kind: record.kind,
                label: record.label.clone(),
                leader: record.group.leader().map(|pid| pid.as_raw_nonzero().get()),
                stop_requested: record.stop_requested,
            })
            .collect()
    }

    pub fn child_stats(&self) -> ChildStats {
        locked(&self.inner.children).stats
    }

    /// A watch on the live child count; for observers and event-driven waits.
    pub fn watch_children(&self) -> tokio::sync::watch::Receiver<ChildActivity> {
        self.inner.children_activity.subscribe()
    }

    /// SIGKILL the process group of a registered child.
    ///
    /// Nothing is sent once the owner has reaped the leader, so a reused pid
    /// is never signalled.
    ///
    /// # Errors
    /// [`UnknownChild`] when the ledger does not hold `id`.
    pub fn stop_child(&self, id: ChildId) -> Result<(), UnknownChild> {
        self.signal(id, ChildGroup::kill)
    }

    /// SIGTERM the process group of a registered child.
    ///
    /// # Errors
    /// [`UnknownChild`] when the ledger does not hold `id`.
    pub fn terminate_child(&self, id: ChildId) -> Result<(), UnknownChild> {
        self.signal(id, ChildGroup::terminate)
    }

    fn signal(&self, id: ChildId, send: fn(&ChildGroup)) -> Result<(), UnknownChild> {
        let mut table = locked(&self.inner.children);
        let record = table.live.get_mut(&id).ok_or(UnknownChild(id))?;
        record.stop_requested = true;
        send(&record.group);
        Ok(())
    }

    /// SIGKILL every registered child's group; returns how many were signalled.
    ///
    /// Children registered later are not covered: close the fence first.
    pub fn stop_all_children(&self) -> usize {
        let mut table = locked(&self.inner.children);
        for record in table.live.values_mut() {
            record.stop_requested = true;
            record.group.kill();
        }
        table.live.len()
    }

    /// Wait until the child has left the ledger. Resolves at once for an id
    /// that is not held.
    ///
    /// # Errors
    /// [`UnconfirmedReap`] when it left without a confirmed reap.
    pub async fn reap_child(&self, id: ChildId) -> Result<(), UnconfirmedReap> {
        loop {
            let mut activity = self.inner.children_activity.subscribe();
            {
                let table = locked(&self.inner.children);
                if !table.live.contains_key(&id) {
                    return if table.unconfirmed.contains(&id) {
                        Err(UnconfirmedReap(vec![id]))
                    } else {
                        Ok(())
                    };
                }
            }
            if activity.changed().await.is_err() {
                return Ok(());
            }
        }
    }

    /// Wait until the ledger holds no child, and say whether every one of them
    /// was confirmed reaped. With the fence open, children registered
    /// meanwhile are waited for too.
    ///
    /// # Errors
    /// [`UnconfirmedReap`] naming every child that left without a confirmed
    /// reap, ever. The restart must not go on as if the children were gone.
    pub async fn reap_all_children(&self) -> Result<(), UnconfirmedReap> {
        let mut activity = self.inner.children_activity.subscribe();
        let _ = activity.wait_for(|activity| activity.live == 0).await;
        let table = locked(&self.inner.children);
        if table.unconfirmed.is_empty() {
            Ok(())
        } else {
            Err(UnconfirmedReap(table.unconfirmed.iter().copied().collect()))
        }
    }
}
