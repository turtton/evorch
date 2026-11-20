use crate::fixture::*;
use storage::recovery::{graph_control::ActivationControl, quarantine::*, *};

fn discard(f: &Fixture, request: DiscardRequest) -> DiscardReport {
    f.handle().discard_quarantined_graph(request).unwrap()
}

#[test]
fn discard_revokes_eligibility_permanently_without_a_claim() {
    let (mut f, _) = Fixture::cold_quarantine_threadless(false);
    let before = f.graph();
    let request = f.request("op-1");
    let result = discard(&f, request).unwrap();
    assert_eq!(result.change, DiscardChange::Revoked);
    assert_eq!(result.claim_release, ClaimRelease::NotNeeded);
    assert!(result.graph.revoked);
    assert_eq!(result.graph.completion, QuarantineCompletion::Complete);
    assert!(result.graph.revision > before.revision);
    let control = f.db().recovery_graph_control(MANIFEST).unwrap().unwrap();
    assert!(control.revoked);
    assert_eq!(control.mode, "revoked_idle");
    assert_eq!(control.origin, "manual_release_without_claim");

    // A process restart changes nothing: the revoked row is durable and permanent.
    f.reload();
    assert_eq!(f.graph(), result.graph);
    let update = f
        .sql()
        .execute(
            "UPDATE recovery_graph_control SET revoked=0 WHERE manifest_id=?1",
            [MANIFEST],
        )
        .unwrap_err()
        .to_string();
    assert!(update.contains("cannot be restored"), "{update}");
}

#[test]
fn discard_releases_a_prepared_claim_and_blocks_automatic_activation_after_reload() {
    let (mut f, activation) = Fixture::cold_quarantine_threadless(true);
    assert_eq!(f.graph().claim, QuarantineClaim::Prepared);
    let result = discard(&f, f.request("op-1")).unwrap();
    assert_eq!(result.change, DiscardChange::Revoked);
    assert_eq!(result.claim_release, ClaimRelease::Released);
    assert_eq!(result.graph.claim, QuarantineClaim::Released);
    assert_eq!(result.graph.completion, QuarantineCompletion::Complete);
    // The reason the evidence was set aside is history; a discard does not rewrite it.
    assert_eq!(result.graph.reason, QuarantineReason::ColdBoot);
    assert!(matches!(
        f.db().recovery_claims().unwrap()[0].state,
        RecoveryClaimState::Released { .. }
    ));
    let control = f.db().recovery_graph_control(MANIFEST).unwrap().unwrap();
    assert_eq!(control.origin, "manual_release");

    f.reload();
    // The discarded graph is still visible, still revoked, and cannot activate.
    assert_eq!(f.graph(), result.graph);
    assert!(matches!(
        activation.reservation,
        ActivationControl::Automatic
    ));
    assert!(f.handle().activate_recovery_claim(activation).is_err());
}

#[test]
fn a_stale_revision_is_refused_and_writes_nothing() {
    let (f, _) = Fixture::cold_quarantine(false);
    let mut request = f.request("op-1");
    request.expected_revision -= 1;
    assert_eq!(discard(&f, request), Err(DiscardRefusal::Conflict));
    let mut request = f.request("op-1");
    request.capture_id = "other-capture".into();
    assert_eq!(discard(&f, request), Err(DiscardRefusal::Conflict));
    assert_eq!((f.control_rows(), f.effect_rows()), (0, 0));
    assert!(!f.graph().revoked);
}

#[test]
fn a_revision_that_advanced_since_inspection_is_refused() {
    let (f, _) = Fixture::cold_quarantine(false);
    let request = f.request("op-1");
    f.sql()
        .execute(
            "UPDATE restart_components SET revision=revision+1 WHERE manifest_id=?1",
            [MANIFEST],
        )
        .unwrap();
    assert_eq!(discard(&f, request), Err(DiscardRefusal::Conflict));
    assert_eq!((f.control_rows(), f.effect_rows()), (0, 0));
}

#[test]
fn repeating_an_operation_replays_without_another_write() {
    let (f, _) = Fixture::cold_quarantine(true);
    let request = f.request("op-1");
    let first = discard(&f, request.clone()).unwrap();
    let settled = (f.control_rows(), f.effect_rows(), f.graph());
    // The same operation carries the same stale revision: it still replays.
    let again = discard(&f, request.clone()).unwrap();
    assert_eq!(again.change, DiscardChange::Replayed);
    assert_eq!(again.graph, first.graph);
    let mut retry = request.clone();
    retry.attempt_nonce = "another-attempt".into();
    assert_eq!(discard(&f, retry).unwrap().change, DiscardChange::Replayed);
    assert_eq!(settled, (f.control_rows(), f.effect_rows(), f.graph()));
}

