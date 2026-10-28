//! Per-registration receipts shared by output snapshots and un-injected queues.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use event_bus::AgentMessage;

use crate::RunId;

/// Identity is (recipient, child, generation), never message text or sender alone.
/// Old snapshots keep their receipt even after a same-ID registration replaces it.
#[derive(Debug)]
pub(crate) struct CompletionNotice {
    pub(crate) recipient: Option<RunId>,
    pub(crate) child: RunId,
    pub(crate) generation: u64,
    observed: AtomicBool,
}

impl CompletionNotice {
    pub(crate) fn new(recipient: Option<RunId>, child: RunId, generation: u64) -> Arc<Self> {
        Arc::new(Self {
            recipient,
            child,
            generation,
            observed: AtomicBool::new(false),
        })
    }

    pub(crate) fn observe(&self) {
        self.observed.store(true, Ordering::Release);
    }

    fn observed(&self) -> bool {
        self.observed.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum MessageOrigin {
    Explicit,
    CompletionNotice(Arc<CompletionNotice>),
}

#[derive(Debug, Clone)]
pub(crate) struct QueuedMessage {
    pub(crate) message: AgentMessage,
    pub(crate) origin: MessageOrigin,
}

impl QueuedMessage {
    pub(crate) fn explicit(message: AgentMessage) -> Self {
        Self {
            message,
            origin: MessageOrigin::Explicit,
        }
    }

    pub(crate) fn completion(message: AgentMessage, notice: Arc<CompletionNotice>) -> Self {
        debug_assert_eq!(message.sender_run_id, notice.child.to_string());
        debug_assert_eq!(
            Some(message.recipient_run_id.clone()),
            notice.recipient.map(|run| run.to_string())
        );
        Self {
            message,
            origin: MessageOrigin::CompletionNotice(notice),
        }
    }

    pub(crate) fn observed(&self) -> bool {
        match &self.origin {
            MessageOrigin::Explicit => false,
            MessageOrigin::CompletionNotice(notice) => notice.observed(),
        }
    }

    pub(crate) fn interrupts_wait(&self) -> bool {
        matches!(self.origin, MessageOrigin::Explicit)
    }
}
