//! The reindex of the thread search index (ADR 0041 R5, ADR 0045 stage 17d-3):
//! the durable request / lease / step path the GUI uses too, driven as a state
//! machine that never sleeps and never blocks the async runtime on its own.
//!
//! [`submit`] stores the request and elects the owner; each [`Driver::advance`]
//! is one pass of the foreground owner (renew or take the lease, run one index
//! step, read the request). The area runs every pass on a blocking worker and
//! decides between the passes whether to go on, so a cancel, a stop of the host
//! or a disconnect is looked at before every step. A pass that has nothing to do
//! answers how long to wait ([`Advance::Wait`]); the area waits on its own
//! signals, never on a bare sleep. The request is durable: whoever stops
//! driving it leaves it `pending` in the database and the next index owner (the
//! GUI, or the next `reindex_threads` operation) continues the shared work.
//!
//! The wire results are the storage's own (same JSON) behind exhaustive
//! conversions, so a new storage variant cannot be dropped silently.
use std::time::Duration;

use storage::StorageHandle;
use storage::THREAD_INDEX_LEASE_NS;
use storage::thread_index::{
    ExtractionError, IndexBlockedReason, IndexLease, IndexOwner, IndexStepResult, LeaseResult,
    ProvenanceUnknownReason, ReindexRequest, ReindexResult as StorageResult, RequestFailure,
    RequestGapReason, RequestProgress, RequestSubmission, RequestSummary,
};
use workbench_protocol::namespaces::maintenance as wire;

const FIRST_BACKOFF: Duration = Duration::from_millis(25);
const MAX_BACKOFF: Duration = Duration::from_secs(1);
const MIN_WAIT: Duration = Duration::from_millis(1);

/// What `submit` made of the request.
pub(super) enum Submitted {
    /// The request was not stored.
    Failed(RequestFailure),
    /// Another live owner holds the index and the caller did not wait: the
    /// request is accepted and durable, not done.
    Delegated { request_id: String },
    /// This operation drives the request.
    Driving(Box<Driver>),
}

/// How a request ended for its driver.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Finished {
    /// A persisted result, or a failure of the control path.
    Result(StorageResult),
    /// Someone cancelled this request (not a failed result).
    Cancelled,
}

/// One pass of the owner.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Advance {
    Done(Finished),
    /// An index step made progress.
    Progressed,
    /// Nothing to do before this long has passed (or a signal arrives).
    Wait(Duration),
}

/// Stores the request and elects the owner. With `wait` unset and another live
/// owner holding the lease the request is only accepted (`Delegated`).
pub(super) fn submit(
    handle: &StorageHandle,
    request: &ReindexRequest,
    owner: IndexOwner,
    wait: bool,
    now_ns: i64,
) -> Submitted {
    let RequestSubmission {
        request_id,
        lease,
        progress,
    } = match handle.submit_thread_index_request(request, &owner, now_ns, THREAD_INDEX_LEASE_NS) {
        Ok(submission) => submission,
        Err(reason) => return Submitted::Failed(reason),
    };
    let lease = match lease {
        LeaseResult::Acquired(lease) => Some(lease),
        LeaseResult::HeldByOther(_) if !wait && matches!(progress, RequestProgress::Pending(_)) => {
            return Submitted::Delegated { request_id };
        }
        _ => None,
    };
    Submitted::Driving(Box::new(Driver {
        owner,
        request_id,
        progress,
        lease,
        backoff: FIRST_BACKOFF,
    }))
}

/// The foreground owner of one request.
pub(super) struct Driver {
    owner: IndexOwner,
    request_id: String,
    progress: RequestProgress,
    lease: Option<IndexLease>,
    backoff: Duration,
}

impl Driver {
    pub(super) fn request_id(&self) -> &str {
        &self.request_id
    }

    /// How the request's targets stand after the last pass.
    pub(super) fn summary(&self) -> Option<&RequestSummary> {
        match &self.progress {
            RequestProgress::Pending(summary)
            | RequestProgress::Terminal(
                StorageResult::Completed { summary, .. }
                | StorageResult::CompletedWithGaps { summary, .. },
            ) => Some(summary),
            RequestProgress::Terminal(_) | RequestProgress::Cancelled => None,
        }
    }

