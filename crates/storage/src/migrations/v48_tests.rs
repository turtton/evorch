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

/// Empty-target reclaim probes must not scan unrelated lanes' accumulated history.
#[test]
fn phase2_reclaim_indexes_bound_unrelated_history_vm_steps() {
    let conn = Connection::open_in_memory().unwrap();
    apply_up_to(&conn, 48).unwrap();
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    for table in ["lane_closes", "lane_forward_receipts"] {
        let mut measurements = vec![];
        for rows in [10, 10_000] {
            conn.execute(&format!("DELETE FROM {table}"), []).unwrap();
            // FKs are disabled in this index-only fixture; the probe never returns these rows.
            let columns = if table == "lane_closes" {
                "close_id,lane_id,verification_id,payload"
            } else {
                "caller_root,operation_id,lane_id,question_id,request_json,result_json"
            };
            let values = if table == "lane_closes" {
                "printf('close-%d',x),'unrelated','v','{}'"
            } else {
                "'root',printf('op-%d',x),'unrelated','q','{}','{}'"
            };
            conn.execute_batch(&format!("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<{rows}) INSERT INTO {table}({columns}) SELECT {values} FROM n")).unwrap();
            let bytes = if table == "lane_closes" {
                "OCTET_LENGTH(payload)+COALESCE(OCTET_LENGTH(pr_evidence_json),0)"
            } else {
                "OCTET_LENGTH(request_json)+OCTET_LENGTH(result_json)"
            };
            let mut query=conn.prepare(&format!("SELECT COALESCE(SUM(bytes),0) FROM (SELECT {bytes} AS bytes FROM {table} WHERE lane_id='target' LIMIT 100)")).unwrap();
            assert_eq!(query.query_row([], |r| r.get::<_, u64>(0)).unwrap(), 0);
            let read = query.get_status(rusqlite::StatementStatus::VmStep);
            let mut delete=conn.prepare(&format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE lane_id='target' LIMIT 100)")).unwrap();
            assert_eq!(delete.execute([]).unwrap(), 0);
            measurements.push(read + delete.get_status(rusqlite::StatementStatus::VmStep));
        }
        assert!(measurements[0] > 0);
        assert!(
            measurements[1] <= measurements[0] * 2,
            "{table}: {measurements:?}"
        );
    }
}
