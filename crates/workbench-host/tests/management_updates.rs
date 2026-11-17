//! The management updates of `memory` and `improvement` (ADR 0045, stage 14), through
//! the host's command entry the way a desktop and a web client reach it.
//!
//! Every update is one conditional write on the storage writer, quoting the revision
//! the read side showed. These tests hold an update at the two points a restart or a
//! cancel can matter (its start, and right before the write) with a gate the test
//! opens, so no test waits for time to pass: every wait is for an event the host or
//! the gate sends.
#[path = "memory_reads/support.rs"]
mod support;

mod common;

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use storage::improvement::{
    ImprovementRecordOutcome, ImprovementSeverity, ImprovementSource, ImprovementWritePolicy,
    NewImprovementCandidate,
};
use storage::memory::{Lesson, LessonScope, MemoryStatus as StoredMemoryStatus};
use support::{World, operation, project, world};
use workbench_host::areas::improvement::{ImprovementArea, draft_file_name};
use workbench_host::areas::memory::MemoryArea;
use workbench_host::areas::memory::update::{UpdateHook, UpdateStage};
use workbench_host::areas::{Areas, diagnostics::read_ops::SlugOf};
use workbench_host::bootstrap::restart::HostRestartPhase;
use workbench_host::provenance::local_desktop;
use workbench_protocol::WorkbenchError;
use workbench_protocol::commands::{Command, CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
use workbench_protocol::ids::{ImprovementCandidateId, MemoryId, OperationId};
use workbench_protocol::management::{
    ActionState, DenialReason, ImprovementAction, ImprovementStatus, MemoryAction, MemoryRef,
    MemoryStatus, TriageSubject,
};
use workbench_protocol::namespaces::diagnostics::OperationTarget;
use workbench_protocol::namespaces::improvement::{
    GetImprovement, ImprovementCommand, ImprovementDetail, ImprovementEvent, ImprovementQuery,
    ImprovementReply, ImprovementResult, ImprovementUpdateView, TriageImprovement,
    UpdateImprovement,
};
use workbench_protocol::namespaces::memory::{
    GetMemory, MemoryCommand, MemoryDetail, MemoryEvent, MemoryQuery, MemoryReply, MemoryResult,
    MemoryUpdateView, UpdateMemory,
};
use workbench_protocol::operation::{CancelReason, OperationState};
use workbench_protocol::queries::{Query, QueryReply};
use workbench_protocol::wire::Revision;

const SLUG: &str = "acme/widgets";
const OTHER_SLUG: &str = "other/project";

/// A gate the test opens: every update that reaches `stage` counts itself in
/// `reached` and waits until `release` is called.
struct Hold {
    stage: UpdateStage,
    reached: tokio::sync::Semaphore,
    open: tokio::sync::watch::Sender<bool>,
}

impl Hold {
    fn new(stage: UpdateStage) -> Arc<Self> {
        Arc::new(Self {
            stage,
            reached: tokio::sync::Semaphore::new(0),
            open: tokio::sync::watch::channel(false).0,
        })
    }

    fn hook(self: &Arc<Self>) -> UpdateHook {
        let hold = self.clone();
        Arc::new(move |stage, _| {
            let hold = hold.clone();
            Box::pin(async move {
                if stage == hold.stage {
                    hold.reached.add_permits(1);
                    let _ = hold.open.subscribe().wait_for(|open| *open).await;
                }
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        })
    }

    /// Wait until `count` updates are inside the gate.
    async fn reached(&self, count: u32) {
        self.reached.acquire_many(count).await.unwrap().forget();
    }

    fn release(&self) {
        self.open.send_replace(true);
    }
}

fn slug() -> SlugOf {
    Arc::new(|_: &Path| SLUG.to_owned())
}

/// A host with both updating areas. `drafts` says whether triage has a draft directory.
fn setup(hook: Option<UpdateHook>, drafts: bool) -> World {
    world(|parts| {
        let draft_dir = drafts.then(|| parts.dir.join("drafts"));
        Areas {
            memory: MemoryArea::from_parts(
                parts.tasks.clone(),
                parts.projection.source("memory").unwrap(),
                parts.database.clone(),
                slug(),
            )
            .with_writer(parts.storage.clone(), hook.clone()),
            improvement: ImprovementArea::from_parts(
                parts.tasks,
                parts.projection.source("improvement").unwrap(),
                parts.database,
                slug(),
                true,
            )
            .with_writer(parts.storage, draft_dir, hook),
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

fn names() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// ---- memory -----------------------------------------------------------------

fn lesson(id: &str, project: &str) -> Lesson {
    Lesson {
        id: id.into(),
        project: project.into(),
        task_id: "task-1".into(),
        content: format!("content of {id}"),
        evidence: format!("evidence of {id}"),
        scope: LessonScope::Project,
    }
}

fn seed(w: &World, id: &str) {
    w.store
        .handle()
        .append_lesson(&lesson(id, SLUG))
        .expect("the lesson is stored");
}

fn memory_command(
    make: fn(UpdateMemory) -> MemoryCommand,
    operation_id: &str,
    id: &str,
    revision: u64,
) -> MemoryCommand {
    make(UpdateMemory {
        operation_id: operation(operation_id),
        project: project(),
        target: MemoryRef {
            id: MemoryId::new(id).unwrap(),
            expected_revision: Revision(revision),
        },
    })
}

fn send_memory(
    w: &mut World,
    who: Provenance,
    command: MemoryCommand,
) -> Result<CommandReply, WorkbenchError> {
    w.host.handle_command(Command::Memory(command), who)
}

fn memory_view(reply: &CommandReply) -> MemoryUpdateView {
    match reply.result.as_deref() {
        Some(CommandResult::Memory(MemoryResult::Operation(view))) => view.clone(),
        other => panic!("not a memory update: {other:?}"),
    }
}

fn memory_update(w: &World, operation_id: &str) -> Result<MemoryUpdateView, WorkbenchError> {
    match w.query(Query::Memory(MemoryQuery::GetMemoryUpdate(
        OperationTarget {
            operation_id: operation(operation_id),
        },
    )))? {
        QueryReply::Memory(MemoryReply::Update(view)) => Ok(*view),
        other => panic!("{other:?}"),
    }
}

fn memory_detail(w: &World, id: &str) -> MemoryDetail {
    let name = format!("read-{id}-{}", names());
    w.read(|| {
        match w.query(Query::Memory(MemoryQuery::GetMemory(GetMemory {
            operation_id: operation(&name),
            project: project(),
            id: MemoryId::new(id).unwrap(),
        })))? {
            QueryReply::Memory(MemoryReply::Detail(outcome)) => Ok::<_, WorkbenchError>(*outcome),
            other => panic!("{other:?}"),
        }
    })
    .data
    .unwrap()
}

/// The lesson as the storage holds it, read directly (the host may be stopped).
fn stored_lesson(w: &World, id: &str) -> (storage::memory::MemoryEntry, u64) {
    let database = storage::Database::open_read_only(&db_path(w)).unwrap();
    (
        database
            .memory_entry(id)
            .unwrap()
            .expect("the lesson exists"),
        database.memory_revision(id).unwrap(),
    )
}

fn failed(view: &MemoryUpdateView) -> ErrorCode {
    match view.operation.state {
        OperationState::Failed { code } => code,
        ref other => panic!("expected a failed update, got {other:?}"),
    }
}

fn is_ready(state: &OperationState) -> bool {
    *state == OperationState::Ready
}

#[test]
fn a_desktop_and_a_web_client_updating_at_one_revision_one_wins_and_the_other_is_told_to_reread() {
    let hold = Hold::new(UpdateStage::BeforeCommit);
    let mut w = setup(Some(hold.hook()), false);
    seed(&w, "race");
    let shown = memory_detail(&w, "race").state.revision;

    let desktop = memory_command(
        MemoryCommand::SetMemoryFavorite,
        "desktop-1",
        "race",
        shown.0,
    );
    let browser = memory_command(MemoryCommand::ValidateMemory, "web-1", "race", shown.0);
    send_memory(&mut w, local_desktop(), desktop).unwrap();
    send_memory(&mut w, web(), browser).unwrap();
    // Both have read the lesson and are about to write: the writer decides.
    w.rt.block_on(hold.reached(2));
    hold.release();
    w.settle();

    let desktop = memory_update(&w, "desktop-1").unwrap();
    let browser = memory_update(&w, "web-1").unwrap();
    let (winner, loser) = match (
        is_ready(&desktop.operation.state),
        is_ready(&browser.operation.state),
    ) {
        (true, false) => (&desktop, &browser),
        (false, true) => (&browser, &desktop),
        other => panic!("exactly one update wins, got {other:?}"),
    };
    assert_eq!(failed(loser), ErrorCode::StaleRevision);
    assert!(
        loser.message.as_deref().unwrap().contains("again"),
        "the loser is told to read again: {loser:?}"
    );
    assert_eq!(winner.changed, Some(true));

    // Only the winner's effect is in the lesson, and its revision is the one shown next.
    let (entry, revision) = stored_lesson(&w, "race");
    assert_eq!(Some(Revision(revision)), winner.revision);
    assert_eq!(
        (entry.favorite, entry.status),
        if winner.action == MemoryAction::SetFavorite {
            (true, StoredMemoryStatus::Candidate)
        } else {
            (false, StoredMemoryStatus::Validated)
        }
    );
    assert_eq!(memory_detail(&w, "race").state.revision, Revision(revision));

    // Nothing was retried for the loser; it decides again from what it reads.
    let again = if loser.action == MemoryAction::SetFavorite {
        MemoryCommand::SetMemoryFavorite
    } else {
        MemoryCommand::ValidateMemory
    };
    let reply = send_memory(
        &mut w,
        web(),
        memory_command(again, "web-2", "race", revision),
    )
    .unwrap();
    assert_eq!(reply.disposition, CommandDisposition::Accepted);
    w.settle();
    assert!(is_ready(
        &memory_update(&w, "web-2").unwrap().operation.state
    ));
}

#[test]
fn favoriting_and_rejecting_at_one_revision_never_leave_a_rejected_favorite() {
    let hold = Hold::new(UpdateStage::BeforeCommit);
    let mut w = setup(Some(hold.hook()), false);
    seed(&w, "both");
    let shown = memory_detail(&w, "both").state.revision;

    send_memory(
        &mut w,
        local_desktop(),
        memory_command(MemoryCommand::SetMemoryFavorite, "fav", "both", shown.0),
    )
    .unwrap();
    send_memory(
        &mut w,
        web(),
        memory_command(MemoryCommand::RejectMemory, "rej", "both", shown.0),
    )
    .unwrap();
    w.rt.block_on(hold.reached(2));
    hold.release();
    w.settle();

    let fav = memory_update(&w, "fav").unwrap();
    let rej = memory_update(&w, "rej").unwrap();
    assert_eq!(
        [&fav, &rej]
            .iter()
            .filter(|view| is_ready(&view.operation.state))
            .count(),
        1,
        "{fav:?} {rej:?}"
    );
    let (entry, revision) = stored_lesson(&w, "both");
    assert!(
        !(entry.favorite && entry.status == StoredMemoryStatus::Rejected),
        "a favorite is not rejected"
    );

    // The loser asks again at the new revision and is refused for the state, not
    // for the revision.
    let (retry, expected) = if entry.favorite {
        (
            MemoryCommand::RejectMemory as fn(_) -> _,
            DenialReason::FavoriteMustBeCleared,
        )
    } else {
        (
            MemoryCommand::SetMemoryFavorite as fn(_) -> _,
            DenialReason::AlreadyResolved,
        )
    };
    send_memory(
        &mut w,
        web(),
        memory_command(retry, "retry", "both", revision),
    )
    .unwrap();
    w.settle();
    let retry = memory_update(&w, "retry").unwrap();
    assert!(matches!(
        retry.operation.state,
        OperationState::Failed { .. }
    ));
    assert_eq!(retry.denial, Some(expected));
    assert_eq!(
        stored_lesson(&w, "both").1,
        revision,
        "a refusal writes nothing"
    );
}

#[test]
fn a_denied_action_names_the_reason_the_read_side_showed_and_writes_nothing() {
    let mut w = setup(None, false);
    seed(&w, "plain");
    seed(&w, "liked");
    w.store.handle().set_lesson_favorite("liked", true).unwrap();

    for (id, make, action, reason) in [
        (
            "liked",
            MemoryCommand::SetMemoryFavorite as fn(_) -> _,
            MemoryAction::SetFavorite,
            DenialReason::NotApplicableInState,
        ),
        (
            "plain",
            MemoryCommand::PromoteMemory as fn(_) -> _,
            MemoryAction::Promote,
            DenialReason::NotApplicableInState,
        ),
        (
            "liked",
            MemoryCommand::RejectMemory as fn(_) -> _,
            MemoryAction::Reject,
            DenialReason::FavoriteMustBeCleared,
        ),
    ] {
        let shown = memory_detail(&w, id);
        let availability = shown
            .state
            .actions
            .iter()
            .find(|a| a.action == action)
            .unwrap()
            .state;
        assert_eq!(availability, ActionState::Denied { reason });

        let name = format!("deny-{id}-{action:?}");
        let reply = send_memory(
            &mut w,
            local_desktop(),
            memory_command(make, &name, id, shown.state.revision.0),
        )
        .unwrap();
        // The handler answers at once; the outcome is the operation's.
        assert!(!is_ready(&memory_view(&reply).operation.state));
        w.settle();
        let view = memory_update(&w, &name).unwrap();
        assert_eq!(view.denial, Some(reason));
        assert_eq!(view.revision, None);
        assert_eq!(stored_lesson(&w, id).1, shown.state.revision.0);
    }
}

#[test]
fn the_same_operation_resent_is_not_applied_twice_and_another_request_under_its_id_is_refused() {
    let hold = Hold::new(UpdateStage::Started);
    let mut w = setup(Some(hold.hook()), false);
    seed(&w, "once");
    let shown = memory_detail(&w, "once").state.revision;
    let command = memory_command(MemoryCommand::SetMemoryFavorite, "op-1", "once", shown.0);

    let first = send_memory(&mut w, web(), command.clone()).unwrap();
    assert_eq!(first.disposition, CommandDisposition::Accepted);
    w.rt.block_on(hold.reached(1));
    // Resent while it runs: answered from the table, nothing new starts.
    let pending = send_memory(&mut w, web(), command.clone()).unwrap();
    assert_eq!(pending.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(
        memory_view(&pending).operation.state,
        OperationState::Pending
    );
    // The same id for another request is not this operation.
    let other = send_memory(
        &mut w,
        web(),
        memory_command(MemoryCommand::ClearMemoryFavorite, "op-1", "once", shown.0),
    )
    .unwrap_err();
    assert_eq!(other.code, ErrorCode::Conflict);

    hold.release();
    w.settle();
    let settled = send_memory(&mut w, web(), command).unwrap();
    assert_eq!(settled.disposition, CommandDisposition::AlreadyApplied);
    let view = memory_view(&settled);
    assert!(is_ready(&view.operation.state));
    assert_eq!(view.changed, Some(true));

    // One write: the revision moved once, and the read side shows what was written.
    let (entry, revision) = stored_lesson(&w, "once");
    assert_eq!(revision, shown.0 + 1);
    assert_eq!(view.revision, Some(Revision(revision)));
    assert!(entry.favorite);
    let detail = memory_detail(&w, "once");
    assert!(detail.state.favorite);
    assert_eq!(detail.state.revision, Revision(revision));
}

#[test]
fn a_lesson_of_another_project_or_none_at_all_is_not_found_and_is_not_written() {
    let mut w = setup(None, false);
    w.store
        .handle()
        .append_lesson(&lesson("foreign", OTHER_SLUG))
        .unwrap();
    let revision = stored_lesson(&w, "foreign").1;

    for (id, name) in [("foreign", "foreign-op"), ("missing", "missing-op")] {
        send_memory(
            &mut w,
            local_desktop(),
            memory_command(MemoryCommand::SetMemoryFavorite, name, id, revision),
        )
        .unwrap();
        w.settle();
        let view = memory_update(&w, name).unwrap();
        assert_eq!(failed(&view), ErrorCode::NotFound);
        assert_eq!(view.denial, None);
    }
    // Nothing happened to the other project's lesson: its revision is the one
    // the writer still accepts.
    let (entry, after) = stored_lesson(&w, "foreign");
    assert_eq!((after, entry.favorite), (revision, false));
    assert!(matches!(
        w.store
            .handle()
            .update_memory(
                "foreign",
                revision,
                storage::management::MemoryOp::SetFavorite
            )
            .unwrap(),
        storage::management::CasOutcome::Applied { .. }
    ));
}

#[test]
fn a_lesson_that_disappears_after_the_project_check_is_not_found_from_the_writer() {
    let hold = Hold::new(UpdateStage::BeforeCommit);
    let mut w = setup(Some(hold.hook()), false);
    seed(&w, "gone");
    let shown = memory_detail(&w, "gone").state.revision;
    send_memory(
        &mut w,
        web(),
        memory_command(MemoryCommand::ValidateMemory, "gone-op", "gone", shown.0),
    )
    .unwrap();
    // The membership check passed; the lesson goes before the write.
    w.rt.block_on(hold.reached(1));
    rusqlite::Connection::open(db_path(&w))
        .unwrap()
        .execute("DELETE FROM memory_entries WHERE id = 'gone'", [])
        .unwrap();
    hold.release();
    w.settle();
    assert_eq!(
        failed(&memory_update(&w, "gone-op").unwrap()),
        ErrorCode::NotFound
    );
}

/// Hold an update at `stage`, stop the host there, and let it go.
fn stopped_at(stage: UpdateStage) {
    let hold = Hold::new(stage);
    let mut w = setup(Some(hold.hook()), false);
    seed(&w, "stop");
    let shown = memory_detail(&w, "stop").state.revision;
    let command = memory_command(MemoryCommand::SetMemoryFavorite, "stop-op", "stop", shown.0);
    send_memory(&mut w, web(), command.clone()).unwrap();
    w.rt.block_on(hold.reached(1));

    w.tasks.request_stop();
    hold.release();
    w.settle();

    // A result that was not committed is not a success: it is cancelled and says
    // to read the lesson.
    let view = memory_update(&w, "stop-op").unwrap();
    assert_eq!(
        view.operation.state,
        OperationState::Cancelled {
            reason: CancelReason::Shutdown
        }
    );
    assert_eq!((view.revision, view.changed), (None, None));
    assert!(view.message.as_deref().unwrap().contains("read"));
    let (entry, revision) = stored_lesson(&w, "stop");
    assert_eq!((revision, entry.favorite), (shown.0, false));

    // Sent again, the same operation is still the cancelled one; a new one is not
    // accepted by a host that is stopping.
    let repeat = send_memory(&mut w, web(), command).unwrap();
    assert_eq!(repeat.disposition, CommandDisposition::AlreadyApplied);
    assert!(matches!(
        memory_view(&repeat).operation.state,
        OperationState::Cancelled { .. }
    ));
    let refused = send_memory(
        &mut w,
        web(),
        memory_command(MemoryCommand::SetMemoryFavorite, "after", "stop", shown.0),
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Draining);
    assert_eq!(stored_lesson(&w, "stop").1, shown.0);
}

#[test]
fn a_stop_before_the_update_starts_writes_nothing() {
    stopped_at(UpdateStage::Started);
}

#[test]
fn a_stop_right_before_the_write_writes_nothing() {
    stopped_at(UpdateStage::BeforeCommit);
}

#[test]
fn an_accepted_update_holds_the_restart_gate_until_its_outcome_is_settled() {
    let hold = Hold::new(UpdateStage::Started);
    let mut w = setup(Some(hold.hook()), false);
    seed(&w, "gate");
    let shown = memory_detail(&w, "gate").state.revision;
    let gate = w.host.service().restart_gate();

    send_memory(
        &mut w,
        web(),
        memory_command(MemoryCommand::SetMemoryFavorite, "gate-op", "gate", shown.0),
    )
    .unwrap();
    w.rt.block_on(hold.reached(1));
    // The restart cannot reach its safe point while the update is open ...
    w.rt.block_on(async {
        tokio::select! {
            biased;
            () = gate.drained() => panic!("drained while an update is running"),
            () = std::future::ready(()) => {}
        }
    });
    // ... and once it prepares, no new management update is accepted.
    assert!(gate.transition(1, HostRestartPhase::Preparing));
    let refused = send_memory(
        &mut w,
        web(),
        memory_command(MemoryCommand::ValidateMemory, "late", "gate", shown.0),
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Busy);

    hold.release();
    w.rt.block_on(gate.drained());
    assert!(is_ready(
        &memory_update(&w, "gate-op").unwrap().operation.state
    ));
}

#[test]
fn an_update_the_core_does_not_hold_is_not_found_so_the_client_reads_the_state() {
    let w = setup(None, false);
    let error = memory_update(&w, "never-sent").unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
    // A core that restarted has an empty table, like a fresh area over the same
    // database: the update is unknown, and the lesson is where the answer is.
    seed(&w, "kept");
    let fresh = MemoryArea::from_parts(
        w.tasks.clone(),
        w.host.projection().source("memory").unwrap(),
        db_path(&w),
        slug(),
    );
    assert!(fresh.update_of(&operation("never-sent")).is_err());
}

#[test]
fn the_completion_of_an_update_is_announced_with_the_view_the_query_returns() {
    let mut w = setup(None, false);
    seed(&w, "told");
    let shown = memory_detail(&w, "told").state.revision;
    let mut events = w
        .host
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap();
    send_memory(
        &mut w,
        web(),
        memory_command(MemoryCommand::ValidateMemory, "told-op", "told", shown.0),
    )
    .unwrap();
    let announced = w.rt.block_on(async {
        loop {
            match events.recv().await.expect("the subscription is open").event {
                UiEvent::Memory(MemoryEvent::MemoryUpdateCompleted { update })
                    if update.operation.operation_id.as_str() == "told-op" =>
                {
                    return update;
                }
                _ => {}
            }
        }
    });
    w.settle();
    assert_eq!(announced, memory_update(&w, "told-op").unwrap());
    assert!(is_ready(&announced.operation.state));
}

// ---- improvement ------------------------------------------------------------

fn policy(max_candidates: u32) -> ImprovementWritePolicy {
    ImprovementWritePolicy {
        duplicate_cooldown: Duration::from_secs(60),
        daily_limit: 1000,
        run_error_daily_limit: 1000,
        max_candidates,
    }
}

fn record_in(w: &World, project: &str, id: &str, max_candidates: u32) {
    let outcome = w
        .store
        .handle()
        .record_improvement_candidate(
            project,
            NewImprovementCandidate {
                id: id.into(),
                source: ImprovementSource::Diagnostic,
                code: "tool_failure".into(),
                severity: ImprovementSeverity::Warning,
                title: format!("title of {id}"),
                evidence: format!("evidence of {id}"),
                dedup_key: format!("key-{id}"),
                run_id: None,
            },
            policy(max_candidates),
        )
        .unwrap();
    assert!(matches!(outcome, ImprovementRecordOutcome::Stored { .. }));
}

fn record(w: &World, id: &str) {
    record_in(w, SLUG, id, 1000);
}

fn candidate_id(id: &str) -> ImprovementCandidateId {
    ImprovementCandidateId::new(id).unwrap()
}

fn review_command(
    make: fn(UpdateImprovement) -> ImprovementCommand,
    operation_id: &str,
    id: &str,
    revision: u64,
) -> ImprovementCommand {
    make(UpdateImprovement {
        operation_id: operation(operation_id),
        project: project(),
        candidate: candidate_id(id),
        expected_revision: Revision(revision),
    })
}

fn triage_command(operation_id: &str, id: &str, revision: u64) -> ImprovementCommand {
    ImprovementCommand::TriageImprovement(TriageImprovement {
        operation_id: operation(operation_id),
        project: project(),
        subject: TriageSubject {
            candidate: candidate_id(id),
            expected_revision: Revision(revision),
        },
    })
}

fn send_improvement(
    w: &mut World,
    who: Provenance,
    command: ImprovementCommand,
) -> Result<CommandReply, WorkbenchError> {
    w.host.handle_command(Command::Improvement(command), who)
}

fn improvement_view(reply: &CommandReply) -> ImprovementUpdateView {
    match reply.result.as_deref() {
        Some(CommandResult::Improvement(ImprovementResult::Operation(view))) => view.clone(),
        other => panic!("not an improvement update: {other:?}"),
    }
}

fn improvement_update(
    w: &World,
    operation_id: &str,
) -> Result<ImprovementUpdateView, WorkbenchError> {
    match w.query(Query::Improvement(ImprovementQuery::GetImprovementUpdate(
        OperationTarget {
            operation_id: operation(operation_id),
        },
    )))? {
        QueryReply::Improvement(ImprovementReply::Update(view)) => Ok(*view),
        other => panic!("{other:?}"),
    }
}

fn improvement_detail(w: &World, id: &str) -> ImprovementDetail {
    let name = format!("read-{id}-{}", names());
    w.read(|| {
        match w.query(Query::Improvement(ImprovementQuery::GetImprovement(
            GetImprovement {
                operation_id: operation(&name),
                project: project(),
                candidate: candidate_id(id),
            },
        )))? {
            QueryReply::Improvement(ImprovementReply::Detail(outcome)) => Ok(*outcome),
            other => panic!("{other:?}"),
        }
    })
    .data
    .unwrap()
}

fn stored_candidate(w: &World, id: &str) -> Option<storage::improvement::ImprovementCandidate> {
    storage::Database::open_read_only(&db_path(w))
        .unwrap()
        .improvement_candidate(id)
        .unwrap()
}

fn improvement_failed(view: &ImprovementUpdateView) -> ErrorCode {
    match view.operation.state {
        OperationState::Failed { code } => code,
        ref other => panic!("expected a failed update, got {other:?}"),
    }
}

fn drafts(w: &World) -> Vec<String> {
    let dir = w.dir.path().join("drafts");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn two_clients_reviewing_and_dismissing_at_one_revision_leave_one_decision() {
    let hold = Hold::new(UpdateStage::BeforeCommit);
    let mut w = setup(Some(hold.hook()), true);
    record(&w, "cand");
    let shown = improvement_detail(&w, "cand").summary.state.revision;

    send_improvement(
        &mut w,
        local_desktop(),
        review_command(
            ImprovementCommand::MarkImprovementReviewed,
            "review",
            "cand",
            shown.0,
        ),
    )
    .unwrap();
    send_improvement(
        &mut w,
        web(),
        review_command(
            ImprovementCommand::DismissImprovement,
            "dismiss",
            "cand",
            shown.0,
        ),
    )
    .unwrap();
    w.rt.block_on(hold.reached(2));
    hold.release();
    w.settle();

    let review = improvement_update_checked(&w, "review");
    let dismiss = improvement_update_checked(&w, "dismiss");
    let (winner, loser) = match (
        is_ready(&review.operation.state),
        is_ready(&dismiss.operation.state),
    ) {
        (true, false) => (&review, &dismiss),
        (false, true) => (&dismiss, &review),
        other => panic!("exactly one update wins, got {other:?}"),
    };
    assert_eq!(improvement_failed(loser), ErrorCode::StaleRevision);
    let stored = stored_candidate(&w, "cand").unwrap();
    assert_eq!(
        stored.status,
        if winner.action == ImprovementAction::MarkReviewed {
            storage::improvement::ImprovementStatus::Reviewed
        } else {
            storage::improvement::ImprovementStatus::Dismissed
        }
    );
    assert_eq!(
        improvement_detail(&w, "cand").summary.state.revision,
        winner.revision.unwrap()
    );
}

fn improvement_update_checked(w: &World, name: &str) -> ImprovementUpdateView {
    improvement_update(w, name).unwrap()
}

#[test]
fn an_action_the_state_forbids_is_refused_with_the_shown_reason() {
    let mut w = setup(None, true);
    record(&w, "done");
    let shown = improvement_detail(&w, "done").summary.state.revision;
    send_improvement(
        &mut w,
        web(),
        review_command(
            ImprovementCommand::DismissImprovement,
            "close",
            "done",
            shown.0,
        ),
    )
    .unwrap();
    w.settle();
    let closed = improvement_update_checked(&w, "close");
    assert!(is_ready(&closed.operation.state));

    // A dismissed candidate cannot be dismissed again, nor triaged.
    let detail = improvement_detail(&w, "done");
    assert_eq!(detail.summary.state.status, ImprovementStatus::Dismissed);
    for (name, command, action, expected) in [
        (
            "again",
            ImprovementCommand::DismissImprovement as fn(_) -> _,
            ImprovementAction::Dismiss,
            DenialReason::AlreadyResolved,
        ),
        (
            "triage",
            ImprovementCommand::DismissImprovement as fn(_) -> _,
            ImprovementAction::Triage,
            DenialReason::NotApplicableInState,
        ),
    ] {
        let shown = detail
            .summary
            .state
            .actions
            .iter()
            .find(|a| a.action == action)
            .unwrap()
            .state;
        assert_eq!(shown, ActionState::Denied { reason: expected });
        let command = if action == ImprovementAction::Triage {
            triage_command(name, "done", detail.summary.state.revision.0)
        } else {
            review_command(command, name, "done", detail.summary.state.revision.0)
        };
        send_improvement(&mut w, web(), command).unwrap();
        w.settle();
        let refused = improvement_update_checked(&w, name);
        assert_eq!(refused.denial, Some(expected));
        assert_eq!(refused.revision, None);
    }
    assert_eq!(
        stored_candidate(&w, "done").unwrap().status,
        storage::improvement::ImprovementStatus::Dismissed
    );
    assert!(drafts(&w).is_empty(), "a refused triage leaves no draft");
}

#[test]
fn a_stale_improvement_revision_is_refused_and_asks_for_a_new_read() {
    let mut w = setup(None, true);
    record(&w, "stale");
    let shown = improvement_detail(&w, "stale").summary.state.revision;
    w.store
        .handle()
        .set_improvement_status("stale", storage::improvement::ImprovementStatus::Reviewed)
        .unwrap();
    send_improvement(
        &mut w,
        web(),
        review_command(
            ImprovementCommand::DismissImprovement,
            "late",
            "stale",
            shown.0,
        ),
    )
    .unwrap();
    w.settle();
    let view = improvement_update_checked(&w, "late");
    assert_eq!(improvement_failed(&view), ErrorCode::StaleRevision);
    assert!(view.message.as_deref().unwrap().contains("again"));
    assert_eq!(
        stored_candidate(&w, "stale").unwrap().status,
        storage::improvement::ImprovementStatus::Reviewed
    );
}

#[test]
fn a_candidate_that_is_evicted_after_the_project_check_is_not_found_from_the_writer() {
    let hold = Hold::new(UpdateStage::BeforeCommit);
    let mut w = setup(Some(hold.hook()), true);
    record_in(&w, SLUG, "old", 1);
    let shown = improvement_detail(&w, "old").summary.state.revision;
    send_improvement(
        &mut w,
        web(),
        review_command(
            ImprovementCommand::DismissImprovement,
            "evict",
            "old",
            shown.0,
        ),
    )
    .unwrap();
    w.rt.block_on(hold.reached(1));
    // The next candidate pushes the oldest one out (the project keeps one).
    record_in(&w, SLUG, "new", 1);
    assert!(stored_candidate(&w, "old").is_none());
    hold.release();
    w.settle();
    assert_eq!(
        improvement_failed(&improvement_update_checked(&w, "evict")),
        ErrorCode::NotFound
    );
}

#[test]
fn a_candidate_of_another_project_is_not_found_and_not_written() {
    let mut w = setup(None, true);
    record_in(&w, OTHER_SLUG, "foreign", 1000);
    let revision =
        workbench_host::areas::improvement::revision_of(&stored_candidate(&w, "foreign").unwrap());
    for (name, command) in [
        (
            "foreign-review",
            review_command(
                ImprovementCommand::MarkImprovementReviewed,
                "foreign-review",
                "foreign",
                revision.0,
            ),
        ),
        (
            "foreign-triage",
            triage_command("foreign-triage", "foreign", revision.0),
        ),
    ] {
        send_improvement(&mut w, web(), command).unwrap();
        w.settle();
        assert_eq!(
            improvement_failed(&improvement_update_checked(&w, name)),
            ErrorCode::NotFound
        );
    }
    let stored = stored_candidate(&w, "foreign").unwrap();
    assert_eq!(stored.status, storage::improvement::ImprovementStatus::New);
    assert_eq!(stored.draft_path, None);
    assert_eq!(
        workbench_host::areas::improvement::revision_of(&stored),
        revision
    );
    assert!(drafts(&w).is_empty(), "no draft was made for it");
}

#[test]
fn triage_saves_one_draft_under_a_name_the_core_makes_and_changes_nothing_else() {
    let mut w = setup(None, true);
    record(&w, "cand-1");
    let shown = improvement_detail(&w, "cand-1");
    let revision = shown.summary.state.revision;

    send_improvement(
        &mut w,
        web(),
        triage_command("triage-1", "cand-1", revision.0),
    )
    .unwrap();
    w.settle();
    let view = improvement_update_checked(&w, "triage-1");
    assert!(is_ready(&view.operation.state), "{view:?}");
    assert_eq!(view.draft_file.as_deref(), Some("cand-1.issue.md"));
    assert_eq!(drafts(&w), vec!["cand-1.issue.md".to_owned()]);

    let draft = std::fs::read_to_string(w.dir.path().join("drafts/cand-1.issue.md")).unwrap();
    assert!(draft.contains("title of cand-1"), "{draft}");

    // Only the draft was attached: the candidate was neither reviewed nor
    // dismissed, and nothing was sent anywhere.
    let stored = stored_candidate(&w, "cand-1").unwrap();
    assert_eq!(stored.status, storage::improvement::ImprovementStatus::New);
    assert_eq!(stored.draft_path.as_deref(), Some("cand-1.issue.md"));
    let after = improvement_detail(&w, "cand-1");
    assert_eq!(after.draft_file.as_deref(), Some("cand-1.issue.md"));
    assert_eq!(after.summary.state.revision, view.revision.unwrap());
}

#[test]
fn a_triage_resent_makes_no_second_draft_and_a_second_triage_changes_nothing() {
    let mut w = setup(None, true);
    record(&w, "twice");
    let revision = improvement_detail(&w, "twice").summary.state.revision;
    let command = triage_command("triage-a", "twice", revision.0);

    send_improvement(&mut w, web(), command.clone()).unwrap();
    w.settle();
    let after_first = improvement_detail(&w, "twice");
    let resent = send_improvement(&mut w, web(), command).unwrap();
    assert_eq!(resent.disposition, CommandDisposition::AlreadyApplied);
    assert!(is_ready(&improvement_view(&resent).operation.state));
    assert_eq!(drafts(&w).len(), 1);

    // Another operation at the revision that now stands finds the draft in place.
    send_improvement(
        &mut w,
        local_desktop(),
        triage_command("triage-b", "twice", after_first.summary.state.revision.0),
    )
    .unwrap();
    w.settle();
    let second = improvement_update_checked(&w, "triage-b");
    assert!(is_ready(&second.operation.state), "{second:?}");
    assert_eq!(second.changed, Some(false));
    assert_eq!(drafts(&w).len(), 1);
    assert_eq!(
        improvement_detail(&w, "twice").summary.state.revision,
        after_first.summary.state.revision
    );
}

#[test]
fn a_triage_that_the_writer_refuses_leaves_no_draft_behind() {
    let mut w = setup(None, true);
    record(&w, "refused");
    let shown = improvement_detail(&w, "refused").summary.state.revision;
    w.store
        .handle()
        .set_improvement_status("refused", storage::improvement::ImprovementStatus::Reviewed)
        .unwrap();
    send_improvement(
        &mut w,
        web(),
        triage_command("triage-stale", "refused", shown.0),
    )
    .unwrap();
    w.settle();
    let view = improvement_update_checked(&w, "triage-stale");
    assert_eq!(improvement_failed(&view), ErrorCode::StaleRevision);
    assert_eq!(view.draft_file, None);
    assert!(drafts(&w).is_empty());
    assert_eq!(stored_candidate(&w, "refused").unwrap().draft_path, None);
}

#[test]
fn triage_without_a_draft_directory_is_refused_and_makes_nothing() {
    let mut w = setup(None, false);
    record(&w, "off");
    let shown = improvement_detail(&w, "off").summary.state.revision;
    send_improvement(&mut w, web(), triage_command("triage-off", "off", shown.0)).unwrap();
    w.settle();
    assert_eq!(
        improvement_failed(&improvement_update_checked(&w, "triage-off")),
        ErrorCode::UnsupportedCapability
    );
    assert!(drafts(&w).is_empty());
}

#[test]
fn a_triage_names_no_path_and_the_core_makes_the_file_name_safe() {
    let json = serde_json::json!({
        "type": "triage_improvement",
        "payload": {
            "operation_id": "t",
            "project": "project-1",
            "subject": {"candidate": "c", "expected_revision": 1},
            "draft_path": "/etc/passwd"
        }
    });
    assert!(serde_json::from_value::<ImprovementCommand>(json).is_err());

    for hostile in ["../../etc/passwd", "a/b\\c", "x\0y", "dir/../../escape"] {
        let name = draft_file_name(hostile);
        assert!(
            !name.contains('/') && !name.contains('\\') && !name.contains('\0'),
            "{name}"
        );
        assert!(name.ends_with(".issue.md"), "{name}");
        assert!(Path::new(&name).components().count() == 1, "{name}");
    }
}

#[test]
fn a_triage_stopped_before_the_write_makes_no_draft_and_attaches_nothing() {
    let hold = Hold::new(UpdateStage::BeforeCommit);
    let mut w = setup(Some(hold.hook()), true);
    record(&w, "halt");
    let shown = improvement_detail(&w, "halt").summary.state.revision;
    send_improvement(
        &mut w,
        web(),
        triage_command("triage-halt", "halt", shown.0),
    )
    .unwrap();
    w.rt.block_on(hold.reached(1));
    w.tasks.request_stop();
    hold.release();
    w.settle();

    let view = improvement_update_checked(&w, "triage-halt");
    assert_eq!(
        view.operation.state,
        OperationState::Cancelled {
            reason: CancelReason::Shutdown
        }
    );
    assert_eq!(view.draft_file, None);
    assert!(drafts(&w).is_empty());
    assert_eq!(stored_candidate(&w, "halt").unwrap().draft_path, None);
}

#[test]
fn an_improvement_update_stopped_at_its_start_writes_nothing() {
    let hold = Hold::new(UpdateStage::Started);
    let mut w = setup(Some(hold.hook()), true);
    record(&w, "early");
    let shown = improvement_detail(&w, "early").summary.state.revision;
    send_improvement(
        &mut w,
        local_desktop(),
        review_command(
            ImprovementCommand::MarkImprovementReviewed,
            "early-op",
            "early",
            shown.0,
        ),
    )
    .unwrap();
    w.rt.block_on(hold.reached(1));
    w.tasks.request_stop();
    hold.release();
    w.settle();
    assert!(matches!(
        improvement_update_checked(&w, "early-op").operation.state,
        OperationState::Cancelled {
            reason: CancelReason::Shutdown
        }
    ));
    assert_eq!(
        stored_candidate(&w, "early").unwrap().status,
        storage::improvement::ImprovementStatus::New
    );
}

#[test]
fn the_completion_of_an_improvement_update_is_announced() {
    let mut w = setup(None, true);
    record(&w, "told");
    let shown = improvement_detail(&w, "told").summary.state.revision;
    let mut events = w
        .host
        .projection()
        .subscribe_with_snapshot(&Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .unwrap();
    send_improvement(
        &mut w,
        web(),
        review_command(
            ImprovementCommand::MarkImprovementReviewed,
            "told-op",
            "told",
            shown.0,
        ),
    )
    .unwrap();
    let announced = w.rt.block_on(async {
        loop {
            match events.recv().await.expect("the subscription is open").event {
                UiEvent::Improvement(ImprovementEvent::ImprovementUpdateCompleted { update })
                    if update.operation.operation_id.as_str() == "told-op" =>
                {
                    return update;
                }
                _ => {}
            }
        }
    });
    w.settle();
    assert_eq!(announced, improvement_update_checked(&w, "told-op"));
}

#[test]
fn an_unknown_operation_id_is_not_found() {
    let w = setup(None, true);
    assert_eq!(
        improvement_update(&w, "nope").unwrap_err().code,
        ErrorCode::NotFound
    );
    let _: OperationId = operation("unused");
    let _ = MemoryStatus::Candidate;
}
