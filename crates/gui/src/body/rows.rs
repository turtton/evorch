//! Reading the rows of some runs on the resolver's worker: what a late-known
//! conversation needs, and what reading an unreadable event again needs.

use super::backend::HistoryRow;
use super::parts::{Accepted, DeferredPartAssembler};

/// Why rows are wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowsPurpose {
    /// The earlier events of a conversation that just became known.
    Rehydrate { thread: String },
    /// The unreadable event `event_id` of `run`, read again together with
    /// the rest of its run (a response in slices needs all of them).
    Gap { run: String, event_id: i64 },
}

/// One read of rows, queued on the resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowsRequest {
    pub id: u64,
    pub purpose: RowsPurpose,
    pub runs: Vec<String>,
    pub after: i64,
    pub through: i64,
    /// Automatic retries scheduled so far, the one being waited for included.
    pub retries: u8,
}

/// The rows of a request, or why they could not be read.
#[derive(Debug, Clone, PartialEq)]
pub struct RowsOutcome {
    pub request: RowsRequest,
    pub harness_notices: Vec<storage::AdoptedHarnessNotice>,
    pub result: Result<Vec<HistoryRow>, super::types::BodyError>,
}

/// Joins the slices of a response into one completion, in place, the way a
/// replay does: a failure ends the run's open response, and a response that
/// never completes leaves nothing. Done on the worker, so the app only applies.
#[must_use]
pub fn assemble(rows: Vec<HistoryRow>) -> Vec<HistoryRow> {
    let mut parts = DeferredPartAssembler::new();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        match row {
            HistoryRow::Failure(failure) => {
                parts.interrupt(failure.run_id.as_deref());
                out.push(HistoryRow::Failure(failure));
            }
            HistoryRow::Event(event) => {
                let replacement = match parts.accept(&event) {
                    Accepted::Event(_) => None,
                    Accepted::Completed(done) => Some(Some(*done)),
                    Accepted::Nothing => Some(None),
                };
                match replacement {
                    None => out.push(HistoryRow::Event(event)),
                    Some(Some(done)) => out.push(HistoryRow::Event(done)),
                    Some(None) => {}
                }
            }
        }
    }
    out
}
