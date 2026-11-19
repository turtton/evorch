//! The renderer data plane: dedicated connections that carry raw frames and
//! image blobs, bound to an authorization made on a control connection.
//!
//! A data connection first opens a [`Channel`] (limited in number, closed at the
//! end of its binding window) and learns only its one-time id. It carries
//! nothing until the control connection of an authorized caller binds the id to
//! a target: the host, never the connection, decides what flows, and a
//! [`Lease`] is the only thing a bound channel can read from.
//!
//! The public surface is deliberately small: the core opens a channel and waits
//! for its lease. Creating a lease, and binding it, stays inside this crate, so
//! no path skips the authorization that precedes it.
//!
//! A lease ends when the host drops its [`Revocation`] (unsubscribe, observer
//! revoke, tab close, control disconnect, restart): the core stops sending,
//! closes the socket and releases the frame or blob it still holds.
use std::{
    collections::HashMap,
    io::Read,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use renderer_ipc::{FrameWaitEnd, ReceivedFrame, RendererClient, TabId};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};
use workbench_protocol::namespaces::browser::{
    RendererId,
    data::{
        BlobStart, FrameStart, MAX_BLOB_TRANSFERS, MAX_FRAME_BYTES, MAX_FRAME_SUBSCRIPTIONS,
        MAX_UNBOUND_CONNECTIONS,
    },
};

