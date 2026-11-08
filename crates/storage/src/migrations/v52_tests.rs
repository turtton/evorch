//! Upgrade a populated v51 lane ledger; no downgrade/migration compatibility layer.
use super::apply_up_to;
use rusqlite::Connection;
#[test]
fn phase3_backfills_independent_membership_and_keeps_history() {
    let c = Connection::open_in_memory().unwrap();
    c.pragma_update(None, "foreign_keys", true).unwrap();
    apply_up_to(&c, 51).unwrap();
    for (id, policy, lead, revision) in [
        ("attached", "attached", Some("lead"), 3),
        ("detached", "detached", None, 5),
    ] {
        c.execute("INSERT INTO lanes(lane_id,thread_id,origin_thread_id,origin_run_id,creation_key,request_hash,scope,lead_thread_id,policy,management_revision,brief_json,brief_revision,start_seed,created_at_ns,updated_at_ns,last_activity_at_ns) VALUES (?1,?1,'lead','lead-root',?1,'hash','lane',?2,?3,?4,'{}',1,'seed',1,1,1)",rusqlite::params![id,lead,policy,revision]).unwrap();
    }
    c.execute_batch("INSERT INTO lane_reports(lane_id,report_seq,status,summary,artifact_refs_json,needs_human,question_refs_json,producer,management_revision,brief_revision,created_at_ns) VALUES ('attached',1,'ready','unchanged','[]',0,'[]','lane_agent',3,1,1)").unwrap();
    apply_up_to(&c, 52).unwrap();
    apply_up_to(&c, 52).unwrap();
    let rows=c.prepare("SELECT lane_id,management_revision,detached_since_management_revision,management_projection_revision FROM lanes ORDER BY lane_id").unwrap().query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,u64>(1)?,r.get::<_,Option<u64>>(2)?,r.get::<_,u64>(3)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
    assert_eq!(
        rows,
        vec![
            ("attached".into(), 3, None, 0),
            ("detached".into(), 5, Some(5), 0)
        ]
    );
    assert_eq!(
        c.query_row("SELECT summary FROM lane_reports", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "unchanged"
    );
    assert_eq!(
        c.query_row("SELECT family_topology_revision FROM lane_state", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
}
