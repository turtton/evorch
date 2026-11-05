//! What the host remembers about commands it took: receipts and provenance.
//!
//! Three bounded, in-memory lookups (the submission one is backed by storage, below):
//!
//! - by session and request id, for `GetCommandReceipt`: the reply or error the
//!   command got. A request id is unique only within its session, so the session
//!   is part of the key;
//! - by submission id, for `SendChat` deduplication: a digest of the whole message
//!   ([`fingerprint::chat_fingerprint`]), the run it went to and who sent it. Both
//!   ingresses (protocol and in-process desktop) record and look up here, so a
//!   resend through either door is recognised;
//! - by question id: who saved the answer first.
//!
//! The same record is written to storage (`host_chat_submissions`), so after a
//! restart, or once the bounded table has dropped an id, [`crate::ingress`] still
//! tells a resend (answered with the original run) from a different message
//! (refused). The runtime's host input receipt carries only the words and cannot
//! prove two messages are the same: an id found only there is refused.
//! Nothing here reaches the model: provenance is bookkeeping beside the command,
//! not part of any prompt.

use std::collections::{HashMap, VecDeque};

use workbench_protocol::commands::CommandReply;
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::ids::{RequestId, SessionId};
use workbench_protocol::queries::{CommandReceipt, CommandReceiptState};

pub mod fingerprint;

/// Entries kept per table. The oldest are dropped first; a dropped submission is
/// still recognised from its stored fingerprint, a dropped request reads as unknown.
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

    fn remove(&mut self, key: &str) {
        if self.entries.remove(key).is_some() {
            self.order.retain(|queued| queued != key);
        }
    }

    /// Insert unless `key` is present; the first value wins.
    fn insert_new(&mut self, key: String, value: V) {
        if self.entries.contains_key(&key) {
            return;
        }
        self.entries.insert(key.clone(), value);
        self.order.push_back(key);
        while self.entries.len() > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

/// A chat message the host accepted.
#[derive(Debug, Clone)]
pub struct SubmissionRecord {
    /// Digest of the whole message ([`fingerprint::chat_fingerprint`]), to tell a
    /// resend from a different message under the same id.
    pub fingerprint: String,
    /// The run it went to.
    pub run_id: Option<String>,
    pub sent_by: Provenance,
}

#[derive(Debug, Clone)]
struct RequestRecord {
    state: CommandReceiptState,
    provenance: Provenance,
    /// What an applied request acted on, so a receipt can be withheld once that was
    /// deleted.
    subject: Subject,
}

/// The submission and run a request's reply names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subject {
    pub submission: Option<String>,
    pub run: Option<String>,
}

pub struct Receipts {
    requests: Bounded<RequestRecord>,
    submissions: Bounded<SubmissionRecord>,
    answers: Bounded<Provenance>,
}

impl Default for Receipts {
    fn default() -> Self {
        Self::with_capacity(RECEIPT_CAPACITY)
    }
}

/// Request ids repeat across sessions. Ids hold no control characters, so the
/// unit separator keeps the pair unambiguous.
fn request_key(session: &SessionId, request: &RequestId) -> String {
    format!("{}\u{1f}{}", session.as_str(), request.as_str())
}

impl Receipts {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            requests: Bounded::new(capacity),
            submissions: Bounded::new(capacity),
            answers: Bounded::new(capacity),
        }
    }

    /// Remember how a request ended.
    pub fn record_request(
        &mut self,
        session: &SessionId,
        request: &RequestId,
        provenance: &Provenance,
        result: &Result<CommandReply, WorkbenchError>,
    ) {
        let subject = match result {
            Ok(reply) => Subject {
                submission: reply
                    .submission_id
                    .as_ref()
                    .map(|id| id.as_str().to_owned()),
                run: reply.run_id.as_ref().map(|id| id.as_str().to_owned()),
            },
            Err(_) => Subject::default(),
        };
        let state = match result {
            Ok(reply) => CommandReceiptState::Applied {
                reply: reply.clone(),
            },
            Err(error) => CommandReceiptState::Rejected {
                error: error.clone(),
            },
        };
        self.requests.insert_new(
            request_key(session, request),
            RequestRecord {
                state,
                provenance: provenance.clone(),
                subject,
            },
        );
    }

    /// What became of `request` in `session`. A request the host has no record of
    /// did not take effect as far as this host can tell, so it reads as unknown.
    pub fn command_receipt(&self, session: &SessionId, request: &RequestId) -> CommandReceipt {
        CommandReceipt {
            request_id: request.clone(),
            state: self
                .requests
                .get(&request_key(session, request))
                .map_or(CommandReceiptState::Unknown, |record| record.state.clone()),
        }
    }

    /// The submission and run an applied request acted on.
    pub fn request_subject(&self, session: &SessionId, request: &RequestId) -> Option<&Subject> {
        self.requests
            .get(&request_key(session, request))
            .map(|record| &record.subject)
    }

    /// Who sent `request`, as remembered; see [`Self::command_receipt`].
    pub fn request_provenance(
        &self,
        session: &SessionId,
        request: &RequestId,
    ) -> Option<&Provenance> {
        self.requests
            .get(&request_key(session, request))
            .map(|r| &r.provenance)
    }

    /// Remember an accepted message. The first record for an id stays.
    pub fn record_submission(&mut self, submission: &str, record: SubmissionRecord) {
        self.submissions.insert_new(submission.to_owned(), record);
    }

    pub fn submission(&self, submission: &str) -> Option<&SubmissionRecord> {
        self.submissions.get(submission)
    }

    /// Drop what is remembered about a submission whose conversation was deleted.
    pub fn forget_submission(&mut self, submission: &str) {
        self.submissions.remove(submission);
    }

    /// Remember who saved the answer to `question`. The first save stays: a
    /// repeat of the same answer never takes the credit.
    pub fn record_answer(&mut self, question: &str, provenance: &Provenance) {
        self.answers
            .insert_new(question.to_owned(), provenance.clone());
    }

    pub fn answer_provenance(&self, question: &str) -> Option<&Provenance> {
        self.answers.get(question)
    }
}
