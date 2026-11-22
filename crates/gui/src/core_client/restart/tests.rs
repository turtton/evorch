use super::panel::*;
use std::time::{Duration, Instant};
use workbench_client::{
    ClientError,
    restart::{intent::BlockerSelector, reconnect::Stage, view::ReconnectView},
};
use workbench_protocol::{
    ErrorCode, RetryAdvice, WorkbenchError,
    commands::Command,
    ids::{RestartId, RunId},
    namespaces::lifecycle::{
        AdminId, AdminOperationState, AdminOutcome, AdminReceipt, LifecycleCommand, LifecycleReply,
        RestartPhase,
        restart::{
            AdminAuditDiagnostics, InterruptBinding, RestartCommand, RestartManagementStatus,
            RestartReply, RestartStatus, RestartTarget, RestartTargets, ShellBlocker,
            ShellBlockers,
        },
    },
    queries::QueryReply,
    wire::{InstanceEpoch, Revision},
};

fn id(text: &str) -> AdminId {
    AdminId::new(text).unwrap()
}

fn targets(epoch: u64, revision: u64) -> QueryReply {
    QueryReply::Lifecycle(LifecycleReply::Restart(RestartReply::Targets(
        RestartTargets {
            core_epoch: InstanceEpoch(epoch),
            revision: Revision(1),
            targets: vec![RestartTarget {
                id: id("current"),
                revision: Revision(revision),
            }],
            truncated: false,
        },
    )))
}

fn status(phase: RestartPhase, revision: u64) -> QueryReply {
    QueryReply::Lifecycle(LifecycleReply::Restart(RestartReply::Status(
        RestartManagementStatus {
            audit: AdminAuditDiagnostics::default(),
            core_epoch: InstanceEpoch(7),
            status: RestartStatus {
                restart_id: Some(RestartId::new("r1").unwrap()),
                phase,
                revision,
                blockers: vec![],
                blocker_total: 0,
                blocker_omitted: 0,
                active_producers: 0,
            },
        },
    )))
}

fn blockers(revision: u64) -> QueryReply {
    QueryReply::Lifecycle(LifecycleReply::Restart(RestartReply::ShellBlockers(
        ShellBlockers {
            blockers: vec![ShellBlocker {
                run: RunId::new("run-a").unwrap(),
                teardown_confirmed: false,
                observed: false,
                binding: InterruptBinding {
                    restart_id: id("r1"),
                    core_epoch: InstanceEpoch(7),
                    connection_epoch: InstanceEpoch(1),
                    run_incarnation: id("inc"),
                    blocker_identity: id("blk"),
                    revision: Revision(revision),
                    phase: RestartPhase::WaitingSafePoint,
                },
            }],
            total: Revision(1),
            omitted: Revision(0),
        },
    )))
}

fn ready_panel() -> RestartPanel {
    let mut panel = RestartPanel::default();
    panel.on_reply(&targets(7, 11), Instant::now());
    panel
}

fn as_restart(request: PanelRequest) -> RestartCommand {
    let PanelRequest::Command(Command::Lifecycle(LifecycleCommand::Restart(command))) = request
    else {
        panic!("a restart command: {request:?}");
    };
    command
}

#[test]
fn nothing_is_sent_without_an_open_confirmation_and_it_is_single_use() {
    let mut panel = ready_panel();
    assert_eq!(panel.accept(id("i-1")), None);
    panel.begin_restart(id("op-1")).unwrap();
    let RestartCommand::Request(sent) = as_restart(panel.accept(id("i-1")).unwrap()) else {
        panic!("a request");
    };
    assert_eq!(sent.binding.target_revision, Revision(11));
    assert!(sent.confirmed);
    assert_eq!(panel.accept(id("i-2")), None, "one dialog, one command");
}

#[test]
fn a_changed_target_or_epoch_drops_the_dialog_instead_of_running_it() {
    for changed in [targets(7, 12), targets(8, 11)] {
        let mut panel = ready_panel();
        panel.begin_restart(id("op-1")).unwrap();
        panel.on_reply(&changed, Instant::now());
        assert!(panel.confirming().is_none());
        assert_eq!(panel.accept(id("i")), None);
        assert!(
            panel
                .notices()
                .iter()
                .any(|n| n.contains("確認を取り消しました"))
        );
    }
}