/// The host's half of a lease: dropping it ends the lease.
pub(crate) struct Revocation(#[allow(dead_code)] watch::Sender<()>);

/// The core's half: resolves when the host revokes the lease.
#[derive(Clone)]
pub struct Revoked(watch::Receiver<()>);
impl Revocation {
    /// Whether the core has let go of the lease (its connection ended or it finished).
    pub(crate) fn finished(&self) -> bool {
        self.0.is_closed()
    }
}
impl Revoked {
    /// Resolves once the host has revoked the lease.
    pub async fn wait(&mut self) {
        // The host never sends a value, so this returns only when the sender
        // is dropped.
        while self.0.changed().await.is_ok() {}
    }
}
fn revocation() -> (Revocation, Revoked) {
    let (tx, rx) = watch::channel(());
    (Revocation(tx), Revoked(rx))
}

/// What a bound channel may read.
pub enum Lease {
    Frames(FrameLease),
    Blob(BlobLease),
    AgentFrames(AgentFrameLease),
}

/// Raw frames of one tab, each one the latest at the time it is taken.
pub struct FrameLease {
    /// Holds one of the core-wide subscription places until the lease (and the
    /// socket it feeds) is gone, not just until the subscription is unsubscribed.
    _slot: Arc<OwnedSemaphorePermit>,
    client: Arc<RendererClient>,
    tab: TabId,
    public: RendererId,
    generation: u64,
    epoch: u64,
    after: Option<u64>,
    revoked: Revoked,
}

/// A frame taken from the shared latest slot. It holds the shared allocation
/// (and so its share of the memory budget) until it is dropped.
pub struct FrameOut {
    start: FrameStart,
    frame: Arc<ReceivedFrame>,
}
impl FrameOut {
    pub fn start(&self) -> &FrameStart {
        &self.start
    }
    pub fn bytes(&self) -> &[u8] {
        &self.frame.bgra
    }
}

/// One step of waiting for a frame.
pub enum FrameStep {
    Frame(FrameOut),
    /// Nothing newer yet; waiting is not an error for a still picture.
    Idle,
    /// The tab, the helper or the connection ended.
    Ended,
}

impl FrameLease {
    pub fn revoked(&self) -> Revoked {
        self.revoked.clone()
    }
    /// Blocks up to `wait` for a frame newer than the last one taken.
    pub fn next(&mut self, wait: Duration) -> FrameStep {
        match self.client.wait_newer_frame(self.tab, self.after, wait) {
            Ok(Some(frame)) => {
                let total = frame.bgra.len() as u64;
                let Some(stride) = frame.width.checked_mul(4) else {
                    return FrameStep::Ended;
                };
                if total > MAX_FRAME_BYTES {
                    return FrameStep::Ended;
                }
                self.after = Some(frame.seq);
                FrameStep::Frame(FrameOut {
                    start: FrameStart {
                        core_epoch: self.epoch,
                        generation: self.generation,
                        tab: self.public.clone(),
                        sequence: frame.seq,
                        width: frame.width,
                        height: frame.height,
                        stride,
                        total_bytes: total,
                    },
                    frame,
                })
            }
            Ok(None) => FrameStep::Idle,
            Err(FrameWaitEnd::Closed(_) | FrameWaitEnd::TabGone) => FrameStep::Ended,
        }
    }
}

/// The bytes of one image: a stored artifact's original or a Capture PNG.
pub struct BlobLease {
    start: BlobStart,
    reader: Box<dyn Read + Send>,
    revoked: Revoked,
    _permit: OwnedSemaphorePermit,
}
impl BlobLease {
    pub fn start(&self) -> &BlobStart {
        &self.start
    }
    pub fn revoked(&self) -> Revoked {
        self.revoked.clone()
    }
    /// Fills `buf` from the blob; zero is its end. Blocking.
    ///
    /// # Errors
    /// The underlying read failed.
    pub fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

/// A channel a data connection opened and nothing has bound yet.
pub struct Channel {
    id: RendererId,
    bound: oneshot::Receiver<Lease>,
    _pending: PendingGuard,
}
impl Channel {
    pub fn id(&self) -> &RendererId {
        &self.id
    }
    /// Resolves with the lease once a control connection binds the channel.
    /// `None`: the plane dropped the channel without binding it.
    pub async fn bound(self) -> Option<Lease> {
        self.bound.await.ok()
    }
}
struct PendingGuard {
    plane: Arc<Inner>,
    id: String,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        lock(&self.plane.pending).remove(&self.id);
    }
}

/// The channel limit is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

struct Inner {
    pending: Mutex<HashMap<String, oneshot::Sender<Lease>>>,
    blobs: Arc<Semaphore>,
    frames: Arc<Semaphore>,
    next_transfer: AtomicU64,
}

/// The registry of data channels and the transfer budget.
#[derive(Clone)]
pub struct DataPlane {
    inner: Arc<Inner>,
}
impl Default for DataPlane {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                pending: Mutex::default(),
                blobs: Arc::new(Semaphore::new(MAX_BLOB_TRANSFERS)),
                frames: Arc::new(Semaphore::new(MAX_FRAME_SUBSCRIPTIONS)),
                next_transfer: AtomicU64::new(1),
            }),
        }
    }
}
impl DataPlane {
    /// Opens a channel for a new data connection.
    ///
    /// # Errors
    /// [`Busy`] when too many channels wait for a binding.
    pub fn open_channel(&self) -> Result<Channel, Busy> {
        let id = crate::areas::browser::service::id();
        let (tx, rx) = oneshot::channel();
        let mut pending = lock(&self.inner.pending);
        if pending.len() >= MAX_UNBOUND_CONNECTIONS {
            return Err(Busy);
        }
        pending.insert(id.as_str().to_owned(), tx);
        drop(pending);
        Ok(Channel {
            _pending: PendingGuard {
                plane: self.inner.clone(),
                id: id.as_str().to_owned(),
            },
            id,
            bound: rx,
        })
    }

    /// Whether `channel` waits for a binding. A check for the authorizer; the
    /// binding itself is [`Self::bind`].
    pub(crate) fn is_pending(&self, channel: &RendererId) -> bool {
        lock(&self.inner.pending).contains_key(channel.as_str())
    }

    /// Hands `lease` to the data connection that opened `channel`. `false`: the
    /// channel is unknown, bound, expired or its connection is gone; the lease
    /// is dropped and nothing flows.
    pub(crate) fn bind(&self, channel: &RendererId, lease: Lease) -> bool {
        let Some(sender) = lock(&self.inner.pending).remove(channel.as_str()) else {
            return false;
        };
        sender.send(lease).is_ok()
    }

