//! Online maintenance through the host's command entry (ADR 0045, stage 17d), the
//! way a desktop and a web client reach it.
//!
//! The area runs a task as an operation of bounded batches on the storage writer
//! the host already owns. These tests hold an operation at the two points a
//! restart, a stop or a cancel can matter (its start, and between two batches)
//! with a gate the test opens, so no test waits for time to pass: every wait is
//! for an event the host or the gate sends.
#[path = "memory_reads/support.rs"]
mod support;

mod common;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use support::{World, operation, world};
use workbench_host::areas::Areas;
use workbench_host::areas::maintenance::{
    MaintenanceArea, MaintenanceHook, MaintenanceSeams, MaintenanceStage,
};
use workbench_host::bootstrap::restart::HostRestartPhase;
use workbench_host::connections::ConnectionHooks;
use workbench_host::provenance::local_desktop;
use workbench_protocol::WorkbenchError;
use workbench_protocol::commands::{Command, CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::envelope::{Provenance, Surface};
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
use workbench_protocol::ids::ClientId;
use workbench_protocol::namespaces::diagnostics::OperationTarget;
use workbench_protocol::namespaces::maintenance::{
    BackfillEvents, CancelMaintenance, ListMaintenance, MaintenanceCommand, MaintenanceEffect,
    MaintenanceEvent, MaintenanceOutcome, MaintenanceProgress, MaintenanceQuery, MaintenanceReply,
    MaintenanceResult, MaintenanceTask, MaintenanceView, StartMaintenance,
};
use workbench_protocol::operation::{CancelReason, OperationState};
use workbench_protocol::queries::{Query, QueryReply};

/// A gate the test opens: every operation that reaches `stage` having committed
/// `batches` batches counts itself in `reached` and waits until `release`.
struct Hold {
    stage: MaintenanceStage,
    batches: u32,
    reached: tokio::sync::Semaphore,
    open: tokio::sync::watch::Sender<bool>,
}

impl Hold {
    fn new(stage: MaintenanceStage, batches: u32) -> Arc<Self> {
        Arc::new(Self {
            stage,
            batches,
            reached: tokio::sync::Semaphore::new(0),
            open: tokio::sync::watch::channel(false).0,
        })
    }

    fn hook(self: &Arc<Self>) -> MaintenanceHook {
        let hold = self.clone();
        Arc::new(move |stage, _, batches| {
            let hold = hold.clone();
            Box::pin(async move {
                if stage == hold.stage && batches == hold.batches {
                    hold.reached.add_permits(1);
                    let _ = hold.open.subscribe().wait_for(|open| *open).await;
                }
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        })
    }

    async fn reached(&self) {
        self.reached.acquire().await.unwrap().forget();
    }

    fn release(&self) {
        self.open.send_replace(true);
    }
}

fn setup(hook: Option<MaintenanceHook>) -> World {
    setup_with_connections(hook, &Arc::new(ConnectionHooks::default()))
}

/// A host whose maintenance area stops a leaving client's operations through
/// `connections`, the way the bootstrapped core wires it.
fn setup_with_connections(
    hook: Option<MaintenanceHook>,
    connections: &Arc<ConnectionHooks>,
) -> World {
    world(|parts| {
        let maintenance = MaintenanceArea::from_parts(
            parts.tasks.clone(),
            parts.projection.source("maintenance").unwrap(),
            parts.storage.clone(),
            MaintenanceSeams { hook },
        );
        maintenance.stop_on_disconnect(connections);
        Areas {
            maintenance,
            ..Default::default()
        }
    })
}

fn web() -> Provenance {
    common::web()
}

fn db_path(w: &World) -> PathBuf {
    w.dir.path().join("host.db")
}

/// Legacy events: written before the `subkind` / `run_id` columns existed.
fn seed_legacy(w: &World, rows: u32) {
    let conn = rusqlite::Connection::open(db_path(w)).unwrap();
    for n in 0..rows {
        conn.execute(
            "INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
             VALUES (1, ?1, ?1, 'Message', '{}')",
            [i64::from(n) + 1],
        )
        .unwrap();
    }
}

/// Events the backfill has not converted yet, read directly from the file.
fn unconverted(w: &World) -> i64 {
    rusqlite::Connection::open(db_path(w))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE subkind IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn backfill(batch_rows: u32, max_rows: Option<u64>) -> MaintenanceTask {
    MaintenanceTask::BackfillEvents(BackfillEvents {
        batch_rows: Some(batch_rows),
        max_rows,
    })
}

fn start(
    w: &mut World,
    who: Provenance,
    id: &str,
    task: MaintenanceTask,
) -> Result<CommandReply, WorkbenchError> {
    w.host.handle_command(
        Command::Maintenance(MaintenanceCommand::StartMaintenance(StartMaintenance {
            operation_id: operation(id),
            task,
        })),
        who,
    )
}

fn cancel(w: &mut World, who: Provenance, id: &str) -> Result<CommandReply, WorkbenchError> {
    w.host.handle_command(
        Command::Maintenance(MaintenanceCommand::CancelMaintenance(CancelMaintenance {
            operation_id: operation(id),
        })),
        who,
    )
}

fn view_of(reply: &CommandReply) -> MaintenanceView {
    match reply.result.as_deref() {
        Some(CommandResult::Maintenance(MaintenanceResult::Operation(view))) => (**view).clone(),
        other => panic!("not a maintenance operation: {other:?}"),
    }
}

fn get(w: &World, id: &str) -> Result<MaintenanceView, WorkbenchError> {
    match w.query(Query::Maintenance(MaintenanceQuery::GetMaintenance(
        OperationTarget {
            operation_id: operation(id),
        },
    )))? {
        QueryReply::Maintenance(MaintenanceReply::Operation(view)) => Ok(*view),
        other => panic!("{other:?}"),
    }
}

fn list(w: &World) -> Vec<MaintenanceView> {
    match w
        .query(Query::Maintenance(MaintenanceQuery::ListMaintenance(
            ListMaintenance::default(),
        )))
        .unwrap()
    {
        QueryReply::Maintenance(MaintenanceReply::List(page)) => page.items,
        other => panic!("{other:?}"),
    }
}

fn subscribe(w: &World) -> workbench_host::subscriptions::Subscription {
    w.host
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap()
}

/// Every maintenance event of `id` up to and including its settled event.
fn events_until_settled(
    rt: &tokio::runtime::Runtime,
    events: &mut workbench_host::subscriptions::Subscription,
    id: &str,
) -> Vec<MaintenanceEvent> {
    rt.block_on(async {
        let mut seen = Vec::new();
        loop {
            let UiEvent::Maintenance(event) =
                events.recv().await.expect("the subscription is open").event
            else {
                continue;
            };
            let (MaintenanceEvent::MaintenanceProgress { view }
            | MaintenanceEvent::MaintenanceSettled { view }) = &event;
            if view.operation.operation_id.as_str() != id {
                continue;
            }
            let settled = matches!(event, MaintenanceEvent::MaintenanceSettled { .. });
            seen.push(event);
            if settled {
                return seen;
            }
        }
    })
}

fn progress_views(events: &[MaintenanceEvent]) -> Vec<&MaintenanceView> {
    events
        .iter()
        .filter_map(|event| match event {
            MaintenanceEvent::MaintenanceProgress { view } => Some(&**view),
            MaintenanceEvent::MaintenanceSettled { .. } => None,
        })
        .collect()
}

fn settled_view(events: &[MaintenanceEvent]) -> &MaintenanceView {
    match events.last() {
        Some(MaintenanceEvent::MaintenanceSettled { view }) => view,
        other => panic!("not settled: {other:?}"),
    }
}

fn converted_of(view: &MaintenanceView) -> u64 {
    let MaintenanceProgress::BackfillEvents { converted, .. } = &view.progress;
    *converted
}

#[test]
fn a_backfill_runs_in_bounded_batches_and_announces_each_committed_batch() {
    let mut w = setup(None);
    seed_legacy(&w, 10);
    let mut events = subscribe(&w);

    let reply = start(&mut w, web(), "fill", backfill(3, None)).unwrap();
    assert_eq!(reply.disposition, CommandDisposition::Accepted);
    // The handler answers at once: the work is not done inside it.
    assert!(!view_of(&reply).operation.state.is_final());

    let seen = events_until_settled(&w.rt, &mut events, "fill");
    let progress = progress_views(&seen);
    assert_eq!(
        progress
            .iter()
            .map(|view| (view.batches, converted_of(view)))
            .collect::<Vec<_>>(),
        [(1, 3), (2, 6), (3, 9), (4, 10)]
    );
    assert!(progress.iter().all(|view| {
        view.operation.state == OperationState::Pending && view.effect == MaintenanceEffect::Partial
    }));
    let done = settled_view(&seen);
    assert_eq!(done.operation.state, OperationState::Ready);
    assert_eq!(done.effect, MaintenanceEffect::Complete);
    assert_eq!(done.outcome, Some(MaintenanceOutcome::Completed));
    assert_eq!((done.batches, converted_of(done)), (4, 10));
    assert_eq!(&get(&w, "fill").unwrap(), done);
    assert_eq!(unconverted(&w), 0);
    assert!(!w.tasks.busy());
}

#[test]
fn a_backfill_adds_the_undecodable_rows_of_both_of_its_targets() {
    // Given: a row the first target converts, one it cannot decode (no `subkind`
    // and a payload that is not JSON), and an Orchestrator event the second
    // target attributes whose payload no longer decodes.
    let mut w = setup(None);
    let conn = rusqlite::Connection::open(db_path(&w)).unwrap();
    conn.execute_batch(
        "INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
         VALUES (1, 1, 1, 'Message', '{\"kind\":\"Message\",\"payload\":{\"kind\":\"MessageCompleted\",\"payload\":{\"run_id\":\"run-c\",\"text\":\"t\"}}}');
         INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
         VALUES (1, 2, 2, 'Message', '{');
         INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind) \
         VALUES (1, 3, 3, 'Orchestrator', '{', 'TaskProgressed');",
    )
    .unwrap();
    drop(conn);
    let mut events = subscribe(&w);

    // When: the backfill runs one row at a time.
    start(&mut w, web(), "both", backfill(1, None)).unwrap();
    let seen = events_until_settled(&w.rt, &mut events, "both");

    // Then: the operation counts the undecodable rows of both targets.
    let done = settled_view(&seen);
    assert_eq!(done.operation.state, OperationState::Ready);
    let MaintenanceProgress::BackfillEvents {
        converted,
        reattributed,
        undecodable,
        remaining_unconverted,
        remaining_unattributed,
    } = &done.progress;
    assert_eq!(
        (
            *converted,
            *reattributed,
            *undecodable,
            *remaining_unconverted,
            *remaining_unattributed
        ),
        (2, 1, 2, Some(0), Some(0))
    );
    assert_eq!(&get(&w, "both").unwrap(), done);
}

#[test]
fn a_backfill_stops_at_its_row_bound_and_says_what_is_left() {
    let mut w = setup(None);
    seed_legacy(&w, 10);
    let mut events = subscribe(&w);
    start(&mut w, web(), "bounded", backfill(2, Some(5))).unwrap();
    let seen = events_until_settled(&w.rt, &mut events, "bounded");
    let done = settled_view(&seen);
    assert_eq!(done.operation.state, OperationState::Ready);
    assert_eq!((done.batches, converted_of(done)), (3, 5));
    let MaintenanceProgress::BackfillEvents {
        remaining_unconverted,
        ..
    } = &done.progress;
    assert_eq!(*remaining_unconverted, Some(5));
    assert_eq!(unconverted(&w), 5);
}

#[test]
fn a_cancel_stops_at_the_next_batch_boundary_and_keeps_the_committed_batches() {
    let hold = Hold::new(MaintenanceStage::BeforeBatch, 1);
    let mut w = setup(Some(hold.hook()));
    seed_legacy(&w, 10);
    let mut events = subscribe(&w);
    start(&mut w, web(), "stop-me", backfill(3, None)).unwrap();
    w.rt.block_on(hold.reached());

    let asked = cancel(&mut w, local_desktop(), "stop-me").unwrap();
    assert!(!view_of(&asked).operation.state.is_final());
    hold.release();

    let seen = events_until_settled(&w.rt, &mut events, "stop-me");
    let done = settled_view(&seen);
    assert_eq!(
        done.operation.state,
        OperationState::Cancelled {
            reason: CancelReason::Requested
        }
    );
    // One batch is committed and told apart from a complete result.
    assert_eq!(done.effect, MaintenanceEffect::Partial);
    assert_eq!((done.batches, converted_of(done)), (1, 3));
    assert_eq!(done.outcome, None);
    assert_eq!(unconverted(&w), 7);

    // Cancelling an operation that ended answers its final view.
    let again = cancel(&mut w, web(), "stop-me").unwrap();
    assert_eq!(again.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(&view_of(&again), done);
    let unknown = cancel(&mut w, web(), "never-sent").unwrap_err();
    assert_eq!(unknown.code, ErrorCode::NotFound);
}

#[test]
fn a_batch_the_writer_fails_ends_the_operation_unknown_without_leaking_the_cause() {
    let hold = Hold::new(MaintenanceStage::BeforeBatch, 1);
    let mut w = setup(Some(hold.hook()));
    seed_legacy(&w, 10);
    let mut events = subscribe(&w);
    start(&mut w, web(), "fails", backfill(3, None)).unwrap();
    w.rt.block_on(hold.reached());

    // The writer goes away between the first and the second batch.
    let World { rt, store, dir, .. } = w;
    store.close();
    hold.release();
    let seen = events_until_settled(&rt, &mut events, "fails");
    let done = settled_view(&seen);
    assert_eq!(
        done.operation.state,
        OperationState::Failed {
            code: ErrorCode::StorageUnavailable
        }
    );
    // The first batch was reported; the failed step is claimed neither way.
    assert_eq!(done.effect, MaintenanceEffect::Unknown);
    assert_eq!((done.batches, converted_of(done)), (1, 3));
    assert!(!done.message.as_deref().unwrap().contains("host.db"));
    assert_eq!(unconverted_at(dir.path().join("host.db")), 7);
}

fn unconverted_at(path: PathBuf) -> i64 {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE subkind IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn the_same_operation_resent_does_not_run_twice_and_another_request_under_its_id_is_refused() {
    let hold = Hold::new(MaintenanceStage::Started, 0);
    let mut w = setup(Some(hold.hook()));
    seed_legacy(&w, 6);
    let mut events = subscribe(&w);
    let first = start(&mut w, web(), "once", backfill(2, None)).unwrap();
    assert_eq!(first.disposition, CommandDisposition::Accepted);
    w.rt.block_on(hold.reached());

    let resent = start(&mut w, web(), "once", backfill(2, None)).unwrap();
    assert_eq!(resent.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(
        view_of(&resent).operation.operation_id,
        view_of(&first).operation.operation_id
    );
    let other = start(&mut w, web(), "once", backfill(3, None)).unwrap_err();
    assert_eq!(other.code, ErrorCode::Conflict);

    hold.release();
    let seen = events_until_settled(&w.rt, &mut events, "once");
    assert_eq!(settled_view(&seen).batches, 3);
    assert_eq!(unconverted(&w), 0);

    // Resent after it ended: the final view again, and no second run.
    let late = start(&mut w, web(), "once", backfill(2, None)).unwrap();
    assert_eq!(late.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(&view_of(&late), settled_view(&seen));
    assert_eq!(list(&w).len(), 1);
}

#[test]
fn two_clients_starting_a_backfill_at_once_get_one_run_and_the_other_is_told_busy_or_joined() {
    let hold = Hold::new(MaintenanceStage::Started, 0);
    let mut w = setup(Some(hold.hook()));
    seed_legacy(&w, 4);
    let mut events = subscribe(&w);
    start(&mut w, web(), "from-web", backfill(2, None)).unwrap();
    w.rt.block_on(hold.reached());

    // Another client with another id is refused while one runs ...
    let busy = start(&mut w, local_desktop(), "from-desktop", backfill(2, None)).unwrap_err();
    assert_eq!(busy.code, ErrorCode::Busy);
    // ... a client with the first id joins it ...
    let joined = start(&mut w, local_desktop(), "from-web", backfill(2, None)).unwrap();
    assert_eq!(joined.disposition, CommandDisposition::AlreadyApplied);
    // ... and the refused id was not kept.
    assert_eq!(
        get(&w, "from-desktop").unwrap_err().code,
        ErrorCode::NotFound
    );

    hold.release();
    let seen = events_until_settled(&w.rt, &mut events, "from-web");
    assert_eq!(settled_view(&seen).operation.state, OperationState::Ready);

    // Once it ended, the other client may run its own.
    start(&mut w, local_desktop(), "from-desktop", backfill(2, None)).unwrap();
    let seen = events_until_settled(&w.rt, &mut events, "from-desktop");
    assert_eq!(settled_view(&seen).operation.state, OperationState::Ready);
    assert_eq!(list(&w).len(), 2);
}

/// Hold an operation at `stage`, stop the host there, and let it go.
fn stopped_at(stage: MaintenanceStage, committed: u32) {
    let hold = Hold::new(stage, committed);
    let mut w = setup(Some(hold.hook()));
    seed_legacy(&w, 10);
    start(&mut w, web(), "restart", backfill(3, None)).unwrap();
    w.rt.block_on(hold.reached());
    assert!(w.tasks.busy(), "a running maintenance keeps the host busy");

    w.tasks.request_stop();
    hold.release();
    w.settle();

    // Nothing is committed after the stop; the result is not a success.
    let view = get(&w, "restart").unwrap();
    assert_eq!(
        view.operation.state,
        OperationState::Cancelled {
            reason: CancelReason::Shutdown
        }
    );
    assert_eq!(view.batches, committed);
    assert_eq!(unconverted(&w), 10 - i64::from(committed) * 3);
    assert_eq!(
        view.effect,
        if committed == 0 {
            MaintenanceEffect::None
        } else {
            MaintenanceEffect::Partial
        }
    );
    // A host that is stopping accepts no new operation.
    let refused = start(&mut w, web(), "after", backfill(3, None)).unwrap_err();
    assert_eq!(refused.code, ErrorCode::Draining);
    assert_eq!(unconverted(&w), 10 - i64::from(committed) * 3);
}

#[test]
fn a_stop_before_the_first_batch_commits_nothing() {
    stopped_at(MaintenanceStage::Started, 0);
}

#[test]
fn a_stop_between_two_batches_commits_no_further_batch() {
    stopped_at(MaintenanceStage::BeforeBatch, 1);
}

#[test]
fn a_running_maintenance_holds_the_restart_gate_and_a_cancel_is_still_admitted() {
    let hold = Hold::new(MaintenanceStage::BeforeBatch, 1);
    let mut w = setup(Some(hold.hook()));
    seed_legacy(&w, 10);
    let gate = w.host.service().restart_gate();
    let mut events = subscribe(&w);
    start(&mut w, web(), "gate", backfill(3, None)).unwrap();
    w.rt.block_on(hold.reached());

    // The restart cannot reach its safe point while the operation is open ...
    w.rt.block_on(async {
        tokio::select! {
            biased;
            () = gate.drained() => panic!("drained while a maintenance is running"),
            () = std::future::ready(()) => {}
        }
    });
    // ... a preparing host takes no new maintenance, but lets a cancel through.
    assert!(gate.transition(1, HostRestartPhase::Preparing));
    let refused = start(&mut w, web(), "late", backfill(3, None)).unwrap_err();
    assert_eq!(refused.code, ErrorCode::Busy);
    cancel(&mut w, web(), "gate").unwrap();

    hold.release();
    w.rt.block_on(gate.drained());
    let seen = events_until_settled(&w.rt, &mut events, "gate");
    assert_eq!(
        settled_view(&seen).operation.state,
        OperationState::Cancelled {
            reason: CancelReason::Requested
        }
    );
    assert_eq!(unconverted(&w), 7);
}

#[test]
fn a_full_table_refuses_only_unknown_ids_and_keeps_answering_known_ones() {
    let mut w = setup(None);
    let mut events = subscribe(&w);
    // The table fills with the 64th operation, which is still accepted.
    for n in 0..64 {
        let id = format!("op-{n}");
        let accepted = start(&mut w, web(), &id, backfill(2, None)).unwrap();
        assert_eq!(accepted.disposition, CommandDisposition::Accepted, "{id}");
        events_until_settled(&w.rt, &mut events, &id);
    }

    // Right after it filled, the newest id is known and an unknown one is refused.
    let newest = start(&mut w, web(), "op-63", backfill(2, None)).unwrap();
    assert_eq!(newest.disposition, CommandDisposition::AlreadyApplied);
    let refused = start(&mut w, web(), "op-64", backfill(2, None)).unwrap_err();
    assert_eq!(refused.code, ErrorCode::LimitExceeded);
    assert_eq!(get(&w, "op-64").unwrap_err().code, ErrorCode::NotFound);

    // The oldest id is still known: resent, it is answered and never runs again.
    seed_legacy(&w, 4);
    let oldest = start(&mut w, web(), "op-0", backfill(2, None)).unwrap();
    assert_eq!(oldest.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(unconverted(&w), 4);

    // A known id with another request is a conflict, not a new operation.
    let reused = start(&mut w, web(), "op-0", backfill(3, None)).unwrap_err();
    assert_eq!(reused.code, ErrorCode::Conflict);
    assert_eq!(unconverted(&w), 4);

    // Queries of known ids keep working while the table is full.
    for id in ["op-0", "op-63"] {
        assert_eq!(get(&w, id).unwrap().operation.state, OperationState::Ready);
    }
    // Refusals did not make room for anything.
    let refused_again = start(&mut w, web(), "op-65", backfill(2, None)).unwrap_err();
    assert_eq!(refused_again.code, ErrorCode::LimitExceeded);
    assert_eq!(unconverted(&w), 4);
}

#[test]
fn a_step_that_converted_rows_before_it_failed_is_unknown_and_claims_no_count() {
    let mut w = setup(None);
    seed_legacy(&w, 3);
    {
        // Two events that only the attribution touches, and a fault that makes
        // that second transaction of a step fail after the conversion committed.
        let conn = rusqlite::Connection::open(db_path(&w)).unwrap();
        for n in 0..2 {
            conn.execute(
                "INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind) \
                 VALUES (1, ?1, ?1, 'TaskProgressed', '{}', 'TaskProgressed')",
                [100 + n],
            )
            .unwrap();
        }
        conn.execute_batch(
            "CREATE TRIGGER refuse_attribution BEFORE UPDATE ON events \
             WHEN OLD.subkind IS NOT NULL AND OLD.run_id IS NULL \
             BEGIN SELECT RAISE(ABORT, 'fault'); END;",
        )
        .unwrap();
    }
    let mut events = subscribe(&w);
    start(&mut w, web(), "half", backfill(10, None)).unwrap();
    let seen = events_until_settled(&w.rt, &mut events, "half");
    let done = settled_view(&seen);
    assert_eq!(
        done.operation.state,
        OperationState::Failed {
            code: ErrorCode::StorageUnavailable
        }
    );
    // The conversion did commit, but the writer did not say so: the operation
    // claims neither a count nor "nothing happened", and tells to read again.
    assert_eq!(done.effect, MaintenanceEffect::Unknown);
    assert_eq!((done.batches, converted_of(done)), (0, 0));
    assert!(progress_views(&seen).is_empty());
    assert!(
        done.message
            .as_deref()
            .unwrap()
            .contains("read the state again")
    );
    assert_eq!(unconverted(&w), 0);
}

#[test]
fn automatic_retention_running_beside_a_backfill_does_not_change_what_it_reports() {
    // Another writer path removes rows while the operation runs (what the
    // retention does on its own): the counters come from the writer's own
    // reports, so rows deleted elsewhere are never counted as this operation's.
    let hold = Hold::new(MaintenanceStage::BeforeBatch, 1);
    let mut w = setup(Some(hold.hook()));
    seed_legacy(&w, 10);
    let mut events = subscribe(&w);
    start(&mut w, web(), "beside", backfill(3, Some(6))).unwrap();
    w.rt.block_on(hold.reached());
    {
        // Three rows nobody converted yet disappear between the two batches.
        let conn = rusqlite::Connection::open(db_path(&w)).unwrap();
        conn.execute(
            "DELETE FROM events WHERE id IN \
             (SELECT id FROM events WHERE subkind IS NULL ORDER BY id DESC LIMIT 3)",
            [],
        )
        .unwrap();
    }
    hold.release();
    let seen = events_until_settled(&w.rt, &mut events, "beside");
    let done = settled_view(&seen);
    assert_eq!(done.operation.state, OperationState::Ready);
    assert_eq!(done.effect, MaintenanceEffect::Complete);
    assert_eq!((done.batches, converted_of(done)), (2, 6));
    assert_eq!(unconverted(&w), 1);
}

fn client(name: &str) -> Provenance {
    Provenance::human(Surface::Cli, ClientId::new(name).unwrap())
}

#[test]
fn a_client_that_leaves_stops_only_what_it_started_at_the_next_batch_boundary() {
    let hold = Hold::new(MaintenanceStage::BeforeBatch, 1);
    let connections = Arc::new(ConnectionHooks::default());
    let mut w = setup_with_connections(Some(hold.hook()), &connections);
    seed_legacy(&w, 10);
    let mut events = subscribe(&w);
    let owner = client("cli-owner");
    let bystander = client("cli-bystander");
    let owner_id = owner.client_id.clone().unwrap();
    let owner_connection = connections.open(owner_id);
    let bystander_connection = connections.open(bystander.client_id.clone().unwrap());

    start(&mut w, owner, "owned", backfill(3, None)).unwrap();
    w.rt.block_on(hold.reached());

    // A client that started nothing leaves: the operation is not asked to stop.
    drop(bystander_connection);
    let still = get(&w, "owned").unwrap();
    assert!(!still.operation.state.is_final());

    // Its owner leaves: the operation stops after the batch it had committed.
    drop(owner_connection);
    hold.release();
    let seen = events_until_settled(&w.rt, &mut events, "owned");
    let done = settled_view(&seen);
    assert_eq!(
        done.operation.state,
        OperationState::Cancelled {
            reason: CancelReason::Requested
        }
    );
    assert_eq!((done.batches, converted_of(done)), (1, 3));
    assert_eq!(unconverted(&w), 7);
}

#[test]
fn an_operation_without_an_owning_client_is_not_stopped_by_any_client_leaving() {
    let hold = Hold::new(MaintenanceStage::BeforeBatch, 1);
    let connections = Arc::new(ConnectionHooks::default());
    let mut w = setup_with_connections(Some(hold.hook()), &connections);
    seed_legacy(&w, 6);
    let mut events = subscribe(&w);
    let someone = client("cli-someone");
    let connection = connections.open(someone.client_id.clone().unwrap());

    let ownerless = Provenance {
        client_id: None,
        ..web()
    };
    start(&mut w, ownerless, "ownerless", backfill(2, None)).unwrap();
    w.rt.block_on(hold.reached());

    drop(connection);
    hold.release();
    let seen = events_until_settled(&w.rt, &mut events, "ownerless");
    let done = settled_view(&seen);
    assert_eq!(done.operation.state, OperationState::Ready);
    assert_eq!(unconverted(&w), 0);
}

/// A hook that holds up the notices of the first departure: the thread of the leaving
/// connection waits in it until `release`, so what the client does meanwhile
/// happens before the later hooks (the area's) hear of the departure.
struct HeldNotice {
    entered: std::sync::mpsc::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
}

fn hold_notices(connections: &ConnectionHooks) -> HeldNotice {
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let first = std::sync::atomic::AtomicBool::new(true);
    connections.register(Arc::new(move |_, _| {
        // Only the first departure is held up.
        if !first.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        entered_tx.send(()).unwrap();
        release_rx.lock().unwrap().recv().unwrap();
    }));
    HeldNotice { entered, release }
}

#[test]
fn a_late_notice_of_an_earlier_connection_does_not_stop_what_the_client_started_since() {
    // Given: the notices of a departure are held up before they reach the area.
    let hold = Hold::new(MaintenanceStage::BeforeBatch, 1);
    let connections = Arc::new(ConnectionHooks::default());
    let notices = hold_notices(&connections);
    let mut w = setup_with_connections(Some(hold.hook()), &connections);
    seed_legacy(&w, 6);
    let mut events = subscribe(&w);
    let who = client("cli-again");
    let id = who.client_id.clone().unwrap();
    let first = connections.open(id.clone());
    let leaving = std::thread::spawn(move || drop(first));
    notices.entered.recv().unwrap();

    // When: the client connects again and starts an operation, and only then
    // the notice of the connection that ended reaches the area.
    let second = connections.open(id);
    start(&mut w, who, "since", backfill(2, None)).unwrap();
    w.rt.block_on(hold.reached());
    notices.release.send(()).unwrap();
    leaving.join().unwrap();

    // Then: the operation of the client's current connection is not stopped.
    hold.release();
    let seen = events_until_settled(&w.rt, &mut events, "since");
    let done = settled_view(&seen);
    assert_eq!(done.operation.state, OperationState::Ready);
    assert_eq!(unconverted(&w), 0);
    drop(second);
}

#[test]
fn a_late_notice_still_stops_what_the_client_started_before_it_left() {
    // Given: an operation of a client, and the notices of its departure held up.
    let hold = Hold::new(MaintenanceStage::BeforeBatch, 1);
    let connections = Arc::new(ConnectionHooks::default());
    let notices = hold_notices(&connections);
    let mut w = setup_with_connections(Some(hold.hook()), &connections);
    seed_legacy(&w, 10);
    let mut events = subscribe(&w);
    let who = client("cli-leaving");
    let id = who.client_id.clone().unwrap();
    let first = connections.open(id.clone());
    start(&mut w, who, "before", backfill(3, None)).unwrap();
    w.rt.block_on(hold.reached());
    let leaving = std::thread::spawn(move || drop(first));
    notices.entered.recv().unwrap();

    // When: the client is back by the time the notice reaches the area (a
    // count of its connections now would hide that it left).
    let second = connections.open(id);
    notices.release.send(()).unwrap();
    leaving.join().unwrap();

    // Then: what it started before it left stops at the next batch boundary.
    hold.release();
    let seen = events_until_settled(&w.rt, &mut events, "before");
    let done = settled_view(&seen);
    assert_eq!(
        done.operation.state,
        OperationState::Cancelled {
            reason: CancelReason::Requested
        }
    );
    assert_eq!((done.batches, converted_of(done)), (1, 3));
    drop(second);
}