    /// One pass: renew or take the lease, run one index step, read the request.
    pub(super) fn advance(
        &mut self,
        handle: &StorageHandle,
        clock: &mut impl FnMut() -> i64,
    ) -> Advance {
        match self.pass(handle, clock) {
            Ok(advance) => advance,
            Err(reason) => Advance::Done(Finished::Result(StorageResult::Failed {
                request_id: Some(self.request_id.clone()),
                reason,
            })),
        }
    }

    fn finished(&self) -> Option<Finished> {
        match &self.progress {
            RequestProgress::Terminal(result) => Some(Finished::Result(result.clone())),
            RequestProgress::Cancelled => Some(Finished::Cancelled),
            RequestProgress::Pending(_) => None,
        }
    }

    fn pass(
        &mut self,
        handle: &StorageHandle,
        clock: &mut impl FnMut() -> i64,
    ) -> Result<Advance, RequestFailure> {
        if let Some(finished) = self.finished() {
            return Ok(Advance::Done(finished));
        }
        let now = clock();
        let mut next_due = None;
        if let Some(current) = &self.lease {
            if current.expires_at_ns.saturating_sub(now) <= THREAD_INDEX_LEASE_NS / 2 {
                self.lease = match handle.heartbeat_thread_index_lease(
                    current,
                    now,
                    THREAD_INDEX_LEASE_NS,
                )? {
                    LeaseResult::Acquired(renewed) => Some(renewed),
                    _ => None,
                };
            }
        } else {
            self.lease =
                match handle.acquire_thread_index_lease(&self.owner, now, THREAD_INDEX_LEASE_NS)? {
                    LeaseResult::Acquired(acquired) => Some(acquired),
                    LeaseResult::HeldByOther(other) => {
                        next_due = Some(other.expires_at_ns);
                        None
                    }
                    LeaseResult::Lost => None,
                };
        }
        let mut made_progress = false;
        if let Some(current) = &self.lease {
            match handle.step_thread_index(current, now)? {
                IndexStepResult::Progress => made_progress = true,
                IndexStepResult::Idle { next_wake_at_ns } => next_due = next_wake_at_ns,
                IndexStepResult::Paused(_) => {}
                IndexStepResult::LeaseLost => self.lease = None,
            }
        }
        self.progress = handle
            .thread_index_request_status(&self.request_id, clock())?
            .progress;
        if let Some(finished) = self.finished() {
            return Ok(Advance::Done(finished));
        }
        if made_progress {
            self.backoff = FIRST_BACKOFF;
            return Ok(Advance::Progressed);
        }
        let now = clock();
        let until_due = next_due.map_or(self.backoff, |due| {
            Duration::from_nanos(u64::try_from(due.saturating_sub(now)).unwrap_or(0))
        });
        // Cap polling latency at a second; never spin on an overdue wake, and
        // wake well before our lease needs a heartbeat.
        let wait = self.backoff.min(until_due).max(MIN_WAIT);
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        Ok(Advance::Wait(wait))
    }

    /// Gives the lease back; fenced, so a lost lease cannot release its
    /// successor. A failure cannot rewrite a persisted outcome, so it is only
    /// logged.
    pub(super) fn release(&mut self, handle: &StorageHandle) {
        if let Some(lease) = self.lease.take()
            && let Err(reason) = handle.release_thread_index_lease(&lease)
        {
            tracing::warn!(
                request_id = %self.request_id,
                ?reason,
                "thread index lease release failed"
            );
        }
    }
}

/// The wall clock of the storage's lease and request times.
pub(super) fn clock_ns() -> i64 {
    storage::system_time_to_ns(std::time::SystemTime::now()).unwrap_or(i64::MAX)
}

/// The progress of the operation that drives `driver`.
pub(super) fn progress(driver: &Driver) -> wire::MaintenanceProgress {
    wire::MaintenanceProgress::ReindexThreads {
        request_id: Some(driver.request_id().to_owned()),
        summary: driver.summary().map(summary),
        result: None,
    }
}

// --- storage -> wire -------------------------------------------------------

