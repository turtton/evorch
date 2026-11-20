//! Conditional disposal reset, using the existing usage epoch and namespace.
use super::*;
use crate::ownership::Lease;

/// Identity of one graph's disposal of one owner generation. Not an authority,
/// and deliberately independent of operation IDs (a new operation must not
/// erase scratch created after an earlier reset whose ACK was lost).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GraphDisposalKey {
    graph: String,
    tx: String,
    capture: String,
    thread: String,
    owner_generation: u64,
}

impl GraphDisposalKey {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Q2 leaf (pr07b5aq2) wires the product caller")
    )]
    pub(crate) fn new(
        graph: String,
        tx: String,
        capture: String,
        thread: String,
        owner_generation: u64,
    ) -> io::Result<Self> {
        if [&graph, &tx, &capture, &thread]
            .iter()
            .any(|s| s.is_empty())
        {
            return Err(io::Error::other("invalid graph disposal identity"));
        }
        Ok(Self {
            graph,
            tx,
            capture,
            thread,
            owner_generation,
        })
    }
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "Q2 leaf (pr07b5aq2) wires the product caller")
)]
impl ThreadScratch {
    /// Recheck an unused snapshot after runtime/main DB observations, while the
    /// caller holds the existing owners-registry reservation. Never read runtime
    /// locks from inside that reservation or this helper.
    ///
    /// None means changed epoch/users/lease (or saturated epoch): retain scratch.
    /// Some(detached) means applied, including readback of the same key. An absent
    /// namespace still advances the reset notice, just like reset_thread.
    /// Errors leave the ordinary namespace and bookkeeping unchanged.
    ///
    /// Call wake_trash AFTER releasing registry/scratch locks, even on readback.
    /// Retain the key until durable progress has confirmed this step; unknown
    /// ACKs are not permission to forget it. At most 64 supervisors x 64 threads
    /// may be retained; capacity never evicts an unresolved result.
    pub(crate) fn reset_thread_if_unused(
        &self,
        thread: &str,
        epoch: u64,
        lease: &Lease,
        effect: &GraphDisposalKey,
    ) -> io::Result<Option<bool>> {
        if effect.thread != thread || effect.owner_generation != lease.generation {
            return Err(io::Error::other("graph disposal identity mismatch"));
        }
        let mut state = self.lock();
        if let Some(detached) = state.reset_effects.get(effect) {
            return Ok(Some(*detached));
        }
        let key = ScratchKey::Thread(thread.into());
        let (current_epoch, busy) = state
            .usage
            .get(&key)
            .map_or((0, false), |u| (u.epoch, !u.users.is_empty()));
        if busy
            || epoch == u64::MAX
            || current_epoch != epoch
            || state.leases.get(thread).is_none_or(|current| {
                current.owner_id != lease.owner_id || current.generation != lease.generation
            })
        {
            return Ok(None);
        }
        if state.reset_effects.len() >= 64 * 64 {
            return Err(io::Error::other("scratch reset result capacity exhausted"));
        }
        let name = key.name();
        let next = state
            .resets
            .get(&name)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| io::Error::other("scratch reset generation exhausted"))?;
        let detached = self.root.discard_deferred(&name)?;
        state.resets.insert(name, next);
        state.reset_effects.insert(effect.clone(), detached);
        Ok(Some(detached))
    }

    /// Forget ONLY after the main DB's durable progress has confirmed the scratch
    /// step. The caller must never reset that confirmed step again, including
    /// under a new operation ID. Do not call on timeout/unknown ACK or merely
    /// after scheduling the progress write. Cold runtimes cannot rebuild disposal
    /// authority and therefore cannot replay resets after this memory is lost.
    pub(crate) fn forget_durable_reset(&self, effect: &GraphDisposalKey) {
        self.lock().reset_effects.remove(effect);
    }
}

#[cfg(test)]
mod tests;
