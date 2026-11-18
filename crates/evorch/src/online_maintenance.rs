//! The online maintenance tasks of the core as a CLI sees them (ADR 0045,
//! stage 17d): start one task, follow its operation until it settles and report
//! its progress. `evorch maintenance backfill-events` is the first user; the
//! next one (`reindex-threads`, stage 17d-3) adds a [`MaintenanceTask`] and a
//! function like [`backfill`] over [`run_task`], and nothing else here.
//!
//! The core owns the database and runs the task in bounded batches through its
//! one writer. The CLI only asks, follows and prints; it never opens the
//! database, and without a core it does nothing (see
//! [`crate::cli::core_client`]). The operation belongs to the connection that
//! started it: when this process is interrupted or killed, or its connection is
//! lost, the core stops that operation at its next batch boundary (ADR 0045
//! §10) and no other one.
//!
//! Nothing here waits for time: [`run_task`] waits for the events the core
//! publishes for the operation.

use std::cell::Cell;
use std::path::PathBuf;

use serde_json::json;
use storage::EventBackfillSummary;
use workbench_client::WorkbenchClient;
use workbench_protocol::{
    commands::{Command as CoreCommand, CommandResult},
    events::{SubscriptionScope, UiEvent},
    handshake::Capability,
    ids::OperationId,
    namespaces::{
        diagnostics::OperationTarget,
        maintenance::{
            BackfillEvents, MAX_ROWS_LIMIT, MaintenanceCommand, MaintenanceEvent,
            MaintenanceProgress, MaintenanceQuery, MaintenanceReply, MaintenanceResult,
            MaintenanceTask, MaintenanceView, StartMaintenance,
        },
    },
    operation::{CancelReason, OperationState},
    queries::{Query, QueryReply},
};

use crate::cli::core_client::{self, CliError, Rendered, Step};

/// Whether `event` may change a maintenance operation.
pub fn is_maintenance_event(event: &UiEvent) -> bool {
    matches!(
        event,
        UiEvent::Maintenance(
            MaintenanceEvent::MaintenanceProgress { .. }
                | MaintenanceEvent::MaintenanceSettled { .. }
        )
    )
}

/// Start `task` as the operation `id` and follow it until it settles. `progress`
/// sees the view after each batch the core committed (once per batch, in
/// order, without the batches the core coalesced); the settled view is
/// returned whatever its state is.
///
/// # Errors
/// The core refused the task (`busy` while another operation of the task runs,
/// or an invalid request), the connection was lost (the outcome is then
/// unknown: the core stops the operation, and the command is not sent again),
/// or the core answered with something else.
pub async fn run_task(
    client: &WorkbenchClient,
    id: &OperationId,
    task: MaintenanceTask,
    progress: &dyn Fn(&MaintenanceView),
) -> Result<MaintenanceView, CliError> {
    let reply = core_client::send(
        client,
        CoreCommand::Maintenance(MaintenanceCommand::StartMaintenance(StartMaintenance {
            operation_id: id.clone(),
            task,
        })),
    )
    .await?;
    match reply.result.as_deref() {
        Some(CommandResult::Maintenance(MaintenanceResult::Operation(_))) => {}
        _ => {
            return Err(CliError::Failed(
                "protocol violation: the core did not answer with an operation".into(),
            ));
        }
    }
    let printed = Cell::new(0_u32);
    core_client::wait_settled(client, SubscriptionScope::All, is_maintenance_event, || {
        let (printed, id) = (&printed, id.clone());
        async move {
            let query = Query::Maintenance(MaintenanceQuery::GetMaintenance(OperationTarget {
                operation_id: id,
            }));
            // The core no longer holds the operation: it restarted, so whether
            // the task finished is not known.
            let Some(view) = core_client::read_operation(client, query, |reply| match reply {
                QueryReply::Maintenance(MaintenanceReply::Operation(view)) => Some(*view),
                _ => None,
            })
            .await?
            else {
                return Ok(Step::Unknown);
            };
            if view.batches > printed.get() {
                printed.set(view.batches);
                progress(&view);
            }
            Ok(if view.operation.state.is_final() {
                Step::Settled(view)
            } else {
                Step::Pending
            })
        }
    })
    .await
}

/// What a task that did not end `ready` tells.
fn stopped(task: &str, view: &MaintenanceView) -> String {
    view.message
        .clone()
        .unwrap_or_else(|| match view.operation.state {
            OperationState::Cancelled {
                reason: CancelReason::Shutdown,
            } => format!("{task} was stopped by the shutdown of the core"),
            OperationState::Cancelled { .. } => format!("{task} was cancelled"),
            _ => format!("{task} failed"),
        })
}

