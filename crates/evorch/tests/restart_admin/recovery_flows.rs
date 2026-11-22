//! Q1/Q2 from the management CLI: a change is sent once, bound to what the
//! core showed, only when confirmed and only when management is offered, and a
//! lost reply is read back by the same operation id.
use super::{fake_core::*, harness::*};
use evorch::cli::core_admin::{AdminCommand, GraphsOp, QuarantineOp};
use workbench_protocol::{
    ErrorCode, RetryAdvice, WorkbenchError,
    commands::Command,
    namespaces::lifecycle::{
        AdminOutcome, LifecycleCommand,
        contracts::GraphCas,
        owned_graphs::{
            ActionAvailability, ClaimView, DisposalCompletion, DisposalProgressView,
            GraphAuthorityProblem, GraphControlView, GraphEligibilityView, GraphProblemDetail,
            GraphReference, IncompleteDisposals, OperationLimitView, OwnedGraphBinding,
            OwnedGraphView, OwnedGraphsCommand, OwnerCas, OwnerView, OwnershipView,
            RestartDispositionView, RestartPhaseView, StepObservation, UndisposedGraphs,
        },
        quarantine::{
            QuarantineClaim, QuarantineCommand, QuarantineCompletion, QuarantineDetail,
            QuarantineEligibility, QuarantineItem, QuarantineReason,
        },
    },
    wire::{InstanceEpoch, Revision},
};

fn never() -> std::future::Pending<()> {
    std::future::pending()
}

fn owner() -> OwnerCas {
    OwnerCas {
        thread_ref: id("thread-a"),
        owner_identity: None,
        owner_generation: None,
    }
}

fn view(epoch: u64, resume: ActionAvailability) -> OwnedGraphView {
    OwnedGraphView {
        graph_ref: id("graph-1"),
        snapshot_revision: Revision(5),
        binding: OwnedGraphBinding {
            graph: GraphCas {
                graph_id: id("graph-1"),
                restart_tx_id: id("tx-1"),
                capture_id: id("capture-1"),
                expected_revision: Revision(5),
                expected_core_epoch: InstanceEpoch(epoch),
                owner_identity: None,
                owner_generation: None,
                operation_id: id("unassigned"),
            },
            owners: vec![owner()],
            expected_control_revision: Revision(2),
        },
        restart_phase: RestartPhaseView::Completed,
        disposition: RestartDispositionView::RetryPending,
        claim: ClaimView::Absent,
        ownership: OwnershipView::Absent,
        control: GraphControlView::ManualIdle,
        eligibility: GraphEligibilityView::AdoptedRetryPending,
        resume,
        release: ActionAvailability::Available,
        progress: DisposalProgressView {
            eligibility: StepObservation::NotStarted,
            prepared_release: StepObservation::NotStarted,
            owners: StepObservation::NotStarted,
            scratch: StepObservation::NotStarted,
            completion: DisposalCompletion::NotStarted,
        },
        owner_count: Revision(1),
        owner_refs: vec![OwnerView {
            binding: owner(),
            ownership: OwnershipView::Absent,
            cleanup: StepObservation::NotStarted,
        }],
        owner_refs_truncated: false,
        operation_limit: OperationLimitView::WithinLimit,
    }
}

fn quarantined(epoch: u64) -> QuarantineDetail {
    QuarantineDetail {
        item: QuarantineItem {
            graph_id: id("q-1"),
            revision: Revision(3),
            cold: true,
            restart_tx_id: id("tx-q"),
            capture_id: id("capture-q"),
            reason: QuarantineReason::ColdBoot,
            eligibility: QuarantineEligibility::Open,
            claim: QuarantineClaim::Absent,
            completion: QuarantineCompletion::NotStarted,
            members: 2,
        },
        core_epoch: InstanceEpoch(epoch),
    }
}

fn release(yes: bool) -> AdminCommand {
    AdminCommand::Graphs(GraphsOp::Release {
        graph: id("graph-1"),
        yes,
    })
}
fn resume(yes: bool) -> AdminCommand {
    AdminCommand::Graphs(GraphsOp::Resume {
        graph: id("graph-1"),
        yes,
    })
}
fn discard(yes: bool) -> AdminCommand {
    AdminCommand::Quarantine(QuarantineOp::Discard {
        graph: id("q-1"),
        yes,
    })
}

async fn finished(core: &FakeCore, outcome: AdminOutcome) {
    core.wait_for(|s| {
        s.seen.iter().any(|x| {
            matches!(
                x.body,
                workbench_protocol::envelope::RequestBody::Command(_)
            )
        })
    })
    .await;
    core.complete_operation(&the_operation(core), outcome);
}

#[tokio::test]
async fn a_graph_is_released_once_with_the_binding_it_was_shown_and_a_fresh_operation_id() {
    let env = env();
    let core = core(&env, 7);
    core.state().graph = Some(view(7, ActionAvailability::Available));
    let run = Run::new();
    let (code, _) = both(
        run.drive(&env, release(true), &NONE, never()),
        finished(&core, AdminOutcome::Succeeded),
    )
    .await;
    assert_eq!(code, 0, "{}", run.err.text());
    let commands = core.commands();
    assert_eq!(commands.len(), 1, "{commands:?}");
    let Command::Lifecycle(LifecycleCommand::OwnedGraphs(OwnedGraphsCommand::Release(sent))) =
        &commands[0]
    else {
        panic!("not a release: {commands:?}");
    };
    let shown = view(7, ActionAvailability::Available).binding;
    assert!(sent.confirmed);
    assert_ne!(sent.binding.graph.operation_id.as_str(), "unassigned");
    let mut same = sent.binding.clone();
    same.graph.operation_id = shown.graph.operation_id.clone();
    assert_eq!(same, shown, "everything but the operation id is as shown");
}