#[test]
fn one_operation_id_cannot_carry_a_different_request() {
    let (f, _) = Fixture::cold_quarantine(false);
    discard(&f, f.request("op-1")).unwrap();
    let mut other = f.request("op-1");
    other.expected_revision += 1;
    assert_eq!(discard(&f, other), Err(DiscardRefusal::OperationConflict));
}

#[test]
fn a_new_operation_on_a_revoked_graph_changes_nothing_and_reports_the_state() {
    let (f, _) = Fixture::cold_quarantine(false);
    discard(&f, f.request("op-1")).unwrap();
    let settled = f.graph();
    let result = discard(&f, f.request("op-2")).unwrap();
    assert_eq!(result.change, DiscardChange::AlreadyRevoked);
    assert_eq!(result.graph, settled);
    assert_eq!(f.control_rows(), 1);
    let control = f.db().recovery_graph_control(MANIFEST).unwrap().unwrap();
    assert_eq!(control.effect.operation_id, "op-1");
}

#[test]
fn an_unfinished_claim_release_leaves_the_revocation_partial_until_it_completes() {
    let (f, _) = Fixture::cold_quarantine_threadless(true);
    // A member intent moved on: the claim cannot be released without losing that fence.
    f.sql()
        .execute(
            "UPDATE resume_intents SET revision=revision+1 WHERE run_id='run-a'",
            [],
        )
        .unwrap();
    let result = discard(&f, f.request("op-1")).unwrap();
    assert_eq!(result.change, DiscardChange::Revoked);
    assert_eq!(result.claim_release, ClaimRelease::Failed);
    assert!(result.graph.revoked);
    assert_eq!(result.graph.claim, QuarantineClaim::Prepared);
    assert_eq!(result.graph.completion, QuarantineCompletion::Partial);

    f.sql()
        .execute(
            "UPDATE resume_intents SET revision=revision-1 WHERE run_id='run-a'",
            [],
        )
        .unwrap();
    let finished = discard(&f, f.request("op-2")).unwrap();
    assert_eq!(finished.change, DiscardChange::AlreadyRevoked);
    assert_eq!(finished.claim_release, ClaimRelease::Released);
    assert_eq!(finished.graph.completion, QuarantineCompletion::Complete);
}

#[test]
fn a_discard_is_one_atomic_transaction() {
    let (f, _) = Fixture::cold_quarantine(true);
    f.sql()
        .execute_batch(
            "CREATE TRIGGER fail_effect BEFORE INSERT ON recovery_graph_effects \
             BEGIN SELECT RAISE(ABORT,'injected'); END;",
        )
        .unwrap();
    let before = f.graph();
    assert!(
        f.handle()
            .discard_quarantined_graph(f.request("op-1"))
            .is_err()
    );
    assert_eq!((f.control_rows(), f.effect_rows()), (0, 0));
    assert_eq!(f.graph(), before);
    assert_eq!(f.graph().claim, QuarantineClaim::Prepared);
}

#[test]
fn an_adopted_or_running_graph_is_not_a_cold_discard() {
    let (f, _) = Fixture::cold_quarantine(false);
    let request = f.request("op-1");
    f.sql()
        .execute(
            "UPDATE restart_transactions SET adopted_epoch=2 WHERE restart_id='restart'",
            [],
        )
        .unwrap();
    let request = DiscardRequest {
        expected_revision: f.graph().revision,
        ..request
    };
    assert_eq!(discard(&f, request), Err(DiscardRefusal::NotCold));

    let (f, _) = Fixture::cold_quarantine(false);
    f.sql()
        .execute(
            "UPDATE restart_transactions SET phase='armed' WHERE restart_id='restart'",
            [],
        )
        .unwrap();
    assert_eq!(
        discard(&f, f.request("op-1")),
        Err(DiscardRefusal::NotTerminal)
    );
    assert_eq!((f.control_rows(), f.effect_rows()), (0, 0));
}

#[test]
fn a_manual_resume_holding_the_graph_blocks_the_discard() {
    let (f, _) = Fixture::cold_quarantine(false);
    f.sql()
        .execute(
            "INSERT INTO recovery_graph_control(manifest_id,mode,revision,operation_id,attempt_nonce,revoked,origin) \
             VALUES(?1,'manual_idle',1,'resume','n',0,'manual_resume')",
            [MANIFEST],
        )
        .unwrap();
    assert_eq!(
        discard(&f, f.request("op-1")),
        Err(DiscardRefusal::ManualInProgress)
    );
    assert_eq!(f.effect_rows(), 0);
}

#[test]
fn an_unknown_graph_is_not_found() {
    let (f, _) = Fixture::cold_quarantine(false);
    let mut request = f.request("op-1");
    request.manifest_id = "missing".into();
    assert_eq!(discard(&f, request), Err(DiscardRefusal::NotFound));
}

