//! Upgrade a v56 database that holds a verified close.
use super::apply_up_to;
use rusqlite::Connection;

#[test]
fn a_verified_close_becomes_a_lead_close_that_says_what_was_not_recorded() {
    let c = Connection::open_in_memory().unwrap();
    // Rows here stand alone; the lanes they would reference are not under test.
    c.pragma_update(None, "foreign_keys", false).unwrap();
    apply_up_to(&c, 56).unwrap();
    c.execute_batch(
        r#"INSERT INTO lane_closes(close_id,lane_id,verification_id,payload)
           VALUES ('lc-1','lane-1','v-1','{"lane_id":"lane-1","close_id":"lc-1","verification_ref":"v-1","ready_seq":3,"closed_revision":5,"outcome":"closed"}');"#,
    )
    .unwrap();
    apply_up_to(&c, 57).unwrap();
    let (kind, verification, payload): (String, Option<String>, String) = c
        .query_row(
            "SELECT kind,verification_id,payload FROM lane_closes WHERE close_id='lc-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(kind, "lead_closed");
    assert_eq!(verification.as_deref(), Some("v-1"));
    let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(value["kind"], "lead_closed");
    assert_eq!(value["ready_seq"], 3);
    assert_eq!(value["closed_revision"], 5);
    // The closer and the observations were never recorded and are not invented.
    assert!(value.get("review").is_none());
    assert!(value.get("artifact_revision").is_none());
}

#[test]
fn the_rebuilt_table_keeps_history_immutable_and_takes_the_larger_payload() {
    let c = Connection::open_in_memory().unwrap();
    // Rows here stand alone; the lanes they would reference are not under test.
    c.pragma_update(None, "foreign_keys", false).unwrap();
    apply_up_to(&c, 57).unwrap();
    let big = "x".repeat(30_000);
    c.execute(
        "INSERT INTO lane_closes(close_id,lane_id,kind,payload) VALUES ('a','lane-1','human_closed',?1)",
        [&big],
    )
    .unwrap();
    assert!(
        c.execute(
            "INSERT INTO lane_closes(close_id,lane_id,kind,payload) VALUES ('b','lane-1','lead_closed',?1)",
            ["x".repeat(32_769)],
        )
        .is_err()
    );
    assert!(
        c.execute(
            "INSERT INTO lane_closes(close_id,lane_id,kind,payload) VALUES ('c','lane-1','verified_closed','{}')",
            [],
        )
        .is_err()
    );
    assert!(
        c.execute("UPDATE lane_closes SET payload='{}'", [])
            .is_err()
    );
    // A close without a Reviewer has no verification reference.
    c.execute(
        "INSERT INTO lane_closes(close_id,lane_id,kind,payload) VALUES ('d','lane-1','discarded','{}')",
        [],
    )
    .unwrap();
}
