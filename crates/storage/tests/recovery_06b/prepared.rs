//! Restart resolution: no timing assumptions or hand-written successful claims.
use super::*;

fn inspect(f: &Fixture) -> Result<ValidatedPreparedClaim, storage::StorageError> {
    let root = f._dir.path().join("ownership");
    let leases = storage::shared_with_wait(&f.config.db_path, &root, std::time::Duration::ZERO)?;
    let startup = leases.recovery_startup_complete(&f.config.db_path, &root)?;
    let retained = startup.try_clone()?;
    drop(startup);
    drop(leases);
    f.storage
        .handle()
        .validate_prepared_claim("claim".into(), &retained, &root)
}

fn rewrite(f: &Fixture, id: &str, change: impl FnOnce(&mut Value)) {
    let mut row = f.db().resume_intent(id).unwrap().unwrap();
    let mut payload = serde_json::from_str(&row.payload).unwrap();
    change(&mut payload);
    row.incarnation = payload["incarnation"].as_str().unwrap().into();
    row.payload = payload.to_string();
    f.storage
        .handle()
        .save_resume_intent(row, Some("old".into()))
        .unwrap();
}

#[test]
fn prepared_evidence_keeps_the_claim_incarnations_revisions_and_snapshot_expectations() {
    let f = Fixture::new();
    let request = f.seed(true);
    let claim = f.claim(&request);
    for _ in 0..2 {
        let validated = inspect(&f).unwrap();
        assert_eq!(validated.claim(), &claim);
        assert_eq!(validated.manifest().manifest_id, "component");
        assert_eq!(validated.snapshots(), request.snapshots);
        assert_eq!(validated.intents(), f.db().resume_intents().unwrap());
        validated.validate_database(&f.config.db_path).unwrap();
        let other = Fixture::new();
        assert!(validated.validate_database(&other.config.db_path).is_err());
        f.assert_prepared(&claim);
    }
    // Evidence is reusable for inspection, never another revision reservation.
    f.storage.handle().activate_recovery_claim(request).unwrap();
    assert!(inspect(&f).is_err());
}

#[test]
fn prepared_evidence_requires_matching_startup_leases() {
    let f = Fixture::new();
    let request = f.seed(false);
    f.claim(&request);
    let other = Fixture::new();
    let root = other._dir.path().join("ownership");
    let leases =
        storage::shared_with_wait(&other.config.db_path, &root, std::time::Duration::ZERO).unwrap();
    let startup = leases
        .recovery_startup_complete(&other.config.db_path, &root)
        .unwrap();
    let retained = startup.try_clone().unwrap();
    drop(startup);
    drop(leases);
    assert!(
        f.storage
            .handle()
            .validate_prepared_claim("claim".into(), &retained, &root)
            .is_err()
    );
}

#[test]
fn release_with_a_missing_snapshot_is_unexecuted_and_does_not_restore_or_consume_it() {
    let f = Fixture::new();
    let request = f.seed(false);
    f.claim(&request);
    f.sql()
        .execute_batch("PRAGMA foreign_keys=OFF; DELETE FROM run_contexts WHERE run_id='run-a'")
        .unwrap();
    assert!(inspect(&f).is_err());
    let before = f.db().resume_intents().unwrap();
    let released = f
        .storage
        .handle()
        .release_prepared_claim_if_unexecuted("claim".into(), "snapshot_unavailable".into())
        .unwrap();
    assert!(released.stopped_members.is_empty());
    assert!(matches!(
        released.claim.state,
        RecoveryClaimState::Released { .. }
    ));
    assert_eq!(f.db().resume_intents().unwrap(), before);
    assert!(f.db().run_context("run-a").unwrap().is_none());
    assert_eq!(f.db().resume_intent_revision("run-a").unwrap(), Some(1));
    assert!(inspect(&f).is_err());
}

#[test]
fn release_reports_explicit_stop_without_undoing_its_revision_or_payload() {
    for stop in ["operator_stopped", "cancelled"] {
        let f = Fixture::new();
        let request = f.seed(false);
        f.claim(&request);
        rewrite(&f, "run-a", |v| {
            v["active"] = json!(false);
            v["stop_reason"] = json!(stop);
        });
        assert!(inspect(&f).is_err());
        let stopped = f.db().resume_intent("run-a").unwrap();
        let revision = f.db().resume_intent_revision("run-a").unwrap();
        assert_eq!(revision, Some(2));
        let outcome = f
            .storage
            .handle()
            .release_prepared_claim_if_unexecuted("claim".into(), "stopped".into())
            .unwrap();
        assert_eq!(outcome.stopped_members, ["run-a"]);
        assert_eq!(f.db().resume_intent("run-a").unwrap(), stopped);
        assert_eq!(f.db().resume_intent_revision("run-a").unwrap(), revision);
    }
}

