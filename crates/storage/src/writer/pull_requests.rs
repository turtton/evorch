//! The pull request tables' writes and reads through the single writer (ADR 0043).
//!
//! Every method runs on the writer thread in one immediate transaction, so concurrent
//! callers and other evorch processes are serialized by SQLite and each write is a
//! compare-and-swap on the state it names. Writes are refused while the storage limit
//! suspends them; reads are not.

use std::sync::mpsc;

use rusqlite::Connection;

use super::{Command, StorageHandle, lanes::LaneJob};
use crate::StorageError;
use crate::pull_requests::{
    CandidatePresented, CandidateState, FetchCommit, FetchFailure, InheritanceCursor,
    InheritanceRecord, InheritanceResolution, InheritanceSkipReason, LinkPullRequest, LinkReceipt,
    NewPullRequestCandidate, NewPullRequestInheritance, NotificationKey, PullRequestCandidate,
    PullRequestFacts, PullRequestKey, PullRequestLink, PullRequestSnapshot, ThreadPullRequestState,
    UnlinkReceipt,
};
use crate::repo::pull_requests as repo;

impl StorageHandle {
    fn pull_request_write<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, StorageError> + Send + 'static,
    ) -> Result<T, StorageError> {
        self.pull_request_job(true, work)
    }

    fn pull_request_read<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, StorageError> + Send + 'static,
    ) -> Result<T, StorageError> {
        self.pull_request_job(false, work)
    }

    fn pull_request_job<T: Send + 'static>(
        &self,
        writes: bool,
        work: impl FnOnce(&Connection) -> Result<T, StorageError> + Send + 'static,
    ) -> Result<T, StorageError> {
        let (reply, result) = mpsc::channel();
        let job: LaneJob = Box::new(move |conn, suspended| {
            let outcome = if writes && suspended {
                Err(StorageError::Serialization(
                    "pull request writes suspended by storage limit".into(),
                ))
            } else {
                work(conn)
            };
            let _ = reply.send(outcome);
        });
        self.0
            .send(Command::PullRequest(job))
            .map_err(|_| StorageError::WriterClosed)?;
        result.recv().map_err(|_| StorageError::WriterClosed)?
    }

    /// Links a thread to a pull request as an explicit decision. Ends the thread's
    /// previous active link (`relinked`), creates the new one, advances the lifecycle
    /// revision, cancels the thread's own pending inheritance and confirms a matching
    /// candidate, all in one transaction. Linking the pull request the thread is already
    /// explicitly linked to changes nothing.
    ///
    /// # Errors
    /// `StaleRevision { kind: "pull_request_lifecycle" }` when `expected_revision` is not
    /// the thread's current one, `PullRequest(Invalid)`, `Tombstoned`, suspended writes or
    /// a closed writer.
    pub fn link_pull_request(&self, request: LinkPullRequest) -> Result<LinkReceipt, StorageError> {
        self.pull_request_write(move |conn| repo::link(conn, &request))
    }

    /// Ends the thread's active link as `unlinked` from `expected_revision`, the
    /// lifecycle revision the caller read, and cancels the thread's own pending
    /// inheritance in the same transaction. A thread with a pending inheritance but no
    /// link can be unlinked too: that declines the inheritance (`ended` is `None`). The
    /// parent's and children's rows are not touched.
    ///
    /// # Errors
    /// `StaleRevision` when the thread moved on (a later link is never erased),
    /// `PullRequest(NotLinked)` when there is neither a link nor a pending inheritance,
    /// `Tombstoned`, suspended writes or a closed writer.
    pub fn unlink_pull_request(
        &self,
        thread_id: &str,
        expected_revision: u64,
        at_ns: i64,
    ) -> Result<UnlinkReceipt, StorageError> {
        let thread_id = thread_id.to_owned();
        self.pull_request_write(move |conn| {
            repo::unlink(conn, &thread_id, expected_revision, at_ns)
        })
    }

    /// The lifecycle of a thread's pull request in one read: revision, active link, local
    /// HEAD (only while it belongs to the active link) and inheritance.
    ///
    /// # Errors
    /// SQLite failure or a closed writer.
    pub fn pull_request_thread_state(
        &self,
        thread_id: &str,
    ) -> Result<ThreadPullRequestState, StorageError> {
        let thread_id = thread_id.to_owned();
        self.pull_request_read(move |conn| repo::thread_state(conn, &thread_id))
    }

    /// The links of a thread, newest first (the unlink and relink history), at most
    /// `limit` (200 at most).
    ///
    /// # Errors
    /// SQLite failure or a closed writer.
    pub fn pull_request_link_history(
        &self,
        thread_id: &str,
        limit: usize,
    ) -> Result<Vec<PullRequestLink>, StorageError> {
        let thread_id = thread_id.to_owned();
        self.pull_request_read(move |conn| repo::link_history(conn, &thread_id, limit))
    }

    /// Active links of live, unarchived threads in link ID order after `after_link_id`
    /// (0 for the first page), at most `limit` (200 at most). The polling set.
    ///
    /// # Errors
    /// SQLite failure or a closed writer.
    pub fn active_pull_request_links(
        &self,
        after_link_id: u64,
        limit: usize,
    ) -> Result<Vec<PullRequestLink>, StorageError> {
        self.pull_request_read(move |conn| repo::active_links(conn, after_link_id, limit))
    }

    /// Records the thread's local checkout HEAD for the link revision it was observed
    /// under. `Ok(false)` when an observation made later is already stored.
    ///
    /// # Errors
    /// `PullRequest(NotLinked | LinkNotActive)` when the link revision is no longer the
    /// active one, `PullRequest(Invalid)`, `Tombstoned`, suspended writes or a closed writer.
    pub fn record_pull_request_local_head(
        &self,
        thread_id: &str,
        link_revision: u64,
        head_sha: &str,
        observed_at_ns: i64,
    ) -> Result<bool, StorageError> {
        let (thread_id, head_sha) = (thread_id.to_owned(), head_sha.to_owned());
        self.pull_request_write(move |conn| {
            repo::record_local_head(conn, &thread_id, link_revision, &head_sha, observed_at_ns)
        })
    }

    /// Forgets the thread's local checkout HEAD for the link revision it was observed under:
    /// the checkout can no longer be named, and an old commit must not stand for it.
    /// `Ok(false)` when there was nothing stored for that revision.
    ///
    /// # Errors
    /// `PullRequest(NotLinked | LinkNotActive)` when the link revision is no longer the
    /// active one, `Tombstoned`, suspended writes or a closed writer.
    pub fn clear_pull_request_local_head(
        &self,
        thread_id: &str,
        link_revision: u64,
    ) -> Result<bool, StorageError> {
        let thread_id = thread_id.to_owned();
        self.pull_request_write(move |conn| repo::clear_local_head(conn, &thread_id, link_revision))
    }

    /// Hands out the next transition sequence of the thread for the active link
    /// `link_revision`. The runtime numbers a state transition (checks failed, merged, …)
    /// with it before it notifies.
    ///
    /// # Errors
    /// `PullRequest(NotLinked | LinkNotActive)`, `Tombstoned`, suspended writes or a
    /// closed writer.
    pub fn next_pull_request_transition(
        &self,
        thread_id: &str,
        link_revision: u64,
    ) -> Result<u64, StorageError> {
        let thread_id = thread_id.to_owned();
        self.pull_request_write(move |conn| repo::next_transition(conn, &thread_id, link_revision))
    }

    /// Claims the right to send a notification: `true` the first time `key` (thread, link
    /// revision, transition sequence, kind) is claimed, `false` for every repeat. A key
    /// of an ended link is refused, so nothing is sent after an unlink.
    ///
    /// # Errors
    /// `PullRequest(NotLinked | LinkNotActive | Invalid)`, `Tombstoned`, suspended writes
    /// or a closed writer.
    pub fn claim_pull_request_notification(
        &self,
        thread_id: &str,
        key: NotificationKey,
    ) -> Result<bool, StorageError> {
        let thread_id = thread_id.to_owned();
        self.pull_request_write(move |conn| repo::claim_notification(conn, &thread_id, &key))
    }

    /// Whether `key` was already claimed (or is older than the oldest key kept exactly).
    ///
    /// # Errors
    /// `PullRequest(Invalid)`, SQLite failure or a closed writer.
    pub fn pull_request_notification_claimed(
        &self,
        thread_id: &str,
        key: NotificationKey,
    ) -> Result<bool, StorageError> {
        let thread_id = thread_id.to_owned();
        self.pull_request_read(move |conn| repo::notification_claimed(conn, &thread_id, &key))
    }

    /// Presents a pull request a tool output suggested. Idempotent per (thread, pull
    /// request): an existing candidate keeps its state, so a dismissed or confirmed one is
    /// never offered again. A thread holds at most [`crate::pull_requests::PULL_REQUEST_CANDIDATES_MAX`] candidates
    /// of any state; a full thread refuses new ones and drops nothing.
    ///
    /// # Errors
    /// `PullRequest(Invalid | CandidateLimit)`, `Tombstoned` (thread or source run),
    /// suspended writes or a closed writer.
    pub fn present_pull_request_candidate(
        &self,
        new: NewPullRequestCandidate,
    ) -> Result<CandidatePresented, StorageError> {
        self.pull_request_write(move |conn| repo::present_candidate(conn, &new))
    }

    /// Dismisses a presented candidate. A dismissed or confirmed one is returned as it is.
    ///
    /// # Errors
    /// `PullRequest(CandidateNotFound)`, `Tombstoned`, suspended writes or a closed writer.
    pub fn dismiss_pull_request_candidate(
        &self,
        thread_id: &str,
        candidate_id: u64,
        at_ns: i64,
    ) -> Result<PullRequestCandidate, StorageError> {
        let thread_id = thread_id.to_owned();
        self.pull_request_write(move |conn| {
            repo::dismiss_candidate(conn, &thread_id, candidate_id, at_ns)
        })
    }

    /// The thread's candidates, oldest first, optionally of one state.
    ///
    /// # Errors
    /// SQLite failure or a closed writer.
    pub fn pull_request_candidates(
        &self,
        thread_id: &str,
        state: Option<CandidateState>,
    ) -> Result<Vec<PullRequestCandidate>, StorageError> {
        let thread_id = thread_id.to_owned();
        self.pull_request_read(move |conn| repo::candidates(conn, &thread_id, state))
    }

    /// Hands out the sequence of a fetch that is starting. Call it BEFORE the fetch runs
    /// and pass the value to [`Self::commit_pull_request_fetch`] or
    /// [`Self::fail_pull_request_fetch`].
    ///
    /// # Errors
    /// `PullRequest(Invalid)`, suspended writes or a closed writer.
    pub fn begin_pull_request_fetch(&self, key: &PullRequestKey) -> Result<u64, StorageError> {
        let key = key.clone();
        self.pull_request_write(move |conn| repo::snapshots::begin_fetch(conn, &key))
    }

    /// Commits a successful fetch. The facts are stored only when `seq` is newer than the
    /// stored success; the attempt metadata only when it is newer than the last attempt.
    /// Replaying a result stores nothing twice.
    ///
    /// # Errors
    /// `PullRequest(Invalid)` for malformed facts or a sequence that was never handed
    /// out, suspended writes or a closed writer.
    pub fn commit_pull_request_fetch(
        &self,
        key: &PullRequestKey,
        seq: u64,
        facts: PullRequestFacts,
        at_ns: i64,
    ) -> Result<FetchCommit, StorageError> {
        let key = key.clone();
        self.pull_request_write(move |conn| {
            repo::snapshots::commit_success(conn, &key, seq, &facts, at_ns)
        })
    }

    /// Records a failed fetch. It never erases the last success; it is ordered like the
    /// attempt metadata of a success.
    ///
    /// # Errors
    /// `PullRequest(Invalid)` for a sequence that was never handed out, suspended writes
    /// or a closed writer.
    pub fn fail_pull_request_fetch(
        &self,
        key: &PullRequestKey,
        seq: u64,
        failure: FetchFailure,
        at_ns: i64,
    ) -> Result<FetchCommit, StorageError> {
        let key = key.clone();
        self.pull_request_write(move |conn| {
            repo::snapshots::commit_failure(conn, &key, seq, &failure, at_ns)
        })
    }

    /// The shared snapshot of a pull request, if any fetch was ever started for it.
    ///
    /// # Errors
    /// `PullRequest(Invalid)`, SQLite failure or a closed writer.
    pub fn pull_request_snapshot(
        &self,
        key: &PullRequestKey,
    ) -> Result<Option<PullRequestSnapshot>, StorageError> {
        let key = key.clone();
        self.pull_request_read(move |conn| repo::snapshots::get(conn, &key))
    }

    /// Schedules the inheritance of a parent's active link by a derived thread. The link
    /// is pinned; the child's current lifecycle revision is recorded. Idempotent: an
    /// existing row (whatever its state) is returned unchanged.
    ///
    /// # Errors
    /// `PullRequest(ParentLinkNotFound | ParentLinkNotActive | LaneThread | SameThread)`,
    /// `Tombstoned`, suspended writes or a closed writer.
    pub fn schedule_pull_request_inheritance(
        &self,
        new: NewPullRequestInheritance,
    ) -> Result<InheritanceRecord, StorageError> {
        self.pull_request_write(move |conn| repo::inheritance::schedule(conn, &new))
    }

    /// Schedules the inheritance of whatever active link `parent_thread_id` has, read in the same
    /// transaction that stores the row: a parent that relinks at the same moment is pinned to its
    /// old link or its new one, never to neither. `Ok(None)` when the parent has no active link.
    /// Idempotent: an existing row of the child is returned unchanged.
    ///
    /// # Errors
    /// `PullRequest(LaneThread | SameThread | Invalid)`, `Tombstoned`, suspended writes or a
    /// closed writer.
    pub fn pin_pull_request_inheritance(
        &self,
        child_thread_id: &str,
        parent_thread_id: &str,
        at_ns: i64,
    ) -> Result<Option<InheritanceRecord>, StorageError> {
        let (child, parent) = (child_thread_id.to_owned(), parent_thread_id.to_owned());
        self.pull_request_write(move |conn| {
            repo::inheritance::schedule_from_parent(conn, &child, &parent, at_ns)
        })
    }

    /// Tries to apply a pending inheritance: it needs the child's revision unchanged, no
    /// active child link and a stored snapshot (succeeded at `required_success_seq` or
    /// later) that shows the pull request open. Merged or closed skips it; unknown state
    /// leaves it pending. A decided row is never revived, so a retry after an unlink
    /// does nothing.
    ///
    /// # Errors
    /// `Tombstoned`, suspended writes or a closed writer.
    pub fn resolve_pull_request_inheritance(
        &self,
        child_thread_id: &str,
        required_success_seq: u64,
        at_ns: i64,
    ) -> Result<InheritanceResolution, StorageError> {
        let child = child_thread_id.to_owned();
        self.pull_request_write(move |conn| {
            repo::inheritance::resolve(conn, &child, required_success_seq, at_ns)
        })
    }

    /// Counts a failed attempt (for example a fetch that failed) on a pending inheritance.
    /// `None` when the thread has no inheritance row.
    ///
    /// # Errors
    /// `Tombstoned`, suspended writes or a closed writer.
    pub fn record_pull_request_inheritance_attempt(
        &self,
        child_thread_id: &str,
        note: &str,
        at_ns: i64,
    ) -> Result<Option<InheritanceRecord>, StorageError> {
        let (child, note) = (child_thread_id.to_owned(), note.to_owned());
        self.pull_request_write(move |conn| {
            repo::inheritance::record_attempt(conn, &child, &note, at_ns)
        })
    }

    /// Skips a pending inheritance for a reason the runtime decided. A decided row is
    /// returned unchanged; `None` when the thread has no inheritance row.
    ///
    /// # Errors
    /// `Tombstoned`, suspended writes or a closed writer.
    pub fn skip_pull_request_inheritance(
        &self,
        child_thread_id: &str,
        reason: InheritanceSkipReason,
        at_ns: i64,
    ) -> Result<Option<InheritanceRecord>, StorageError> {
        let child = child_thread_id.to_owned();
        self.pull_request_write(move |conn| repo::inheritance::skip(conn, &child, reason, at_ns))
    }

    /// A thread's inheritance row (as a child).
    ///
    /// # Errors
    /// SQLite failure or a closed writer.
    pub fn pull_request_inheritance(
        &self,
        child_thread_id: &str,
    ) -> Result<Option<InheritanceRecord>, StorageError> {
        let child = child_thread_id.to_owned();
        self.pull_request_read(move |conn| repo::inheritance::get(conn, &child))
    }

    /// Pending inheritance rows, oldest first, after `after` (the cursor of the last row
    /// of the previous page; `None` for the first page), at most `limit` (200 at most).
    /// The restart retry set: call it again with `InheritanceRecord::cursor` of the last
    /// row until a page comes back shorter than `limit`.
    ///
    /// # Errors
    /// SQLite failure or a closed writer.
    pub fn pending_pull_request_inheritances(
        &self,
        after: Option<InheritanceCursor>,
        limit: usize,
    ) -> Result<Vec<InheritanceRecord>, StorageError> {
        self.pull_request_read(move |conn| repo::inheritance::pending(conn, after, limit))
    }
}