#[tokio::test]
async fn nothing_is_sent_without_a_confirmation() {
    for command in [release(false), resume(false), discard(false)] {
        let env = env();
        let core = core(&env, 7);
        core.state().graph = Some(view(7, ActionAvailability::Available));
        core.state().quarantine = Some(quarantined(7));
        let run = Run::new();
        let code = run.drive(&env, command, &NONE, never()).await;
        assert_eq!(code, 1);
        assert!(core.commands().is_empty(), "{:?}", core.commands());
        assert!(run.err.text().contains("nothing was sent"));
    }
}

#[tokio::test]
async fn a_terminal_that_says_no_sends_nothing() {
    let env = env();
    let core = core(&env, 7);
    core.state().graph = Some(view(7, ActionAvailability::Available));
    let run = Run::new();
    let terminal = Terminal::answering(false);
    let code = run.drive(&env, resume(false), &terminal, never()).await;
    assert_eq!(code, 1);
    assert_eq!(terminal.asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(core.commands().is_empty());
}

#[tokio::test]
async fn an_action_the_core_does_not_offer_is_not_sent() {
    let env = env();
    let core = core(&env, 7);
    core.state().graph = Some(view(
        7,
        ActionAvailability::Blocked {
            problem: GraphAuthorityProblem::CliContinuityRequired,
            detail: GraphProblemDetail::NoClaim,
        },
    ));
    let run = Run::new();
    let code = run.drive(&env, resume(true), &NONE, never()).await;
    assert_eq!(code, 1);
    assert!(core.commands().is_empty());
    assert!(
        run.out.text().contains("resume: blocked"),
        "{}",
        run.out.text()
    );
}

#[tokio::test]
async fn a_core_that_does_not_offer_management_is_not_asked_anything() {
    let env = env();
    let core = FakeCore::start(&env.socket, 7, capabilities(false));
    core.state().graph = Some(view(7, ActionAvailability::Available));
    let run = Run::new();
    for command in [
        release(true),
        discard(true),
        AdminCommand::Graphs(GraphsOp::List),
    ] {
        assert_eq!(run.drive(&env, command, &NONE, never()).await, 1);
    }
    assert!(
        run.err
            .text()
            .contains("does not offer recovery management")
    );
    let asked: Vec<_> = core
        .state()
        .seen
        .iter()
        .filter(|s| {
            !matches!(
                s.body,
                workbench_protocol::envelope::RequestBody::Control(_)
            )
        })
        .cloned()
        .collect();
    assert!(asked.is_empty(), "{asked:?}");
}

#[tokio::test]
async fn a_quarantined_graph_is_discarded_once_and_a_lost_reply_is_read_back_not_resent() {
    let env = env();
    let core = core(&env, 7);
    core.state().quarantine = Some(quarantined(7));
    core.state().on_request = OnRequest::DropConnection;
    let run = Run::new();
    let (code, _) = both(
        run.drive(&env, discard(true), &NONE, never()),
        finished(&core, AdminOutcome::Succeeded),
    )
    .await;
    assert_eq!(code, 0, "{}", run.err.text());
    assert!(
        run.err.text().contains("not sent again"),
        "{}",
        run.err.text()
    );
    let commands = core.commands();
    assert_eq!(commands.len(), 1, "{commands:?}");
    let Command::Lifecycle(LifecycleCommand::Quarantine(QuarantineCommand::Discard(sent))) =
        &commands[0]
    else {
        panic!("not a discard: {commands:?}");
    };
    assert_eq!(sent.binding.graph_id.as_str(), "q-1");
    assert_eq!(sent.binding.expected_revision, Revision(3));
    assert_eq!(sent.binding.expected_core_epoch, InstanceEpoch(7));
}

#[tokio::test]
async fn a_restart_refused_for_a_conflict_points_at_the_undisposed_graphs() {
    let env = env();
    let core = core(&env, 7);
    core.state().on_request = OnRequest::Reject(WorkbenchError::new(
        ErrorCode::Conflict,
        "undisposed graphs",
        RetryAdvice::Never,
    ));
    core.state().undisposed = Some(UndisposedGraphs {
        count: Revision(1),
        references: vec![GraphReference {
            graph_id: id("graph-1"),
            restart_tx_id: id("tx-1"),
            capture_id: id("capture-1"),
            revision: Revision(5),
        }],
        snapshot_revision: Revision(5),
        truncated: false,
        over_limit_count: Revision(0),
        incomplete_disposals: IncompleteDisposals::empty(),
    });
    let run = Run::new();
    assert_eq!(
        run.drive(&env, request(true, false), &NONE, never()).await,
        1
    );
    let err = run.err.text();
    assert!(err.contains("1 undisposed graph(s)"), "{err}");
    assert!(err.contains("evorch core graphs list"), "{err}");
}
