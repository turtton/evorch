use workbench_client::restart::{confirmation::confirm, intent::*};
use workbench_protocol::{
    commands::Command,
    handshake::Capability,
    ids::{RestartId, RunId},
    namespaces::lifecycle::{
        AdminId, LifecycleCommand, RestartPhase,
        restart::{
            AdminAuditDiagnostics, InterruptBinding, RestartCommand, RestartManagementStatus,
            RestartStatus, RestartTarget, RestartTargets, ShellBlocker, ShellBlockers,
        },
    },
    wire::{InstanceEpoch, Revision},
};

fn id(text: &str) -> AdminId {
    AdminId::new(text).unwrap()
}

fn targets(truncated: bool) -> RestartTargets {
    RestartTargets {
        core_epoch: InstanceEpoch(7),
        revision: Revision(4),
        targets: vec![
            RestartTarget {
                id: id("current"),
                revision: Revision(11),
            },
            RestartTarget {
                id: id("next-build"),
                revision: Revision(12),
            },
        ],
        truncated,
    }
}

fn status(phase: RestartPhase, revision: u64) -> RestartManagementStatus {
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
    }
}

fn binding(phase: RestartPhase, revision: u64) -> InterruptBinding {
    InterruptBinding {
        restart_id: id("r1"),
        core_epoch: InstanceEpoch(7),
        connection_epoch: InstanceEpoch(1),
        run_incarnation: id("inc-1"),
        blocker_identity: id("blk-1"),
        revision: Revision(revision),
        phase,
    }
}

#[test]
fn the_target_is_current_unless_named_and_only_an_opaque_id_is_accepted() {
    assert_eq!(TargetSelector::default(), TargetSelector::Current);
    assert_eq!(
        TargetSelector::parse("current"),
        Ok(TargetSelector::Current)
    );
    assert_eq!(
        TargetSelector::parse("next-build"),
        Ok(TargetSelector::Id(id("next-build")))
    );
    for rejected in ["/usr/bin/evorch-core", "../x", "a b", "a=b", "", "x;y"] {
        assert!(TargetSelector::parse(rejected).is_err(), "{rejected:?}");
    }
}

#[test]
fn a_restart_is_bound_to_the_revision_and_epoch_that_were_listed() {
    let binding = plan_restart(&targets(false), &TargetSelector::Current, id("op-1")).unwrap();
    assert_eq!(binding.target_id, id("current"));
    assert_eq!(binding.target_revision, Revision(11));
    assert_eq!(binding.expected_core_epoch, InstanceEpoch(7));
    let named = plan_restart(
        &targets(false),
        &TargetSelector::parse("next-build").unwrap(),
        id("op-2"),
    )
    .unwrap();
    assert_eq!(named.target_revision, Revision(12));
}

#[test]
fn a_target_the_core_did_not_list_is_not_bound() {
    let unknown = TargetSelector::parse("gone").unwrap();
    assert_eq!(
        plan_restart(&targets(false), &unknown, id("op")),
        Err(IntentError::UnknownTarget("gone".into()))
    );
    assert_eq!(
        plan_restart(&targets(true), &unknown, id("op")),
        Err(IntentError::TargetsTruncated("gone".into()))
    );
}

#[test]
fn the_restart_command_carries_exactly_the_binding_and_a_confirmation() {
    let binding = plan_restart(&targets(false), &TargetSelector::Current, id("op-1")).unwrap();
    let proof = confirm(false, true, false).unwrap();
    let Command::Lifecycle(LifecycleCommand::Restart(RestartCommand::Request(request))) =
        request_restart(binding.clone(), proof)
    else {
        panic!("a restart request");
    };
    assert_eq!(request.binding, binding);
    assert!(request.confirmed);
}

#[test]
fn cancel_needs_a_restart_and_binds_its_epoch_and_revision() {
    let none = RestartManagementStatus {
        status: RestartStatus {
            restart_id: None,
            ..status(RestartPhase::Running, 1).status
        },
        ..status(RestartPhase::Running, 1)
    };
    assert_eq!(cancel_restart(&none, id("op")), Err(IntentError::NoRestart));
    let Command::Lifecycle(LifecycleCommand::Restart(RestartCommand::Cancel(cancel))) =
        cancel_restart(&status(RestartPhase::WaitingSafePoint, 9), id("op")).unwrap()
    else {
        panic!("a cancel");
    };
    assert_eq!(cancel.restart_id, id("r1"));
    assert_eq!(cancel.expected_core_epoch, InstanceEpoch(7));
    assert_eq!(cancel.expected_revision, Revision(9));
}

#[test]
fn an_interrupt_binding_is_checked_element_by_element_against_the_status() {
    let current = status(RestartPhase::WaitingSafePoint, 9);
    assert_eq!(
        stale_binding(&binding(RestartPhase::WaitingSafePoint, 9), &current),
        None
    );
    assert_eq!(
        stale_binding(&binding(RestartPhase::WaitingSafePoint, 8), &current),
        Some("revision")
    );
    assert_eq!(
        stale_binding(&binding(RestartPhase::Sealing, 9), &current),
        Some("phase")
    );
    let mut other_epoch = binding(RestartPhase::WaitingSafePoint, 9);
    other_epoch.core_epoch = InstanceEpoch(6);
    assert_eq!(stale_binding(&other_epoch, &current), Some("core epoch"));
    let mut other_restart = binding(RestartPhase::WaitingSafePoint, 9);
    other_restart.restart_id = id("r0");
    assert_eq!(stale_binding(&other_restart, &current), Some("restart"));
}

#[test]
fn a_blocker_is_picked_from_the_shown_list_and_the_binding_is_not_edited() {
    let shown = ShellBlockers {
        blockers: vec![
            ShellBlocker {
                run: RunId::new("run-a").unwrap(),
                teardown_confirmed: false,
                observed: false,
                binding: binding(RestartPhase::WaitingSafePoint, 9),
            },
            ShellBlocker {
                run: RunId::new("run-b").unwrap(),
                teardown_confirmed: false,
                observed: false,
                binding: InterruptBinding {
                    blocker_identity: id("blk-2"),
                    ..binding(RestartPhase::WaitingSafePoint, 9)
                },
            },
        ],
        total: Revision(2),
        omitted: Revision(0),
    };
    let by_index = select_blocker(&shown, &BlockerSelector::Index(1)).unwrap();
    let by_run = select_blocker(&shown, &BlockerSelector::Run("run-b".into())).unwrap();
    assert_eq!(by_index, by_run);
    assert_eq!(by_index.binding.blocker_identity, id("blk-2"));
    assert_eq!(
        select_blocker(&shown, &BlockerSelector::Index(2)),
        Err(IntentError::NoSuchBlocker("#2".into()))
    );
    assert_eq!(
        select_blocker(&shown, &BlockerSelector::Run("run-z".into())),
        Err(IntentError::NoSuchBlocker("run-z".into()))
    );
}

#[test]
fn only_phases_before_capture_can_be_cancelled() {
    for (phase, expected) in [
        (RestartPhase::Requested, true),
        (RestartPhase::WaitingSafePoint, true),
        (RestartPhase::Sealed, true),
        (RestartPhase::Capturing, false),
        (RestartPhase::Finalizing, false),
        (RestartPhase::FailedClosed, false),
        (RestartPhase::Running, false),
    ] {
        assert_eq!(cancellable(phase), expected, "{phase:?}");
    }
}

#[test]
fn safe_restart_is_a_capability_the_core_must_offer() {
    assert!(Capability::ALL.contains(&Capability::SafeRestart));
}