    /// One of the core-wide frame subscription places. The subscription holds
    /// it and so does its lease once bound, so a subscription that ended but
    /// whose socket is still closing still counts. `None`: all are in use.
    pub(crate) fn frame_slot(&self) -> Option<Arc<OwnedSemaphorePermit>> {
        self.inner
            .frames
            .clone()
            .try_acquire_owned()
            .ok()
            .map(Arc::new)
    }

    /// A frame lease of `tab`, valid until the returned [`Revocation`] drops.
    pub(crate) fn frame_lease(
        slot: Arc<OwnedSemaphorePermit>,
        client: Arc<RendererClient>,
        tab: crate::areas::browser::renderer::Tab,
        public: RendererId,
        epoch: u64,
    ) -> (Lease, Revocation) {
        let (revocation, revoked) = revocation();
        (
            Lease::Frames(FrameLease {
                _slot: slot,
                client,
                tab: tab.id,
                public,
                generation: tab.generation,
                epoch,
                after: None,
                revoked,
            }),
            revocation,
        )
    }

    pub(crate) fn agent_lease(
        slot: Arc<OwnedSemaphorePermit>,
        port: Arc<dyn super::super::observation::AgentObservationPort>,
        scope: workbench_protocol::namespaces::browser::observation::ObservationScope,
        feed: super::super::observation::ObservationFeed,
    ) -> (Lease, Revocation) {
        let (revocation, revoked) = revocation();
        (
            Lease::AgentFrames(AgentFrameLease {
                _slot: slot,
                port,
                scope,
                updates: feed.updates,
                after: None,
                revoked,
            }),
            revocation,
        )
    }
    /// A blob lease reading its reader, valid until the returned [`Revocation`]
    /// drops. `None`: all transfer slots are in use.
    pub(crate) fn blob_lease(
        &self,
        mut start: BlobStart,
        reader: Box<dyn Read + Send>,
        epoch: u64,
    ) -> Option<(Lease, Revocation)> {
        let permit = self.inner.blobs.clone().try_acquire_owned().ok()?;
        start.core_epoch = epoch;
        start.transfer_id = self.inner.next_transfer.fetch_add(1, Ordering::Relaxed);
        let (revocation, revoked) = revocation();
        Some((
            Lease::Blob(BlobLease {
                start,
                reader,
                revoked,
                _permit: permit,
            }),
            revocation,
        ))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Source-native RGBA observation, independent of renderer tab identity.
pub struct AgentFrameLease {
    _slot: Arc<OwnedSemaphorePermit>,
    port: Arc<dyn super::super::observation::AgentObservationPort>,
    scope: workbench_protocol::namespaces::browser::observation::ObservationScope,
    updates: watch::Receiver<Option<Arc<super::super::observation::ObservedPage>>>,
    after: Option<u64>,
    revoked: Revoked,
}
impl AgentFrameLease {
    pub fn revoked(&self) -> Revoked {
        self.revoked.clone()
    }
    pub fn current(&self) -> bool {
        self.revoked.0.has_changed().is_ok() && self.port.current(&self.scope)
    }
    /// Wakes even when invalidation happens during an in-flight transfer.
    pub async fn invalidated(&self) {
        let mut updates = self.updates.clone();
        loop {
            if !self.current() || updates.changed().await.is_err() {
                return;
            }
        }
    }
    pub async fn next(&mut self) -> Option<Arc<super::super::observation::ObservedFrame>> {
        loop {
            if !self.current() || self.updates.has_changed().is_err() {
                return None;
            }
            let page = self.updates.borrow_and_update().clone()?;
            if !page.validate() || page.snapshot.scope != self.scope {
                return None;
            }
            if let Some(frame) = &page.frame
                && self.after.is_none_or(|seq| frame.start.sequence > seq)
            {
                self.after = Some(frame.start.sequence);
                return Some(frame.clone());
            }
            if self.updates.changed().await.is_err() {
                return None;
            }
        }
    }
}
