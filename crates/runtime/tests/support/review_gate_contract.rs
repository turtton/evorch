use super::*;

#[test]
fn empty_checklist_cannot_pass_finish_gate() {
    let mut fixture = Fixture::passing();
    fixture.criteria.as_mut().unwrap().checklist.clear();
    assert_eq!(
        fixture.evaluate(),
        GateVerdict::Reject(vec![GateRejection::CriteriaUnverified {
            head_sha: HEAD.into(),
        }])
    );
}

#[test]
fn missing_evidence_cannot_pass_finish_gate() {
    let mut fixture = Fixture::passing();
    fixture.criteria.as_mut().unwrap().checklist[0].evidence = None;
    assert_eq!(
        fixture.evaluate(),
        GateVerdict::Reject(vec![GateRejection::CriteriaUnmet {
            head_sha: HEAD.into(),
            ids: vec!["ac1".into()],
        }])
    );
}
