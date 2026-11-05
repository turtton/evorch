//! Upgrade a populated v47 ledger, not only an empty database.
use super::apply_up_to;
use rusqlite::Connection;

#[test]
fn phase2_preserves_reports_and_starts_separate_monotonic_counters() {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    apply_up_to(&conn, 47).unwrap();
    conn.execute_batch(
        "INSERT INTO lanes(lane_id,thread_id,origin_thread_id,origin_run_id,creation_key,
          scope,lead_thread_id,policy,management_revision,brief_json,brief_revision,request_hash,
          start_seed,generation,latest_report_seq,created_at_ns,updated_at_ns,last_activity_at_ns)
         VALUES ('lane','thread','lead','lead-root','open','lane','lead','attached',1,
          '{}',1,'fixture','seed',0,0,1,1,1);
         INSERT INTO lane_reports(lane_id,report_seq,status,summary,artifact_refs_json,
          needs_human,question_refs_json,producer,management_revision,brief_revision,created_at_ns)
         VALUES ('lane',1,'working','old report','[]',0,'[]','lane_agent',1,1,1);
         UPDATE lanes SET latest_report_seq=1 WHERE lane_id='lane';",
    )
    .unwrap();
    apply_up_to(&conn, 48).unwrap();
    apply_up_to(&conn, 48).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT request_revision,progress_epoch,current_close_id FROM lanes",
            [],
            |r| Ok((
                r.get::<_, u64>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, Option<String>>(2)?
            ))
        )
        .unwrap(),
        (0, 0, None)
    );
    assert_eq!(
        conn.query_row("SELECT summary FROM lane_reports", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "old report"
    );
    conn.execute("UPDATE lanes SET brief_revision=2", [])
        .unwrap();
    assert_eq!(
        conn.query_row("SELECT request_revision FROM lanes", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        1
    );
    assert!(
        conn.execute("UPDATE lanes SET request_revision=0", [])
            .is_err()
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM lane_stall_episodes", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
}