#[test]
fn an_unchanged_refresh_keeps_the_dialog() {
    let mut panel = ready_panel();
    panel.begin_restart(id("op-1")).unwrap();
    panel.on_reply(&targets(7, 11), Instant::now());
    assert!(panel.confirming().is_some());
}

#[test]
fn an_interrupt_is_confirmed_against_the_listed_binding_and_dropped_when_it_goes_stale() {
    let mut panel = RestartPanel::default();
    let now = Instant::now();
    panel.on_reply(&status(RestartPhase::WaitingSafePoint, 9), now);
    panel.on_reply(&blockers(8), now);
    assert!(
        panel.begin_interrupt(&BlockerSelector::Index(0)).is_err(),
        "stale at once"
    );

    panel.on_reply(&blockers(9), now);
    panel.begin_interrupt(&BlockerSelector::Index(0)).unwrap();
    // The phase moved on: the dialog goes, the binding is not refreshed.
    panel.on_reply(&status(RestartPhase::Sealing, 10), now);
    assert!(panel.confirming().is_none());
    assert_eq!(panel.accept(id("i")), None);

    panel.on_reply(&status(RestartPhase::WaitingSafePoint, 9), now);
    panel.begin_interrupt(&BlockerSelector::Index(0)).unwrap();
    let RestartCommand::Interrupt(sent) = as_restart(panel.accept(id("i-9")).unwrap()) else {
        panic!("an interrupt");
    };
    assert_eq!(sent.binding.revision, Revision(9));
    assert_eq!(sent.operation_id, id("i-9"));
}

#[test]
fn after_a_restart_only_reads_are_asked_never_a_command() {
    let mut panel = ready_panel();
    panel.begin_restart(id("op-1")).unwrap();
    let _ = panel.accept(id("i"));
    let queries = panel.reconcile_queries();
    assert_eq!(queries.len(), 1);
    assert!(queries.iter().all(|q| matches!(q, PanelRequest::Query(_))));
    // The core says it completed: nothing is left to ask about.
    panel.on_reply(
        &QueryReply::Lifecycle(LifecycleReply::OperationStatus(AdminReceipt {
            operation_id: id("op-1"),
            state: AdminOperationState::Completed,
            outcome: AdminOutcome::Succeeded,
            audit_warning: true,
        })),
        Instant::now(),
    );
    assert!(panel.reconcile_queries().is_empty());
    assert!(panel.notices().iter().any(|n| n.contains("監査記録")));
}

#[test]
fn a_new_session_starts_from_nothing() {
    let mut panel = ready_panel();
    panel.begin_restart(id("op-1")).unwrap();
    panel.on_reply(&status(RestartPhase::Finalizing, 3), Instant::now());
    assert!(panel.planned_restart_evidence().is_some());
    panel.on_new_session();
    assert!(panel.confirming().is_none());
    assert!(panel.status().is_none());
    assert!(panel.planned_restart_evidence().is_none());
    assert_eq!(panel.accept(id("i")), None);
}

#[test]
fn a_restart_is_only_evidence_once_it_is_past_capture() {
    let mut panel = RestartPanel::default();
    let now = Instant::now();
    panel.on_reply(&status(RestartPhase::WaitingSafePoint, 1), now);
    assert!(panel.planned_restart_evidence().is_none());
    panel.on_reply(&status(RestartPhase::Finalizing, 2), now);
    assert!(panel.planned_restart_evidence().is_some());
}

#[test]
fn cancel_is_offered_only_before_capture() {
    let mut panel = RestartPanel::default();
    let now = Instant::now();
    panel.on_reply(&status(RestartPhase::WaitingSafePoint, 1), now);
    assert!(matches!(
        as_restart(panel.cancel(id("c-1")).unwrap()),
        RestartCommand::Cancel(_)
    ));
    panel.on_reply(&status(RestartPhase::Capturing, 2), now);
    assert_eq!(panel.cancel(id("c-2")), None);
}

