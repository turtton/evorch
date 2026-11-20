use crate::fixture::*;
use storage::{Database, recovery::quarantine::*};

#[test]
fn a_cold_quarantine_lists_with_its_reason_and_an_open_eligibility() {
    let (f, _) = Fixture::cold_quarantine(false);
    let snapshot = f.db().quarantine_snapshot(QUARANTINE_LIST_CAP).unwrap();
    assert!(!snapshot.truncated);
    let [graph] = snapshot.graphs.as_slice() else {
        panic!("one graph");
    };
    assert_eq!(
        (graph.restart_id.as_str(), graph.manifest_id.as_str()),
        (TX, MANIFEST)
    );
    assert!(graph.cold && graph.terminal);
    assert_eq!(graph.reason, QuarantineReason::ColdBoot);
    assert_eq!(graph.claim, QuarantineClaim::Absent);
    assert!(!graph.revoked);
    assert_eq!(graph.completion, QuarantineCompletion::NotStarted);
    assert_eq!(graph.members, 2);
    // The read-only connection the host uses sees the same graph.
    assert_eq!(
        Database::read_quarantine_snapshot(&f.config, QUARANTINE_LIST_CAP)
            .unwrap()
            .graphs,
        snapshot.graphs
    );
}

#[test]
fn the_list_is_bounded_and_says_when_it_is_cut() {
    let (f, _) = Fixture::cold_quarantine(false);
    let sql = f.sql();
    sql.execute(
        "INSERT INTO restart_transactions(restart_id,capture_id,revision,phase,metadata_json,old_builtin_tool_names_json) \
         SELECT 'other','capture-other',revision,'invalidated',metadata_json,old_builtin_tool_names_json \
         FROM restart_transactions WHERE restart_id='restart'",
        [],
    )
    .unwrap();
    sql.execute(
        "INSERT INTO restart_components(restart_id,manifest_id,disposition,reason) VALUES('other','component-2','quarantined','cold boot has no inherited proof')",
        [],
    )
    .unwrap();
    let all = f.db().quarantine_snapshot(8).unwrap();
    assert_eq!((all.graphs.len(), all.truncated), (2, false));
    let cut = f.db().quarantine_snapshot(1).unwrap();
    assert_eq!((cut.graphs.len(), cut.truncated), (1, true));
    // Newest first: the cut keeps the most recent component.
    assert_eq!(cut.graphs[0].manifest_id, "component-2");
}

#[test]
fn an_adopted_quarantine_is_listed_as_owned_not_cold() {
    let (f, _) = Fixture::cold_quarantine(false);
    f.sql()
        .execute(
            "UPDATE restart_transactions SET phase='adopted',adopted_epoch=2 WHERE restart_id='restart'",
            [],
        )
        .unwrap();
    let graph = f.graph();
    assert!(!graph.cold);
}

#[test]
fn only_quarantined_or_discarded_components_are_inspectable() {
    let f = Fixture::new();
    f.seed(false);
    f.transaction();
    assert!(f.db().quarantined_graph(TX, MANIFEST).unwrap().is_none());
    assert!(f.db().quarantined_graph(TX, "nope").unwrap().is_none());
}
