//! Upgrading a v78 database that already holds a built transcript index.
use super::apply_up_to;
use rusqlite::Connection;

fn int(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

// Given: a v78 database with a ready index for one run
// When: it is upgraded
// Then: the run's index is stale and rebuilt under the new reducer version, and the new
//       row kinds make a run's index dirty like the old ones did
#[test]
fn a_ready_index_is_invalidated_and_the_new_row_kinds_are_watched() {
    let c = Connection::open_in_memory().unwrap();
    apply_up_to(&c, 78).unwrap();
    c.execute_batch(
        "INSERT INTO history_run_state(run_id,phase,source,applied,generation,checkpoint)
         VALUES ('ready',2,3,3,4,10);
         INSERT INTO history_messages VALUES ('ready',10,0,'tool',1);",
    )
    .unwrap();
    let before = int(&c, "SELECT generation FROM history_control");
    apply_up_to(&c, 79).unwrap();
    assert_eq!(int(&c, "SELECT reducer_version FROM history_control"), 3);
    assert_eq!(
        int(&c, "SELECT generation FROM history_control"),
        before + 1
    );
    assert_eq!(int(&c, "SELECT phase FROM history_run_state"), 0);
    assert_eq!(int(&c, "SELECT generation FROM history_run_state"), 5);
    assert_ne!(
        int(&c, "SELECT source FROM history_run_state"),
        int(&c, "SELECT applied FROM history_run_state")
    );
    assert_eq!(int(&c, "SELECT COUNT(*) FROM history_tool_starts"), 0);
    for (kind, subkind) in [
        ("Tool", "ToolStarted"),
        ("Message", "ReasoningCompleted"),
        ("Provider", "RequestFailed"),
        ("Lifecycle", "AgentRunStateChanged"),
    ] {
        let run = format!("{kind}-{subkind}");
        c.execute(
            "INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,subkind,run_id,payload)
             VALUES (1,0,0,?1,?2,?3,'{}')",
            rusqlite::params![kind, subkind, run],
        )
        .unwrap();
        assert_eq!(
            int(
                &c,
                &format!("SELECT COUNT(*) FROM history_run_state WHERE run_id='{run}'")
            ),
            1,
            "{run}"
        );
    }
}