#[test]
fn general_revision_terminal_and_new_incarnation_conflicts_never_count_as_stopped() {
    for case in [
        "revision",
        "terminal",
        "active_stop",
        "cli_stop",
        "mixed",
        "all_new",
        "third",
    ] {
        let f = Fixture::new();
        let request = f.seed(false);
        let claim = f.claim(&request);
        rewrite(&f, "run-a", |v| match case {
            "revision" => v["resume_requested"] = json!(true),
            "terminal" => {
                v["active"] = json!(false);
                v["stop_reason"] = json!("terminal");
            }
            "active_stop" => v["stop_reason"] = json!("operator_stopped"),
            "cli_stop" => {
                v["active"] = json!(false);
                v["stop_reason"] = json!("cli_disconnected");
            }
            "mixed" | "all_new" => v["incarnation"] = json!("new-run-a"),
            "third" => {
                v["incarnation"] = json!("successor");
                v["active"] = json!(false);
                v["stop_reason"] = json!("operator_stopped");
            }
            _ => unreachable!(),
        });
        if case == "all_new" {
            rewrite(&f, "run-b", |v| v["incarnation"] = json!("new-run-b"));
        }
        let before = f.db().resume_intents().unwrap();
        assert!(inspect(&f).is_err(), "{case}");
        assert!(
            f.storage
                .handle()
                .release_prepared_claim_if_unexecuted("claim".into(), "conflict".into())
                .is_err(),
            "{case}"
        );
        assert_eq!(f.db().recovery_claims().unwrap(), [claim], "{case}");
        assert_eq!(f.db().resume_intents().unwrap(), before, "{case}");
    }
}

#[test]
fn missing_or_corrupt_members_and_terminal_endpoint_updates_do_not_release_peers() {
    for sql in [
        "DELETE FROM resume_intents WHERE run_id='run-b'",
        "UPDATE resume_intents SET payload='broken' WHERE run_id='run-b'",
        "UPDATE recovery_claim_members SET old_incarnation='wrong' WHERE run_id='run-a'",
        "UPDATE recovery_claim_members SET reserved_revision=3 WHERE run_id='run-a'",
        "DELETE FROM recovery_claim_members WHERE run_id='run-a'",
        "UPDATE recovery_claim_members SET new_incarnation='old' WHERE run_id='run-a'",
        "INSERT INTO recovery_claim_members(claim_id,run_id,old_incarnation,reserved_revision,new_incarnation) VALUES('claim','run-b','old',1,'new-b')",
        "UPDATE recovery_members SET record_json='broken' WHERE run_id='run-b'",
        "UPDATE resume_intents SET payload=json_set(payload,'$.active',false,'$.stop_reason','operator_stopped') WHERE run_id='run-b'",
    ] {
        let f = Fixture::new();
        let request = f.seed(true);
        f.claim(&request);
        f.sql().execute_batch(sql).unwrap();
        assert!(inspect(&f).is_err(), "{sql}");
        assert!(
            f.storage
                .handle()
                .release_prepared_claim_if_unexecuted("claim".into(), "unavailable".into())
                .is_err(),
            "{sql}"
        );
        let claims = f.db().recovery_claims().unwrap();
        assert_eq!(claims[0].state, RecoveryClaimState::Prepared, "{sql}");
        assert_eq!(f.db().resume_intent_revision("run-a").unwrap(), Some(1));
    }
}

#[test]
fn release_failure_rolls_back_and_remains_explicitly_resolvable() {
    let f = Fixture::new();
    let request = f.seed(false);
    let claim = f.claim(&request);
    f.sql()
        .execute_batch(
            "CREATE TRIGGER fail_release BEFORE UPDATE OF state ON recovery_claims \
         WHEN NEW.state='released' BEGIN SELECT RAISE(ABORT,'release fault'); END;",
        )
        .unwrap();
    assert!(
        f.storage
            .handle()
            .release_prepared_claim_if_unexecuted("claim".into(), "configuration_changed".into())
            .is_err()
    );
    f.assert_prepared(&claim);
    f.sql().execute_batch("DROP TRIGGER fail_release").unwrap();
    f.storage
        .handle()
        .release_prepared_claim_if_unexecuted("claim".into(), "configuration_changed".into())
        .unwrap();
    assert!(
        f.storage
            .handle()
            .release_recovery_claim("claim".into(), "again".into())
            .is_err()
    );
}