#[test]
fn a_conflict_reads_as_a_changed_target_and_the_banner_shows_stage_and_deadline() {
    let mut panel = RestartPanel::default();
    panel.on_failure(&ClientError::Server(WorkbenchError::new(
        ErrorCode::Conflict,
        "changed",
        RetryAdvice::Never,
    )));
    assert!(panel.notices()[0].contains("競合"));
    assert!(panel.notices()[0].contains("何も変更していません"));

    let view = ReconnectView {
        stage: Stage::RestartingToReady,
        remaining: Duration::from_secs(97),
    };
    let banner = panel.banner(Some(view), None);
    let text = banner.text().unwrap();
    assert!(text.contains("120秒中 残り97秒"), "{text}");
    assert!(text.contains("新しいコアは起動しません"), "{text}");
    let expired = panel.banner(Some(view), Some(Stage::ReadyToReconnect));
    assert!(expired.text().unwrap().contains("30秒以内"));
    assert_eq!(panel.banner(None, None).text(), None);
}

#[test]
fn only_an_accepted_request_that_has_not_ended_is_a_planned_restart() {
    let now = Instant::now();
    let epoch = InstanceEpoch(7);
    let mut panel = ready_panel();
    assert!(
        panel.planned_restart(epoch, now).is_none(),
        "EOF alone is no evidence"
    );
    panel.begin_restart(id("op-1")).unwrap();
    assert!(
        panel.planned_restart(epoch, now).is_none(),
        "an open dialog is not a request"
    );
    panel.accept(id("i-1")).unwrap();
    let evidence = panel
        .planned_restart(epoch, now)
        .expect("the core was asked to restart");
    assert_eq!(evidence.predecessor_epoch, epoch);
    panel.on_reply(
        &QueryReply::Lifecycle(LifecycleReply::OperationStatus(AdminReceipt {
            operation_id: id("op-1"),
            state: AdminOperationState::Completed,
            outcome: AdminOutcome::Cancelled,
            audit_warning: false,
        })),
        now,
    );
    assert!(
        panel.planned_restart(epoch, now).is_none(),
        "a restart seen to end is no longer one that is under way"
    );
}

#[test]
fn a_refused_restart_asks_once_which_graphs_are_undisposed_and_says_what_to_do() {
    use workbench_protocol::namespaces::lifecycle::owned_graphs::{
        IncompleteDisposals, UndisposedGraphs,
    };
    let mut panel = ready_panel();
    let conflict = ClientError::Server(WorkbenchError::new(
        ErrorCode::Conflict,
        "undisposed",
        RetryAdvice::Never,
    ));
    panel.on_request_failure(true, &conflict, false);
    assert!(panel.take_undisposed_query().is_some());
    assert!(panel.take_undisposed_query().is_none(), "asked once");
    panel.on_reply(
        &QueryReply::Lifecycle(LifecycleReply::OwnedGraphs(
            workbench_protocol::namespaces::lifecycle::owned_graphs::OwnedGraphsReply::Undisposed(
                UndisposedGraphs {
                    count: Revision(2),
                    references: vec![],
                    snapshot_revision: Revision(1),
                    truncated: true,
                    over_limit_count: Revision(0),
                    incomplete_disposals: IncompleteDisposals::empty(),
                },
            ),
        )),
        Instant::now(),
    );
    assert_eq!(panel.undisposed(), Some(2));
    assert!(
        panel
            .notices()
            .iter()
            .any(|n| n.contains("2 件") && n.contains("evorch core graphs list")),
        "{:?}",
        panel.notices()
    );
}

#[test]
fn a_lost_reply_is_shown_as_unknown_and_not_as_a_failure_to_retry() {
    let mut panel = ready_panel();
    let unknown = ClientError::Server(WorkbenchError::new(
        ErrorCode::Internal,
        "x",
        RetryAdvice::Never,
    ));
    panel.on_request_failure(true, &unknown, true);
    assert!(panel.notices().iter().any(|n| n.contains("結果が不明")));
    assert!(panel.take_undisposed_query().is_none());
}
