//! Conditional disposal reset, using the existing usage epoch and namespace.
use super::*;

/// Identity of one graph's disposal of one thread namespace. Not an authority,
/// and deliberately independent of operation IDs (a new operation must not
/// erase scratch created after an earlier reset whose ACK was lost).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GraphDisposalKey {
    graph: String,
    tx: String,
    capture: String,
    thread: String,
}

impl GraphDisposalKey {
    pub(crate) fn new(
        graph: String,
        tx: String,
        capture: String,
        thread: String,
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
        })
    }
}

impl ThreadScratch {
    /// Recheck an unused snapshot after runtime/main DB observations, while the
    /// caller holds existing writer/binding authority. Never read runtime locks
    /// from inside this helper.
    ///
    /// None means changed epoch/users (or saturated epoch): retain scratch.
    /// Some(detached) means applied, including readback of the same key. An absent
    /// namespace does not generate a cleanup notification.
    /// Errors leave the ordinary namespace and bookkeeping unchanged.
    ///
    /// Call wake_trash AFTER releasing scratch locks, even on readback.
    /// Retain the key until durable progress has confirmed this step; unknown
    /// ACKs are not permission to forget it. At most 64 supervisors x 64 threads
    /// may be retained; capacity never evicts an unresolved result.
    pub(crate) fn reset_thread_if_unused(
        &self,
        thread: &str,
        epoch: u64,
        effect: &GraphDisposalKey,
    ) -> io::Result<Option<bool>> {
        if effect.thread != thread {
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
        if busy || epoch == u64::MAX || current_epoch != epoch {
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
        if detached {
            state.resets.insert(name, next);
        }
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