#[test]
fn released_manifest_is_explicitly_retired_even_if_old_evidence_were_restored() {
    let f = Fixture::new();
    let request = f.seed(false);
    f.claim(&request);
    f.storage
        .handle()
        .release_recovery_claim("claim".into(), "configuration_changed".into())
        .unwrap();
    // Corruption/repair must not let the original receipt make this manifest
    // claimable again; release is a terminal decision for this manifest.
    f.sql()
        .execute("UPDATE resume_intents SET revision=0", [])
        .unwrap();
    assert!(
        f.storage
            .handle()
            .claim_recovery_manifest(RecoveryClaimRequest {
                claim_id: "duplicate".into(),
                manifest_id: "component".into(),
                incarnations: request
                    .intents
                    .iter()
                    .map(|r| (r.run_id.clone(), r.incarnation.clone()))
                    .collect(),
            })
            .is_err()
    );
    assert_eq!(f.db().recovery_claims().unwrap().len(), 1);
}

#[test]
fn diagnostic_readback_tracks_activation_without_authorizing_replay_or_writing_intents() {
    let f = Fixture::new();
    let request = f.seed(true);
    let claim = f.claim(&request);
    let db = f.db();
    let prepared = db.recovery_claim_snapshot("claim").unwrap();
    assert_eq!(prepared.claim, claim);
    assert_eq!(prepared.intents.len(), 2);
    assert_eq!(prepared.intents[0].revision, Some(1));
    assert_eq!(prepared.intents[1].revision, Some(0));
    assert!(
        prepared
            .intents
            .iter()
            .all(|m| m.intent.as_ref().unwrap().incarnation == "old")
    );
    assert_eq!(db.recovery_claim_snapshot("claim").unwrap(), prepared);
    f.storage.handle().activate_recovery_claim(request).unwrap();
    let activated = db.recovery_claim_snapshot("claim").unwrap();
    assert_eq!(activated.claim.state, RecoveryClaimState::Activated);
    assert!(activated.manifest.superseded);
    assert_eq!(
        activated.intents[0].intent.as_ref().unwrap().incarnation,
        "new-run-a"
    );
    assert_eq!(activated.intents[0].revision, Some(2));
    assert_eq!(activated.intents[1], prepared.intents[1]);
    assert_eq!(db.recovery_claim_snapshot("claim").unwrap(), activated);
    assert!(inspect(&f).is_err());
    assert!(
        f.storage
            .handle()
            .release_recovery_claim("claim".into(), "not_unexecuted".into())
            .is_err()
    );
    assert_eq!(db.recovery_claim_snapshot("claim").unwrap(), activated);
}

#[test]
fn diagnostic_readback_keeps_mixed_and_missing_rows_visible_instead_of_resolving_them() {
    let f = Fixture::new();
    let request = f.seed(false);
    f.claim(&request);
    rewrite(&f, "run-a", |v| v["incarnation"] = json!("new-run-a"));
    f.sql().execute_batch(
        "DELETE FROM resume_intents WHERE run_id='run-b'; \
         INSERT INTO recovery_claim_members(claim_id,run_id,old_incarnation,reserved_revision,new_incarnation) \
         VALUES('claim','unexpected-member','old',1,'other');"
    ).unwrap();
    let before = f.db().resume_intents().unwrap();
    let snapshot = f.db().recovery_claim_snapshot("claim").unwrap();
    assert_eq!(snapshot.claim.state, RecoveryClaimState::Prepared);
    assert_eq!(snapshot.intents.len(), 3);
    assert_eq!(
        snapshot.intents[0].intent.as_ref().unwrap().incarnation,
        "new-run-a"
    );
    for missing in &snapshot.intents[1..] {
        assert!(missing.intent.is_none());
        assert!(missing.revision.is_none());
    }
    assert!(inspect(&f).is_err());
    assert_eq!(f.db().resume_intents().unwrap(), before);
}

#[test]
fn diagnostic_readback_keeps_released_and_stop_evidence_distinct() {
    let f = Fixture::new();
    let request = f.seed(false);
    f.claim(&request);
    f.storage
        .handle()
        .release_recovery_claim("claim".into(), "configuration_changed".into())
        .unwrap();
    let db = f.db();
    let released = db.recovery_claim_snapshot("claim").unwrap();
    rewrite(&f, "run-a", |v| {
        v["active"] = json!(false);
        v["stop_reason"] = json!("operator_stopped");
    });
    let stopped = db.recovery_claim_snapshot("claim").unwrap();
    assert_eq!(stopped.claim, released.claim);
    assert!(matches!(
        stopped.claim.state,
        RecoveryClaimState::Released { .. }
    ));
    assert_eq!(stopped.intents[0].revision, Some(2));
    assert_eq!(
        stopped.intents[0].intent.as_ref().unwrap().incarnation,
        "old"
    );
    assert_eq!(stopped.intents[1], released.intents[1]);
    assert_eq!(db.recovery_claim_snapshot("claim").unwrap(), stopped);
}
