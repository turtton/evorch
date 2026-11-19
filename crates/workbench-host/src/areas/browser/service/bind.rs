//! Binding a data connection to a target the control connection is allowed to read.
use super::*;
use crate::areas::browser::renderer::data::DataPlane;
use std::io::Read;
use workbench_protocol::namespaces::browser::data::{
    BlobSource, BlobStart, MAX_CAPTURE_BLOB_BYTES, MediaType,
};

/// The bytes of a Capture, read without copying them. Holding them keeps their
/// share of the Capture budget.
struct SharedBytes {
    bytes: Arc<HeldBytes>,
    at: usize,
}
impl Read for SharedBytes {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let rest = &self.bytes[self.at..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.at += n;
        Ok(n)
    }
}

impl RendererService {
    /// Authorizes and performs the binding of `channel` to `source`. Every
    /// failure is the same denial: nothing tells a caller whether the channel,
    /// the token or the target exists.
    pub(super) fn bind_data(
        &self,
        state: &mut State,
        channel: &RendererId,
        source: &RendererDataSource,
        client: &ClientId,
        p: &Provenance,
    ) -> Result<(), WorkbenchError> {
        let plane: DataPlane = self.owner.data_plane();
        if !plane.is_pending(channel) || self.gate.phase() != HostRestartPhase::Open {
            return Err(denied());
        }
        let epoch = self.connections.epoch_of(client);
        match source {
            RendererDataSource::Frames {
                subscription,
                token,
            } => {
                let lease = state
                    .subscriptions
                    .get(subscription.as_str())
                    .ok_or_else(denied)?;
                if &lease.client != client
                    || lease.surface != p.surface
                    || lease.connection_epoch != epoch
                    || lease.token != *token
                    || lease.bound.is_some()
                    || lease.expires <= Instant::now()
                {
                    return Err(denied());
                }
                let slot = lease.slot.clone().ok_or_else(denied)?;
                let tab = state
                    .tabs
                    .values()
                    .find(|tab| tab.physical == lease.tab)
                    .ok_or_else(denied)?;
                // The grant that allowed the subscription must still hold.
                let owns =
                    tab.client.as_ref() == Some(client) && self.session_of(state, tab, p).is_ok();
                if !owns && !tab.observers.get(client).is_some_and(|rights| rights.1) {
                    return Err(denied());
                }
                let source = self.owner.frame_source(tab.physical).ok_or_else(denied)?;
                let (lease_out, revocation) = DataPlane::frame_lease(
                    slot,
                    source,
                    tab.physical,
                    tab.public.reference.id.clone(),
                    self.epoch,
                );
                if !plane.bind(channel, lease_out) {
                    return Err(denied());
                }
                state
                    .subscriptions
                    .get_mut(subscription.as_str())
                    .unwrap()
                    .bound = Some(revocation);
                Ok(())
            }
            RendererDataSource::Capture {
                operation_id,
                capture_id,
            } => {
                let capture = state.captures.get(capture_id.as_str()).ok_or_else(denied)?;
                if &capture.client != client
                    || capture.surface != p.surface
                    || capture.connection_epoch != epoch
                    || capture.operation != *operation_id
                    || capture.bound.is_some()
                    || capture.expires <= Instant::now()
                    || capture.bytes.len() as u64 > MAX_CAPTURE_BLOB_BYTES
                {
                    return Err(denied());
                }
                let start = BlobStart {
                    core_epoch: 0,
                    transfer_id: 0,
                    source: BlobSource::Capture,
                    media_type: MediaType::Png,
                    total_bytes: capture.bytes.len() as u64,
                    digest: [0; 32],
                };
                let reader = SharedBytes {
                    bytes: capture.bytes.clone(),
                    at: 0,
                };
                let (lease_out, revocation) = plane
                    .blob_lease(start, Box::new(reader), self.epoch)
                    .ok_or_else(busy)?;
                if !plane.bind(channel, lease_out) {
                    return Err(denied());
                }
                state.captures.get_mut(capture_id.as_str()).unwrap().bound = Some(revocation);
                Ok(())
            }
        }
    }
    /// Binds `channel` to the bytes of an image artifact the caller was allowed
    /// to read. The transfer is the caller's: it ends when the control
    /// connection that asked for it ends, or when the core drains.
    pub(crate) fn bind_image(
        &self,
        channel: &RendererId,
        start: BlobStart,
        reader: Box<dyn Read + Send>,
        p: &Provenance,
    ) -> Result<(), WorkbenchError> {
        let client = local(p)?.clone();
        let plane: DataPlane = self.owner.data_plane();
        let mut state = self.state.lock().unwrap();
        if !plane.is_pending(channel) || self.gate.phase() != HostRestartPhase::Open {
            return Err(denied());
        }
        let (lease, revocation) = plane
            .blob_lease(start, reader, self.epoch)
            .ok_or_else(busy_retry)?;
        if !plane.bind(channel, lease) {
            return Err(denied());
        }
        state.images.insert(
            id().as_str().to_owned(),
            ImageTransfer {
                connection_epoch: self.connections.epoch_of(&client),
                client,
                revocation,
            },
        );
        Ok(())
    }
    fn session_of(
        &self,
        state: &State,
        tab: &LiveTab,
        p: &Provenance,
    ) -> Result<(), WorkbenchError> {
        self.session(state, &tab.public.session, p).map(|_| ())
    }
}
