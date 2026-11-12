//! Exact all-member Stop CAS; conflicts never count as a completed Stop.
use super::*;
type Expected = Vec<(ResumeIntentRecord, u64)>;
fn request(f: &Fixture) -> (Expected, Vec<ResumeIntentRecord>) {
    let mut expected = Vec::new();
    let mut stopped = Vec::new();
    for id in ["run-a", "run-b"] {
        let row = f.db().resume_intent(id).unwrap().unwrap();
        let revision = f.db().resume_intent_revision(id).unwrap().unwrap();
        let mut stop = row.clone();
        let mut payload: Value = serde_json::from_str(&row.payload).unwrap();
        payload["active"] = false.into();
        payload["resume_requested"] = false.into();
        payload["stop_reason"] = "operator_stopped".into();
        stop.payload = payload.to_string();
        expected.push((row, revision));
        stopped.push(stop);
    }
    (expected, stopped)
}
#[test]
fn quarantine_stop_is_atomic_nonconsuming_and_requires_exact_new_payload_and_revision() {
    let f = Fixture::new();
    let activate = f.seed(false);
    f.claim(&activate);
    let (expected, stopped) = request(&f);
    assert!(
        f.storage
            .handle()
            .stop_quarantined_recovery_intents("claim".into(), expected, stopped)
            .is_err()
    );
    f.storage
        .handle()
        .activate_recovery_claim(activate)
        .unwrap();
    let (expected, stopped) = request(&f);
    let claim = f.db().recovery_claims().unwrap();
    let snapshots = [
        f.db().run_context("run-a").unwrap(),
        f.db().run_context("run-b").unwrap(),
    ];
    f.storage
        .handle()
        .stop_quarantined_recovery_intents("claim".into(), expected.clone(), stopped.clone())
        .unwrap();
    assert_eq!(f.db().resume_intents().unwrap(), stopped);
    for (before, revision) in &expected {
        assert_eq!(
            f.db().resume_intent_revision(&before.run_id).unwrap(),
            Some(revision + 1)
        );
    }
    assert_eq!(f.db().recovery_claims().unwrap(), claim);
    assert_eq!(
        [
            f.db().run_context("run-a").unwrap(),
            f.db().run_context("run-b").unwrap()
        ],
        snapshots
    );
    let after = f.db().recovery_claim_snapshot("claim").unwrap();
    assert!(
        f.storage
            .handle()
            .stop_quarantined_recovery_intents("claim".into(), expected, stopped)
            .is_err()
    );
    assert_eq!(f.db().recovery_claim_snapshot("claim").unwrap(), after);
}
#[test]
fn quarantine_stop_rolls_back_every_member_on_payload_or_successor_conflict() {
    let f = Fixture::new();
    let activate = f.seed(false);
    f.claim(&activate);
    f.storage
        .handle()
        .activate_recovery_claim(activate)
        .unwrap();
    let (expected, mut stopped) = request(&f);
    let before = f.db().recovery_claim_snapshot("claim").unwrap();
    let mut value: Value = serde_json::from_str(&stopped[1].payload).unwrap();
    value["provider_input"] = json!({"prefix":["rewritten"]});
    stopped[1].payload = value.to_string();
    assert!(
        f.storage
            .handle()
            .stop_quarantined_recovery_intents("claim".into(), expected, stopped)
            .is_err()
    );
    assert_eq!(f.db().recovery_claim_snapshot("claim").unwrap(), before);
    let mut third = f.db().resume_intent("run-b").unwrap().unwrap();
    let old = third.incarnation.clone();
    let mut value: Value = serde_json::from_str(&third.payload).unwrap();
    value["incarnation"] = "third".into();
    third.incarnation = "third".into();
    third.payload = value.to_string();
    f.storage
        .handle()
        .save_resume_intent(third, Some(old))
        .unwrap();
    let before = f.db().recovery_claim_snapshot("claim").unwrap();
    let (expected, stopped) = request(&f);
    assert!(
        f.storage
            .handle()
            .stop_quarantined_recovery_intents("claim".into(), expected, stopped)
            .is_err()
    );
    assert_eq!(f.db().recovery_claim_snapshot("claim").unwrap(), before);
}
#[test]
fn quarantine_stop_writer_fault_does_not_leave_a_partial_stop() {
    let f = Fixture::new();
    let activate = f.seed(false);
    f.claim(&activate);
    f.storage
        .handle()
        .activate_recovery_claim(activate)
        .unwrap();
    let (expected, stopped) = request(&f);
    let before = f.db().recovery_claim_snapshot("claim").unwrap();
    f.sql().execute_batch("CREATE TRIGGER fail_quarantine BEFORE UPDATE ON resume_intents WHEN NEW.run_id='run-b' BEGIN SELECT RAISE(ABORT,'stop fault'); END;").unwrap();
    assert!(
        f.storage
            .handle()
            .stop_quarantined_recovery_intents("claim".into(), expected.clone(), stopped.clone())
            .is_err()
    );
    assert_eq!(f.db().recovery_claim_snapshot("claim").unwrap(), before);
    f.sql()
        .execute_batch("DROP TRIGGER fail_quarantine;")
        .unwrap();
    f.storage
        .handle()
        .stop_quarantined_recovery_intents("claim".into(), expected, stopped)
        .unwrap();
}
