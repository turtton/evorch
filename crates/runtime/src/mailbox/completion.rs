//! Per-registration and idle-turn receipts shared by output snapshots and queues.
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

    pub(crate) fn observed(&self) -> bool {
        self.observed.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum MessageOrigin {
    Explicit,
    CompletionNotice(Arc<CompletionNotice>),
    QuestionNotice(Arc<super::QuestionNotice>),
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

    pub(crate) fn question(message: AgentMessage, notice: Arc<super::QuestionNotice>) -> Self {
        Self {
            message,
            origin: MessageOrigin::QuestionNotice(notice),
        }
    }

    pub(crate) fn question_notice(&self) -> Option<&Arc<super::QuestionNotice>> {
        if let MessageOrigin::QuestionNotice(notice) = &self.origin {
            Some(notice)
        } else {
            None
        }
    }

    pub(crate) fn into_message(self) -> AgentMessage {
        if let Some(notice) = self.question_notice() {
            notice.observe();
        }
        self.message
    }

    pub(crate) fn observed(&self) -> bool {
        match &self.origin {
            MessageOrigin::Explicit => false,
            MessageOrigin::QuestionNotice(notice) => notice.observed(),
            MessageOrigin::CompletionNotice(notice) => notice.observed(),
        }
    }

    pub(crate) fn interrupts_wait(&self) -> bool {
        !matches!(self.origin, MessageOrigin::CompletionNotice(_))
    }
}
