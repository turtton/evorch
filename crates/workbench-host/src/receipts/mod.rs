//! What the host remembers about commands it took: receipts and provenance.
//!
//! Two lookups, both bounded and in memory:
//!
//! - by request id, for `GetCommandReceipt`: the reply or error the command got;
//! - by submission id, for `SendChat` deduplication: the payload, the reply and
//!   who sent it.
//!
//! The durable half of deduplication is the runtime's host input receipt
//! (`storage::HostInputReceipt`), which survives a restart; [`crate::ingress`]
//! consults it when the in-memory record is gone. This module never reaches the
//! model: provenance is bookkeeping beside the command, not part of any prompt.

use std::collections::{HashMap, VecDeque};

use workbench_protocol::commands::{CommandReply, SendChat};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::ids::RequestId;
use workbench_protocol::queries::{CommandReceipt, CommandReceiptState};

/// Entries kept per table. The oldest are dropped first; a dropped submission
/// falls back to the durable runtime receipt, a dropped request reads as unknown.
pub const RECEIPT_CAPACITY: usize = 4096;

/// A map that forgets its oldest entry when full.
struct Bounded<V> {
    entries: HashMap<String, V>,
    order: VecDeque<String>,
    capacity: usize,
}

impl<V> Bounded<V> {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    fn get(&self, key: &str) -> Option<&V> {
        self.entries.get(key)
    }

    fn insert(&mut self, key: String, value: V) {
        if self.entries.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        while self.entries.len() > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

/// A `SendChat` the host accepted.
#[derive(Debug, Clone)]
pub struct SubmissionRecord {
    /// What was sent, to tell a resend from a different message under the same
    /// id. `None` for submissions made through the in-process desktop path,
    /// which has its own submission machinery and is not deduplicated here.
    pub payload: Option<SendChat>,
    pub reply: CommandReply,
    pub sent_by: Provenance,
}

#[derive(Debug, Clone)]
struct RequestRecord {
    state: CommandReceiptState,
    provenance: Provenance,
}

pub struct Receipts {
    requests: Bounded<RequestRecord>,
    submissions: Bounded<SubmissionRecord>,
}

impl Default for Receipts {
    fn default() -> Self {
        Self::with_capacity(RECEIPT_CAPACITY)
    }
}

impl Receipts {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            requests: Bounded::new(capacity),
            submissions: Bounded::new(capacity),
        }
    }

    /// Remember how a request ended.
    pub fn record_request(
        &mut self,
        request: &RequestId,
        provenance: &Provenance,
        result: &Result<CommandReply, WorkbenchError>,
    ) {
        let state = match result {
            Ok(reply) => CommandReceiptState::Applied {
                reply: reply.clone(),
            },
            Err(error) => CommandReceiptState::Rejected {
                error: error.clone(),
            },
        };
        self.requests.insert(
            request.as_str().to_owned(),
            RequestRecord {
                state,
                provenance: provenance.clone(),
            },
        );
    }

    /// What became of `request`. A request the host has no record of did not
    /// take effect as far as this host can tell, so it reads as unknown.
    pub fn command_receipt(&self, request: &RequestId) -> CommandReceipt {
        CommandReceipt {
            request_id: request.clone(),
            state: self
                .requests
                .get(request.as_str())
                .map_or(CommandReceiptState::Unknown, |record| record.state.clone()),
        }
    }

    /// Who sent `request`.
    pub fn request_provenance(&self, request: &RequestId) -> Option<&Provenance> {
        self.requests.get(request.as_str()).map(|r| &r.provenance)
    }

    pub fn record_submission(&mut self, submission: &str, record: SubmissionRecord) {
        self.submissions.insert(submission.to_owned(), record);
    }

    pub fn submission(&self, submission: &str) -> Option<&SubmissionRecord> {
        self.submissions.get(submission)
    }
}