// ---- backfill-events ---------------------------------------------------------

/// `evorch maintenance backfill-events`, as typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillRequest {
    pub user_config_dir: Option<PathBuf>,
    /// Rows per batch (one writer transaction); the core's default when `None`.
    pub batch_rows: Option<u32>,
    /// Stop after about this many rows in all; every row when `None`.
    pub max_rows: Option<u64>,
}

/// The counters of the operations so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Totals {
    converted: u64,
    undecodable: u64,
    batches: u64,
    reattributed: u64,
    /// Rows still unconverted or unattributed after the last batch.
    remaining: u64,
}

impl Totals {
    /// `self` after the operation `view` described.
    fn after(self, view: &MaintenanceView) -> Self {
        let MaintenanceProgress::BackfillEvents {
            converted,
            reattributed,
            undecodable,
            remaining_unconverted,
            remaining_unattributed,
        } = &view.progress;
        Self {
            converted: self.converted + converted,
            undecodable: self.undecodable + undecodable,
            batches: self.batches + u64::from(view.batches),
            reattributed: self.reattributed + reattributed,
            // Absent before the first batch: nothing was converted, so the
            // earlier count still stands.
            remaining: match (remaining_unconverted, remaining_unattributed) {
                (None, None) => self.remaining,
                (unconverted, unattributed) => unconverted.unwrap_or(0) + unattributed.unwrap_or(0),
            },
        }
    }

    fn handled(self) -> u64 {
        self.converted + self.reattributed
    }

    fn summary(self) -> EventBackfillSummary {
        EventBackfillSummary {
            converted: self.converted,
            undecodable: self.undecodable,
            batches: self.batches,
            reattributed: self.reattributed,
            remaining: self.remaining,
        }
    }

    fn line(self) -> String {
        format!(
            "backfill-events: batch {}: converted {}, reattributed {}, undecodable {}, remaining {}",
            self.batches, self.converted, self.reattributed, self.undecodable, self.remaining
        )
    }
}

/// How many of the first batches get a line of their own.
const EVERY_BATCH_UP_TO: u64 = 16;

/// The progress lines of a backfill, within a budget that does not grow with
/// the number of batches: one line for each of the first
/// [`EVERY_BATCH_UP_TO`] batches, then one each time the batch count has
/// doubled (so about `EVERY_BATCH_UP_TO` + log2(batches) lines whatever the
/// batch size, 16 + 64 at most), and the last state always. A line states the
/// totals so far, so the batches between two lines are not lost, only not
/// announced.
#[derive(Debug, Default)]
struct ProgressLines {
    /// The batch count the next line waits for.
    next: Cell<u64>,
    /// The batch count of the last line.
    printed: Cell<u64>,
}

impl ProgressLines {
    /// The line for `totals`, unless the budget skips it.
    fn offer(&self, totals: Totals) -> Option<String> {
        if totals.batches < self.next.get().max(1) {
            return None;
        }
        self.printed.set(totals.batches);
        self.next.set(if totals.batches < EVERY_BATCH_UP_TO {
            totals.batches + 1
        } else {
            totals.batches.saturating_mul(2)
        });
        Some(totals.line())
    }

    /// The line for the last state, unless it was the last one printed.
    fn last(&self, totals: Totals) -> Option<String> {
        (totals.batches > self.printed.get()).then(|| {
            self.printed.set(totals.batches);
            totals.line()
        })
    }
}

