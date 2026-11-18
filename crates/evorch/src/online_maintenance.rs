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

/// Convert the legacy columns of the events table in the core, as many bounded
/// operations as `request.max_rows` and the work need. The summary (stdout) has
/// the keys the offline command printed; the progress goes to stderr, one line
/// per committed batch.
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
            &|view| eprintln!("{}", before.after(view).line()),
        )
        .await?;
        totals = totals.after(&view);
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