#[test]
fn history_and_run_snapshots_are_untouched_by_a_discard() {
    let (f, _) = Fixture::cold_quarantine(false);
    let snapshots = [
        f.db().run_context("run-a").unwrap(),
        f.db().run_context("run-b").unwrap(),
    ];
    let intents = f.db().resume_intents().unwrap();
    discard(&f, f.request("op-1")).unwrap();
    assert_eq!(
        snapshots,
        [
            f.db().run_context("run-a").unwrap(),
            f.db().run_context("run-b").unwrap()
        ]
    );
    assert_eq!(intents, f.db().resume_intents().unwrap());
}

#[test]
fn recorded_progress_that_is_not_finished_keeps_the_discard_partial() {
    let (f, _) = Fixture::cold_quarantine_threadless(false);
    discard(&f, f.request("op-1")).unwrap();
    let sql = f.sql();
    for (steps, expected) in [
        (("applied", "not_started"), QuarantineCompletion::Partial),
        (("not_applicable", "unknown"), QuarantineCompletion::Unknown),
        (("applied", "applied"), QuarantineCompletion::Complete),
    ] {
        sql.execute("DELETE FROM recovery_graph_thread_progress", [])
            .unwrap();
        sql.execute(
            "INSERT INTO recovery_graph_thread_progress(disposal_key,manifest_id,restart_id,capture_id,thread_id,owner_generation,revision,operation_id,attempt_nonce,owners,scratch) \
             VALUES('k',?1,?2,?3,'run-a',1,1,'op-1','nonce-op-1',?4,?5)",
            rusqlite::params![MANIFEST, TX, CAPTURE, steps.0, steps.1],
        )
        .unwrap();
        assert_eq!(f.graph().completion, expected, "{steps:?}");
    }
}

#[test]
fn no_disposal_record_is_not_evidence_that_nothing_remains() {
    // The members belong to threads whose owner and scratch this leaf can
    // neither see nor clean, and nothing has recorded that they were handled.
    let (f, _) = Fixture::cold_quarantine(false);
    let result = discard(&f, f.request("op-1")).unwrap();
    assert_eq!(result.change, DiscardChange::Revoked);
    assert!(result.graph.revoked);
    assert_eq!(result.graph.completion, QuarantineCompletion::Partial);

    // One thread recorded, the other still unknown to the record: still not complete.
    let control = f.db().recovery_graph_control(MANIFEST).unwrap().unwrap();
    f.handle()
        .write_recovery_graph_progress(
            storage::recovery::graph_progress::GraphThreadProgressWrite {
                key: storage::recovery::graph_progress::GraphDisposalProgressKey {
                    manifest_id: MANIFEST.into(),
                    restart_id: TX.into(),
                    capture_id: CAPTURE.into(),
                    thread_id: "run-a".into(),
                    owner_generation: 1,
                },
                expected_revision: 0,
                control,
                owners: storage::recovery::graph_progress::GraphDisposalStep::NotApplicable,
                scratch: storage::recovery::graph_progress::GraphDisposalStep::Applied,
            },
        )
        .unwrap();
    assert_eq!(f.graph().completion, QuarantineCompletion::Partial);
}

#[test]
fn a_graph_is_complete_only_when_every_thread_has_a_finished_record() {
    use storage::recovery::graph_progress::GraphDisposalStep::*;
    let (f, _) = Fixture::cold_quarantine(false);
    discard(&f, f.request("op-1")).unwrap();
    f.record_cleanup(NotApplicable, Applied);
    let done = f.graph();
    assert_eq!(done.completion, QuarantineCompletion::Complete);
    // A later request on the revoked graph reports that same finished state.
    let again = discard(&f, f.request("op-2")).unwrap();
    assert_eq!(again.change, DiscardChange::AlreadyRevoked);
    assert_eq!(again.graph.completion, QuarantineCompletion::Complete);
}

#[test]
fn an_unknown_disposal_step_or_unreadable_membership_is_unknown_not_complete() {
    use storage::recovery::graph_progress::GraphDisposalStep::*;
    let (f, _) = Fixture::cold_quarantine(false);
    discard(&f, f.request("op-1")).unwrap();
    f.record_cleanup(Unknown, Applied);
    assert_eq!(f.graph().completion, QuarantineCompletion::Unknown);

    let (f, _) = Fixture::cold_quarantine(false);
    discard(&f, f.request("op-1")).unwrap();
    f.record_cleanup(NotApplicable, NotApplicable);
    assert_eq!(f.graph().completion, QuarantineCompletion::Complete);
    f.sql()
        .execute(
            "UPDATE restart_expected_members SET member_json='{' WHERE run_id='run-a'",
            [],
        )
        .unwrap();
    assert_eq!(f.graph().completion, QuarantineCompletion::Unknown);
}