/// Convert the legacy columns of the events table in the core, as many bounded
/// operations as `request.max_rows` and the work need. The summary (stdout) has
/// the keys the offline command printed; the progress goes to stderr within a
/// fixed budget of lines (see [`ProgressLines`]), the last state included.
///
/// The task is one operation at a time in the core: another client's running
/// backfill makes this one fail as `busy`.
///
/// # Errors
/// As [`run_task`]. An operation that ends failed or cancelled is the `Err` of
/// the rendered text, with what the batches before it did.
pub async fn backfill(
    client: &WorkbenchClient,
    request: &BackfillRequest,
) -> Result<Rendered, CliError> {
    let lines = ProgressLines::default();
    let mut totals = Totals::default();
    let mut budget = request.max_rows;
    loop {
        let rows = budget.map_or(MAX_ROWS_LIMIT, |left| left.min(MAX_ROWS_LIMIT));
        let id = core_client::operation_id("cli-backfill")?;
        let before = totals;
        let view = run_task(
            client,
            &id,
            MaintenanceTask::BackfillEvents(BackfillEvents {
                batch_rows: request.batch_rows,
                max_rows: Some(rows),
            }),
            &|view| {
                if let Some(line) = lines.offer(before.after(view)) {
                    eprintln!("{line}");
                }
            },
        )
        .await;
        // A failed or lost operation still leaves the state it reached.
        let view = match view {
            Ok(view) => view,
            Err(error) => {
                if let Some(line) = lines.last(totals) {
                    eprintln!("{line}");
                }
                return Err(error);
            }
        };
        totals = totals.after(&view);
        if let Some(line) = lines.last(totals) {
            eprintln!("{line}");
        }
        if view.operation.state != OperationState::Ready {
            return Ok(Err(format!(
                "{}\nbefore it ended: {}",
                stopped("backfill-events", &view),
                json!(totals.summary())
            )));
        }
        let handled = totals.handled() - before.handled();
        budget = budget.map(|left| left.saturating_sub(handled));
        // Done, out of rows, or an operation that found nothing to do.
        if totals.remaining == 0 || handled == 0 || budget == Some(0) {
            break;
        }
    }
    serde_json::to_string_pretty(&json!(totals.summary()))
        .map(Ok)
        .map_err(|error| CliError::Failed(error.to_string()))
}

/// Run the backfill in the core of the request's configuration.
///
/// # Errors
/// As [`backfill`], and no core could be reached or started.
pub async fn execute(request: BackfillRequest) -> Result<Rendered, CliError> {
    let connected = core_client::connect(
        request.user_config_dir.clone(),
        &[Capability::SnapshotEvents],
    )
    .await?;
    backfill(&connected.client, &request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(batches: u64) -> Totals {
        Totals {
            batches,
            converted: batches,
            ..Totals::default()
        }
    }

    /// The lines of a run that commits `batches` batches, one at a time, and
    /// ends there.
    fn lines_of(batches: u64) -> Vec<String> {
        let lines = ProgressLines::default();
        let mut out: Vec<String> = (1..=batches).filter_map(|n| lines.offer(at(n))).collect();
        out.extend(lines.last(at(batches)));
        out
    }

    #[test]
    fn a_few_batches_are_each_announced() {
        // Given/When: a run of three batches.
        let out = lines_of(3);

        // Then: every batch has its line, in order, and nothing is repeated.
        let batches: Vec<_> = out.iter().map(|line| &line[..28]).collect();
        assert_eq!(
            batches,
            [
                "backfill-events: batch 1: co",
                "backfill-events: batch 2: co",
                "backfill-events: batch 3: co"
            ]
        );
    }

    #[test]
    fn the_lines_stay_within_a_budget_however_many_batches_there_are() {
        // Given/When: runs of growing length, up to a million batches.
        for batches in [100, 5_000, 1_000_000] {
            let out = lines_of(batches);

            // Then: the lines do not grow with the batches (16 first ones and
            // one per doubling), and the last state is always the last line.
            assert!(
                out.len() <= 16 + 64 + 1,
                "{batches} batches gave {} lines",
                out.len()
            );
            assert_eq!(out.last().unwrap(), &at(batches).line());
        }
        assert!(lines_of(1_000_000).len() < lines_of(5_000).len() + 10);
    }

    #[test]
    fn batches_the_core_coalesced_still_print_the_last_state_once() {
        // Given: progress observed only at some batch counts (the core joins
        // the events of batches that finish together).
        let lines = ProgressLines::default();
        let seen = [1, 2, 7, 40, 41, 900];

        // When: each is offered and the run ends at 905.
        let mut out: Vec<_> = seen.iter().filter_map(|n| lines.offer(at(*n))).collect();
        out.extend(lines.last(at(905)));

        // Then: the budget decides which are printed, and the end is printed once.
        assert_eq!(out.last().unwrap(), &at(905).line());
        assert_eq!(lines.last(at(905)), None);
        assert!(out.len() <= seen.len() + 1);
    }

    #[test]
    fn a_run_without_a_batch_prints_nothing() {
        // Given/When: nothing to convert.
        let lines = ProgressLines::default();

        // Then: there is no line, as before.
        assert_eq!(lines.last(Totals::default()), None);
    }
}
