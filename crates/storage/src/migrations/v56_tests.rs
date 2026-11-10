//! Upgrade a v55 database that already holds exhausted budgets.
use super::apply_up_to;
use rusqlite::Connection;

#[test]
fn exhaustions_recorded_before_the_mark_are_not_notified_again() {
    let c = Connection::open_in_memory().unwrap();
    apply_up_to(&c, 55).unwrap();
    c.execute_batch(
        "INSERT INTO pr_auto_fix_state(pr_key) VALUES ('pr');
         INSERT INTO pr_auto_fix_limit_events(pr_key,budget_epoch,notice_id,at_ns) VALUES ('pr',0,'n0',77);",
    )
    .unwrap();
    apply_up_to(&c, 56).unwrap();
    c.execute_batch(
        "UPDATE pr_auto_fix_state SET budget_epoch=1;
         INSERT INTO pr_auto_fix_limit_events(pr_key,budget_epoch,notice_id,at_ns) VALUES ('pr',1,'n1',88);",
    )
    .unwrap();
    let marks = c
        .prepare("SELECT budget_epoch,notified_at_ns FROM pr_auto_fix_limit_events ORDER BY budget_epoch")
        .unwrap()
        .query_map([], |r| Ok((r.get::<_, u64>(0)?, r.get::<_, Option<i64>>(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(marks, vec![(0, Some(77)), (1, None)]);
}