fn provenance(reason: ProvenanceUnknownReason) -> wire::ProvenanceUnknown {
    match reason {
        ProvenanceUnknownReason::MissingSidecar => wire::ProvenanceUnknown::MissingSidecar,
        ProvenanceUnknownReason::SnapshotMismatch => wire::ProvenanceUnknown::SnapshotMismatch,
        ProvenanceUnknownReason::IncompleteMap => wire::ProvenanceUnknown::IncompleteMap,
        ProvenanceUnknownReason::LegacyUnproven => wire::ProvenanceUnknown::LegacyUnproven,
        ProvenanceUnknownReason::UnsupportedVersion => wire::ProvenanceUnknown::UnsupportedVersion,
    }
}

fn extraction(error: ExtractionError) -> wire::ExtractionFailure {
    match error {
        ExtractionError::InvalidJson => wire::ExtractionFailure::InvalidJson,
        ExtractionError::ProvenanceUnknown(reason) => {
            wire::ExtractionFailure::ProvenanceUnknown(provenance(reason))
        }
        ExtractionError::NoteRunMismatch => wire::ExtractionFailure::NoteRunMismatch,
    }
}

fn blocked(reason: IndexBlockedReason) -> wire::BlockedReason {
    match reason {
        IndexBlockedReason::Extraction(error) => wire::BlockedReason::Extraction(extraction(error)),
        IndexBlockedReason::AttemptsExhausted => wire::BlockedReason::AttemptsExhausted,
    }
}

fn gap_reason(reason: RequestGapReason) -> wire::ReindexGapReason {
    match reason {
        RequestGapReason::Blocked(reason) => wire::ReindexGapReason::Blocked(blocked(reason)),
        RequestGapReason::CapacityPaused => wire::ReindexGapReason::CapacityPaused,
        RequestGapReason::CapacityWaitExpired => wire::ReindexGapReason::CapacityWaitExpired,
        RequestGapReason::IndexDisabled => wire::ReindexGapReason::IndexDisabled,
        RequestGapReason::OversizedBudget => wire::ReindexGapReason::OversizedBudget,
        RequestGapReason::OversizedStep => wire::ReindexGapReason::OversizedStep,
        RequestGapReason::EvictedByBudget => wire::ReindexGapReason::EvictedByBudget,
        RequestGapReason::Deleted => wire::ReindexGapReason::Deleted,
        RequestGapReason::Superseded => wire::ReindexGapReason::Superseded,
        RequestGapReason::VersionSuperseded => wire::ReindexGapReason::VersionSuperseded,
    }
}

pub(super) fn summary(summary: &RequestSummary) -> wire::ReindexSummary {
    wire::ReindexSummary {
        target_count: summary.target_count,
        succeeded: summary.succeeded,
        deleted_before_submission: summary.deleted_before_submission,
        gaps: summary
            .gaps
            .iter()
            .take(wire::MAX_REINDEX_GAPS)
            .map(|gap| wire::ReindexGap {
                reason: gap_reason(gap.reason),
                count: gap.count,
            })
            .collect(),
    }
}

pub(super) fn failure(reason: RequestFailure) -> wire::ReindexFailure {
    match reason {
        RequestFailure::InvalidArguments => wire::ReindexFailure::InvalidArguments,
        RequestFailure::DatabaseUnavailable => wire::ReindexFailure::DatabaseUnavailable,
        RequestFailure::PersistenceUnavailable => wire::ReindexFailure::PersistenceUnavailable,
        RequestFailure::UnrecoverableControlFailure => {
            wire::ReindexFailure::UnrecoverableControlFailure
        }
    }
}

pub(super) fn wire_result(result: &StorageResult) -> wire::ReindexResult {
    match result {
        StorageResult::Accepted { request_id } => wire::ReindexResult::Accepted {
            request_id: request_id.clone(),
        },
        StorageResult::Completed {
            request_id,
            summary: done,
        } => wire::ReindexResult::Completed {
            request_id: request_id.clone(),
            summary: summary(done),
        },
        StorageResult::CompletedWithGaps {
            request_id,
            summary: done,
        } => wire::ReindexResult::CompletedWithGaps {
            request_id: request_id.clone(),
            summary: summary(done),
        },
        StorageResult::Failed { request_id, reason } => wire::ReindexResult::Failed {
            request_id: request_id.clone(),
            reason: failure(*reason),
        },
    }
}

#[cfg(test)]
mod tests;
