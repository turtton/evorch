//! The child half of the ledger: registration, explicit stop, and reaping.

use std::collections::BTreeMap;
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
    /// The owner reaped the leader.
    pub reaped: u64,
    /// The owner dropped the child without reaping it (its own `Drop` stopped
    /// the group and handed the leader to a background reaper).
    pub released: u64,
}

/// The ledger's children as observers see them. `epoch` changes on every
/// registration and every departure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChildActivity {
    pub live: usize,
    pub epoch: u64,
}

#[derive(Debug)]
pub(super) struct Record {
    kind: ChildKind,
    label: Option<String>,
    group: ProcessGroup,
    stop_requested: bool,
}

#[derive(Debug, Default)]
pub(super) struct Table {
    next: u64,
    live: BTreeMap<ChildId, Record>,
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
    group: &ProcessGroup,
) -> ChildEntry {
    let mut table = locked(&inner.children);
    let id = ChildId(table.next);
    table.next += 1;
    table.live.insert(
        id,
        Record {
            kind,
            label: label.map(str::to_owned),
            group: group.clone(),
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
        table.stats.released += 1;
    }
    table.epoch += 1;
    inner.children_activity.send_replace(table.activity());
}

/// The registration of one child, held by whoever reaps it.
///
/// [`reaped`](Self::reaped) says the leader has been reaped; dropping the
/// entry says the owner let go of it some other way. Either way the child
/// leaves the ledger, and [`SpawnLedger::reap_child`] resolves.
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
        self.signal(id, ProcessGroup::kill)
    }

    /// SIGTERM the process group of a registered child.
    ///
    /// # Errors
    /// [`UnknownChild`] when the ledger does not hold `id`.
    pub fn terminate_child(&self, id: ChildId) -> Result<(), UnknownChild> {
        self.signal(id, ProcessGroup::terminate)
    }

    fn signal(&self, id: ChildId, send: fn(&ProcessGroup)) -> Result<(), UnknownChild> {
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

    /// Wait until the child has left the ledger: its owner reaped it, or gave
    /// it up. Resolves at once for an id that is not held.
    pub async fn reap_child(&self, id: ChildId) {
        loop {
            let mut activity = self.inner.children_activity.subscribe();
            if !locked(&self.inner.children).live.contains_key(&id) {
                return;
            }
            if activity.changed().await.is_err() {
                return;
            }
        }
    }

    /// Wait until the ledger holds no child. With the fence open, children
    /// registered meanwhile are waited for too.
    pub async fn reap_all_children(&self) {
        let mut activity = self.inner.children_activity.subscribe();
        let _ = activity.wait_for(|activity| activity.live == 0).await;
    }
}
