//! Migration preserves root fences and isolates legacy progress, even with bad bindings.
use super::*;
#[test]
fn owner_free_boundary_preserves_stop_claim_history_and_malformed_json() {
    let conn = Connection::open_in_memory().unwrap();
    apply_up_to(&conn, 80).unwrap();
    conn.execute("INSERT INTO lane_delivery_destinations(thread_id,epoch,stopped,owner_json) VALUES('good',7,1,?1)", [r#"{"root_run_id":"run","generation":9,"ownership_generation":42}"#]).unwrap();
    conn.execute("INSERT INTO lane_delivery_destinations(thread_id,epoch,stopped,owner_json) VALUES('bad',8,0,'broken')", []).unwrap();
    conn.execute("INSERT INTO restart_transactions(restart_id,capture_id,revision,phase,metadata_json,old_builtin_tool_names_json) VALUES('old','capture',0,'completed','broken','[]')", []).unwrap();
    conn.execute("INSERT INTO recovery_graph_thread_progress VALUES('key','graph','old','capture','thread',42,1,'op','nonce','applied','unknown')", []).unwrap();
    apply_migrations(&conn).unwrap();
    let (epoch, stopped, json): (u64,bool,String) = conn.query_row("SELECT epoch,stopped,owner_json FROM lane_delivery_destinations WHERE thread_id='good'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!((epoch, stopped), (7, true));
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"root_run_id":"run","generation":9})
    );
    assert_eq!(
        conn.query_row(
            "SELECT owner_json FROM lane_delivery_destinations WHERE thread_id='bad'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "broken"
    );
    assert_eq!(
        conn.query_row("SELECT format_version FROM restart_transactions", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM recovery_graph_thread_progress",
            [],
            |r| r.get::<_, u32>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT scratch FROM recovery_graph_thread_progress_v1",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "unknown"
    );
}
