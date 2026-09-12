use std::sync::Arc;

use workspace_ui::ThreadRunPhase;

/// Opaque render token, bound to one pane/thread model lifetime.
#[derive(Debug, Clone)]
pub struct DisplayRevision {
    lifetime: Arc<()>,
    seq: u64,
}

#[derive(Debug, thiserror::Error)]
#[error("attention state change sequence exhausted")]
pub struct SequenceExhausted;

/// Own one instance per pane/thread pair; recreate it on boot or target replacement.
#[derive(Debug)]
pub struct AttentionAck {
    revision: DisplayRevision,
    acknowledged_seq: u64,
    phase: ThreadRunPhase,
}

impl Default for AttentionAck {
    fn default() -> Self {
        Self {
            revision: DisplayRevision {
                lifetime: Arc::new(()),
                seq: 0,
            },
            acknowledged_seq: 0,
            phase: ThreadRunPhase::Pending,
        }
    }
}

impl AttentionAck {
    pub fn observe(&mut self, phase: ThreadRunPhase) -> Result<(), SequenceExhausted> {
        if self.phase != phase {
            let next = self.revision.seq.checked_add(1).ok_or(SequenceExhausted)?;
            self.revision.seq = next;
            self.phase = phase;
        }
        Ok(())
    }

    /// Capture before drawing; pass back only if that exact surface was displayed.
    pub fn revision(&self) -> DisplayRevision {
        self.revision.clone()
    }

    pub fn acknowledge_surface(
        &mut self,
        displayed: Option<&DisplayRevision>,
        outer_focused: Option<bool>,
    ) -> bool {
        let Some(displayed) = displayed else {
            return false;
        };
        if outer_focused != Some(true)
            || !Arc::ptr_eq(&self.revision.lifetime, &displayed.lifetime)
            || self.revision.seq != displayed.seq
        {
            return false;
        }
        self.acknowledged_seq = self.revision.seq;
        true
    }

    pub const fn is_unread(&self) -> bool {
        match self.phase {
            ThreadRunPhase::Waiting | ThreadRunPhase::Done | ThreadRunPhase::Error => {
                self.revision.seq > self.acknowledged_seq
            }
            ThreadRunPhase::Pending | ThreadRunPhase::Running => false,
        }
    }

    pub const fn phase(&self) -> ThreadRunPhase {
        self.phase
    }

    pub const fn state_change_seq(&self) -> u64 {
        self.revision.seq
    }

    pub const fn acknowledged_seq(&self) -> u64 {
        self.acknowledged_seq
    }
}
