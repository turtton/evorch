use super::{fake_core::*, harness::*};
use evorch::cli::core_admin::AdminCommand;
use workbench_client::restart::intent::BlockerSelector;
use workbench_protocol::{
    ErrorCode, RetryAdvice, WorkbenchError,
    envelope::RequestBody,
    ids::{RestartId, RunId},
    namespaces::lifecycle::{
        AdminOutcome, LifecycleQuery, RestartPhase,
        restart::{InterruptBinding, RestartCommand, ShellBlocker, ShellBlockers},
    },
    queries::Query,
    wire::{InstanceEpoch, Revision},
};

fn never() -> std::future::Pending<()> {
    std::future::pending()
}

fn operation_reads(core: &FakeCore) -> Vec<String> {
    core.state()
        .seen
        .iter()
        .filter_map(|s| match &s.body {
            RequestBody::Query(Query::Lifecycle(LifecycleQuery::OperationStatus {
                operation_id,
            })) => Some(operation_id.as_str().to_owned()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_lost_ack_is_read_back_by_the_same_operation_id_and_never_resent() {
    let env = env();
    let core = core(&env, 7);
    core.state().on_request = OnRequest::DropConnection;
    let run = Run::new();
    let (code, _) = both(
        run.drive(&env, request(true, false), &NONE, never()),
        async {
            core.wait_for(|s| {
                !s.seen.is_empty()
                    && s.seen
                        .iter()
                        .any(|x| matches!(x.body, RequestBody::Command(_)))
            })
            .await;
            let op = the_operation(&core);
            core.complete_operation(&op, AdminOutcome::Succeeded);
        },
    )
    .await;
    assert_eq!(code, 0, "{}", run.err.text());
    assert!(
        run.err.text().contains("was not sent again"),
        "{}",
        run.err.text()
    );
    assert_eq!(
        core.restart_commands().len(),
        1,
        "the request was never resent"
    );
    let op = the_operation(&core);
    assert_eq!(
        operation_reads(&core),
        vec![op.clone(), op],
        "read back by the same id"
    );
}

#[tokio::test]
async fn a_lost_ack_for_an_operation_the_core_does_not_know_is_unknown_not_success() {
    let env = env();
    let core = core(&env, 7);
    core.state().on_request = OnRequest::DropConnection;
    let run = Run::new();
    let code = run.drive(&env, request(true, false), &NONE, never()).await;
    assert_eq!(code, 1);
    assert!(
        run.err.text().contains("does not know operation"),
        "{}",
        run.err.text()
    );
    assert_eq!(core.restart_commands().len(), 1);
}

#[tokio::test]
async fn a_planned_restart_is_followed_to_a_new_epoch_from_a_snapshot_not_the_old_cursor() {
    let env = env();
    let a = core(&env, 7);
    let run = Run::new();
    let (code, driven) = both(
        run.drive(&env, request(true, false), &NONE, never()),
        async {
            a.wait_for(|s| {
                s.seen
                    .iter()
                    .any(|x| matches!(x.body, RequestBody::Command(_)))
            })
            .await;
            let op = the_operation(&a);
            a.set_phase(RestartPhase::Finalizing);
            // The CLI has read the committed phase: now the core can go.
            a.wait_for(|s| s.served_status.contains(&RestartPhase::Finalizing))
                .await;
            let commands = a.restart_commands();
            a.stop_by_ref();
            let b = core(&env, 8);
            b.complete_operation(&op, AdminOutcome::Succeeded);
            (b, commands)
        },
    )
    .await;
    let (b, a_commands) = driven.expect("the scenario completed");
    assert_eq!(code, 0, "{}", run.err.text());
    assert_eq!(a_commands.len(), 1);
    assert!(
        b.restart_commands().is_empty(),
        "nothing was resent to the successor"
    );
    let subscribes = b.state().subscribes.clone();
    assert!(!subscribes.is_empty());
    for (_, after, snapshot) in subscribes {
        assert_eq!(after, None, "the old cursor is dropped");
        assert!(snapshot, "a snapshot is read on the new epoch");
    }
    assert!(
        run.out.text().contains("epoch 7 to 8"),
        "{}",
        run.out.text()
    );
    assert!(
        run.err.text().contains("no new core is started"),
        "{}",
        run.err.text()
    );
}

#[tokio::test]
async fn a_successor_that_replays_instead_of_snapshotting_is_refused() {
    let env = env();
    let a = core(&env, 7);
    let run = Run::new();
    let (code, _b) = both(
        run.drive(&env, request(true, false), &NONE, never()),
        async {
            a.wait_for(|s| {
                s.seen
                    .iter()
                    .any(|x| matches!(x.body, RequestBody::Command(_)))
            })
            .await;
            let op = the_operation(&a);
            a.set_phase(RestartPhase::Finalizing);
            a.wait_for(|s| s.served_status.contains(&RestartPhase::Finalizing))
                .await;
            a.stop_by_ref();
            let b = core(&env, 8);
            b.state().subscribe_mode = SubscribeMode::AlwaysReplay;
            b.complete_operation(&op, AdminOutcome::Succeeded);
            b
        },
    )
    .await;
    assert_eq!(code, 1);
    assert!(
        run.err
            .text()
            .contains("replayed where a snapshot was required"),
        "{}",
        run.err.text()
    );
}

#[tokio::test]
async fn a_lost_connection_without_a_restart_in_progress_is_not_waited_for() {
    let env = env();
    let a = core(&env, 7);
    let run = Run::new();
    let (code, _) = both(
        run.drive(&env, request(true, false), &NONE, never()),
        async {
            a.wait_for(|s| {
                s.seen
                    .iter()
                    .any(|x| matches!(x.body, RequestBody::Command(_)))
            })
            .await;
            // Still Running: the core says nothing about a restart, then vanishes.
            // Once for the confirmation screen, once after the command: the CLI has
            // seen the status of the accepted request, and it is still Running.
            a.wait_for(|s| s.served_status.len() >= 2).await;
            a.stop_by_ref();
        },
    )
    .await;
    assert_eq!(code, 1);
    assert!(
        run.err.text().contains("not treated as a planned restart"),
        "{}",
        run.err.text()
    );
    assert!(!env.socket.exists(), "no core was started in its place");
}

#[tokio::test]
async fn with_no_core_running_the_cli_exits_1_and_starts_nothing() {
    let env = env();
    for command in [
        AdminCommand::Status,
        AdminCommand::Targets,
        request(true, false),
    ] {
        let run = Run::new();
        let code = run.drive(&env, command, &NONE, never()).await;
        assert_eq!(code, 1);
        assert!(
            run.err.text().contains("no evorch core is running"),
            "{}",
            run.err.text()
        );
    }
    assert!(!env.socket.exists());
    let leftovers: Vec<_> = std::fs::read_dir(env.config.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "no lock or launch file: {leftovers:?}"
    );
}

#[tokio::test]
async fn ctrl_c_ends_only_the_wait_and_exits_130() {
    let env = env();
    let a = core(&env, 7);
    let run = Run::new();
    let (interrupt, fire) = tokio::sync::oneshot::channel::<()>();
    let (code, _) = both(
        run.drive(&env, request(true, false), &NONE, async {
            let _ = fire.await;
        }),
        async {
            // The CLI is waiting once it read the operation after its command.
            a.wait_for(|s| {
                let mut after_command = false;
                let mut reads = 0;
                for x in &s.seen {
                    match &x.body {
                        RequestBody::Command(_) => after_command = true,
                        RequestBody::Query(Query::Lifecycle(LifecycleQuery::OperationStatus {
                            ..
                        })) if after_command => reads += 1,
                        _ => {}
                    }
                }
                reads >= 1
            })
            .await;
            let _ = interrupt.send(());
        },
    )
    .await;
    assert_eq!(code, 130);
    assert!(
        run.err.text().contains("Nothing was cancelled or stopped"),
        "{}",
        run.err.text()
    );
    let commands = a.restart_commands();
    assert_eq!(
        commands.len(),
        1,
        "no cancel, interrupt or second request: {commands:?}"
    );
    assert!(matches!(commands[0], RestartCommand::Request(_)));
    assert_eq!(a.state().phase, RestartPhase::Running);
}

#[tokio::test]
async fn ctrl_c_while_waiting_for_the_successor_exits_130_and_starts_no_core() {
    let env = env();
    let a = core(&env, 7);
    let run = Run::new();
    let (interrupt, fire) = tokio::sync::oneshot::channel::<()>();
    let (code, _) = both(
        run.drive(&env, request(true, false), &NONE, async {
            let _ = fire.await;
        }),
        async {
            a.wait_for(|s| {
                s.seen
                    .iter()
                    .any(|x| matches!(x.body, RequestBody::Command(_)))
            })
            .await;
            a.set_phase(RestartPhase::Finalizing);
            a.wait_for(|s| s.served_status.contains(&RestartPhase::Finalizing))
                .await;
            a.stop_by_ref();
            run.err.wait_contains("no new core is started").await;
            let _ = interrupt.send(());
        },
    )
    .await;
    assert_eq!(code, 130);
    assert!(!env.socket.exists(), "the CLI started nothing");
}

#[tokio::test(start_paused = true)]
async fn the_wait_for_the_successor_shows_its_deadline_and_ends_at_120_seconds() {
    let env = env();
    let a = core(&env, 7);
    let run = Run::new();
    let (code, _) = both(
        run.drive(&env, request(true, false), &NONE, never()),
        async {
            a.wait_for(|s| {
                s.seen
                    .iter()
                    .any(|x| matches!(x.body, RequestBody::Command(_)))
            })
            .await;
            a.set_phase(RestartPhase::Finalizing);
            a.wait_for(|s| s.served_status.contains(&RestartPhase::Finalizing))
                .await;
            a.stop_by_ref();
        },
    )
    .await;
    assert_eq!(code, 1);
    let err = run.err.text();
    assert!(err.contains("s left of 120s"), "{err}");
    assert!(
        err.contains("did not accept connections within 120 seconds"),
        "{err}"
    );
    assert!(err.contains("not resent"), "{err}");
    assert!(!env.socket.exists(), "no core was started or killed");
}

#[tokio::test]
async fn restart_cannot_be_sent_without_a_confirmation() {
    let env = env();
    let a = core(&env, 7);
    // No flag, no terminal.
    let run = Run::new();
    assert_eq!(
        run.drive(&env, request(false, true), &NONE, never()).await,
        1
    );
    assert!(
        run.err.text().contains("needs your confirmation"),
        "{}",
        run.err.text()
    );
    // A terminal whose person says no.
    let no = Terminal::answering(false);
    let run = Run::new();
    assert_eq!(run.drive(&env, request(false, true), &no, never()).await, 1);
    assert_eq!(no.asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        a.commands().is_empty(),
        "nothing was sent: {:?}",
        a.commands()
    );
    // A terminal whose person says yes.
    let yes = Terminal::answering(true);
    let run = Run::new();
    assert_eq!(
        run.drive(&env, request(false, true), &yes, never()).await,
        0
    );
    assert_eq!(a.restart_commands().len(), 1);
}

fn blocker(revision: u64) -> ShellBlockers {
    ShellBlockers {
        blockers: vec![ShellBlocker {
            run: RunId::new("run-a").unwrap(),
            teardown_confirmed: false,
            observed: false,
            binding: InterruptBinding {
                restart_id: id("restart-1"),
                core_epoch: InstanceEpoch(7),
                connection_epoch: InstanceEpoch(1),
                run_incarnation: id("inc-1"),
                blocker_identity: id("blk-1"),
                revision: Revision(revision),
                phase: RestartPhase::WaitingSafePoint,
            },
        }],
        total: Revision(1),
        omitted: Revision(0),
    }
}

fn waiting(core: &FakeCore, blockers_revision: u64) {
    let mut state = core.state();
    state.phase = RestartPhase::WaitingSafePoint;
    state.revision = 9;
    state.restart_id = Some(RestartId::new("restart-1").unwrap());
    state.blockers = Some(blocker(blockers_revision));
}

fn interrupt(yes: bool) -> AdminCommand {
    AdminCommand::Interrupt {
        blocker: BlockerSelector::Index(0),
        yes,
    }
}

#[tokio::test]
async fn interrupt_needs_a_confirmation_and_sends_the_listed_binding_untouched() {
    let env = env();
    let a = core(&env, 7);
    waiting(&a, 9);
    let run = Run::new();
    assert_eq!(run.drive(&env, interrupt(false), &NONE, never()).await, 1);
    assert!(a.commands().is_empty());
    let run = Run::new();
    assert_eq!(run.drive(&env, interrupt(true), &NONE, never()).await, 0);
    let commands = a.restart_commands();
    let [RestartCommand::Interrupt(sent)] = commands.as_slice() else {
        panic!("one interrupt: {commands:?}");
    };
    assert_eq!(sent.binding, blocker(9).blockers[0].binding);
    assert!(sent.confirmed);
}

#[tokio::test]
async fn an_interrupt_target_that_changed_since_it_was_listed_is_refused_before_sending() {
    let env = env();
    let a = core(&env, 7);
    waiting(&a, 8);
    let run = Run::new();
    assert_eq!(run.drive(&env, interrupt(true), &NONE, never()).await, 1);
    assert!(run.err.text().contains("stale"), "{}", run.err.text());
    assert!(a.commands().is_empty());
}

#[tokio::test]
async fn a_conflict_from_the_core_is_shown_as_a_changed_target_and_nothing_follows() {
    let env = env();
    let a = core(&env, 7);
    a.state().on_request = OnRequest::Reject(WorkbenchError::new(
        ErrorCode::Conflict,
        "target revision changed",
        RetryAdvice::Never,
    ));
    let run = Run::new();
    assert_eq!(
        run.drive(&env, request(true, false), &NONE, never()).await,
        1
    );
    let err = run.err.text();
    assert!(err.contains("conflict"), "{err}");
    assert!(err.contains("nothing was changed"), "{err}");
    assert_eq!(a.restart_commands().len(), 1, "the conflict is not retried");
}

#[tokio::test]
async fn an_audit_failure_is_shown_with_the_receipt() {
    let env = env();
    let a = core(&env, 7);
    a.state().audit_warning = true;
    let run = Run::new();
    assert_eq!(
        run.drive(&env, request(true, true), &NONE, never()).await,
        0
    );
    assert!(
        run.out.text().contains("audit record"),
        "{}",
        run.out.text()
    );
}

#[tokio::test]
async fn a_core_without_safe_restart_is_refused_before_anything_is_sent() {
    let env = env();
    let _a = FakeCore::start(&env.socket, 7, capabilities(false));
    let run = Run::new();
    assert_eq!(
        run.drive(&env, request(true, true), &NONE, never()).await,
        1
    );
    assert!(
        run.err.text().contains("does not offer safe restart"),
        "{}",
        run.err.text()
    );
    assert!(_a.commands().is_empty());
}

#[tokio::test]
async fn cancel_is_only_sent_before_capture() {
    let env = env();
    let a = core(&env, 7);
    waiting(&a, 9);
    let run = Run::new();
    assert_eq!(
        run.drive(&env, AdminCommand::Cancel, &NONE, never()).await,
        0
    );
    assert!(matches!(
        a.restart_commands().as_slice(),
        [RestartCommand::Cancel(_)]
    ));

    let b_env = super::harness::env();
    let b = core(&b_env, 7);
    b.state().phase = RestartPhase::Capturing;
    b.state().restart_id = Some(RestartId::new("restart-1").unwrap());
    let run = Run::new();
    assert_eq!(
        run.drive(&b_env, AdminCommand::Cancel, &NONE, never())
            .await,
        1
    );
    assert!(
        run.err.text().contains("cannot be cancelled"),
        "{}",
        run.err.text()
    );
    assert!(b.commands().is_empty());
}
