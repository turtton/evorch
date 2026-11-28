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
            OwnedGraphView, OwnedGraphsCommand, RestartDispositionView, RestartPhaseView,
            StepObservation, ThreadCas, UndisposedGraphs,
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

fn thread() -> ThreadCas {
    ThreadCas {
        thread_ref: id("thread-a"),
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
                operation_id: id("unassigned"),
            },
            threads: vec![thread()],
            expected_control_revision: Revision(2),
        },
        restart_phase: RestartPhaseView::Completed,
        disposition: RestartDispositionView::RetryPending,
        claim: ClaimView::Absent,
        control: GraphControlView::ManualIdle,
        eligibility: GraphEligibilityView::AdoptedRetryPending,
        resume,
        release: ActionAvailability::Available,
        progress: DisposalProgressView {
            eligibility: StepObservation::NotStarted,
            prepared_release: StepObservation::NotStarted,
            scratch: StepObservation::NotStarted,
            completion: DisposalCompletion::NotStarted,
        },
        thread_count: Revision(1),
        thread_refs: vec![thread()],
        thread_refs_truncated: false,
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

#[tokio::test]
async fn unsupported_graph_references_are_listed_without_becoming_restart_blockers_or_duplicates() {
    for include_live in [false, true] {
        let env = env();
        let core = core(&env, 7);
        let reference = |graph: &str, revision| GraphReference {
            graph_id: id(graph),
            restart_tx_id: id("tx-legacy"),
            capture_id: id("capture-legacy"),
            revision: Revision(revision),
        };
        let legacy = reference("graph-legacy", 7);
        let live = reference("graph-live", 9);
        let mut warnings = vec![legacy.clone()];
        if include_live {
            warnings.push(live.clone());
        }
        core.state().undisposed = Some(UndisposedGraphs {
            count: Revision(u64::from(include_live)),
            references: if include_live {
                vec![live.clone()]
            } else {
                vec![]
            },
            snapshot_revision: Revision(6),
            truncated: false,
            over_limit_count: Revision(0),
            incomplete_disposals: IncompleteDisposals {
                count: Revision(warnings.len() as u64),
                references: warnings.clone(),
                truncated: false,
            },
        });
        let run = Run::new();
        let code = run
            .drive(&env, AdminCommand::Graphs(GraphsOp::List), &NONE, never())
            .await;
        assert_eq!(code, 0, "{}", run.err.text());
        let output = run.out.text();
        for graph in warnings {
            let prefix = format!("  {} (", graph.graph_id.as_str());
            let listed: Vec<_> = output
                .lines()
                .filter(|line| line.starts_with(&prefix))
                .collect();
            assert_eq!(listed.len(), 1, "{output}");
            assert!(
                listed[0].contains(&format!("revision {})", graph.revision.get())),
                "{output}"
            );
        }
        assert!(
            output.starts_with(&format!("{} undisposed graph(s)", u64::from(include_live))),
            "{output}"
        );
        assert!(core.commands().is_empty(), "listing never mutates a graph");
    }
}

#[tokio::test]
async fn a_listed_unsupported_graph_id_can_be_inspected_confirmed_and_released() {
    let env = env();
    let core = core(&env, 7);
    let mut shown = view(7, ActionAvailability::Available);
    shown.graph_ref = id("graph-legacy");
    shown.binding.graph.graph_id = id("graph-legacy");
    core.state().graph = Some(shown.clone());
    core.state().undisposed = Some(UndisposedGraphs {
        count: Revision(0),
        references: vec![],
        snapshot_revision: Revision(5),
        truncated: false,
        over_limit_count: Revision(0),
        incomplete_disposals: IncompleteDisposals {
            count: Revision(1),
            references: vec![GraphReference {
                graph_id: id("graph-legacy"),
                restart_tx_id: id("tx-1"),
                capture_id: id("capture-1"),
                revision: Revision(5),
            }],
            truncated: false,
        },
    });
    let listing = Run::new();
    assert_eq!(
        listing
            .drive(&env, AdminCommand::Graphs(GraphsOp::List), &NONE, never())
            .await,
        0,
        "{}",
        listing.err.text()
    );
    // The operator copies the public id from the listing, as documented.
    let listed = listing
        .out
        .text()
        .lines()
        .find_map(|line| {
            let graph = line.trim().strip_suffix(", revision 5)")?;
            Some(graph.split_once(" (restart ")?.0.to_owned())
        })
        .expect("the unsupported graph is listed with its public id");
    assert_eq!(listed, "graph-legacy");

    let inspect = Run::new();
    let show = AdminCommand::Graphs(GraphsOp::Show { graph: id(&listed) });
    assert_eq!(inspect.drive(&env, show, &NONE, never()).await, 0);
    assert!(core.commands().is_empty(), "inspection never mutates");

    // Confirmation is the terminal's, not a flag: the same id is asked about once.
    let terminal = Terminal::answering(true);
    let released = Run::new();
    let release = AdminCommand::Graphs(GraphsOp::Release {
        graph: id(&listed),
        yes: false,
    });
    let (code, _) = both(
        released.drive(&env, release, &terminal, never()),
        finished(&core, AdminOutcome::Succeeded),
    )
    .await;
    assert_eq!(code, 0, "{}", released.err.text());
    assert_eq!(terminal.asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    let asked_for: Vec<_> = core
        .state()
        .seen
        .iter()
        .filter_map(|seen| match &seen.body {
            workbench_protocol::envelope::RequestBody::Query(
                workbench_protocol::queries::Query::Lifecycle(
                    workbench_protocol::namespaces::lifecycle::LifecycleQuery::OwnedGraphs(
                        workbench_protocol::namespaces::lifecycle::owned_graphs::OwnedGraphsQuery::Detail {
                            graph_ref,
                        },
                    ),
                ),
            ) => Some(graph_ref.as_str().to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(
        asked_for,
        [listed.as_str(), listed.as_str()],
        "inspect and release read the listed id"
    );
    let commands = core.commands();
    assert_eq!(commands.len(), 1, "{commands:?}");
    let Command::Lifecycle(LifecycleCommand::OwnedGraphs(OwnedGraphsCommand::Release(sent))) =
        &commands[0]
    else {
        panic!("not a release: {commands:?}");
    };
    assert!(sent.confirmed);
    assert_eq!(sent.binding.graph.graph_id.as_str(), listed);
}
