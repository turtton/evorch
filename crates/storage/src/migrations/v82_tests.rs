//! Upgrading a v81 database that already holds a built transcript index.
use super::apply_up_to;
use rusqlite::Connection;

fn int(c: &Connection, sql: &str) -> i64 {
    c.query_row(sql, [], |r| r.get(0)).unwrap()
}

fn triggers(c: &Connection) -> Vec<(String, String)> {
    c.prepare("SELECT name,sql FROM sqlite_schema WHERE type='trigger' AND name LIKE 'history_%' ORDER BY name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

// Given: a v81 database with a ready index for one run
// When: it is upgraded
// Then: the index is stale and rebuilt under the new reducer version, presented artifacts
//       make a run's index dirty like the other row kinds, and no source event changes
#[test]
fn a_ready_index_is_invalidated_and_presentations_are_watched() {
    let c = Connection::open_in_memory().unwrap();
    apply_up_to(&c, 81).unwrap();
    c.execute_batch(
        "INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,payload)
         VALUES (1,0,1000,'Tool','source');
         INSERT INTO history_run_state(run_id,phase,source,applied,generation,checkpoint)
         VALUES ('ready',2,3,3,4,10);
         INSERT INTO history_messages VALUES ('ready',10,0,'tool',1);",
    )
    .unwrap();
    let before = int(&c, "SELECT generation FROM history_control");
    apply_up_to(&c, 82).unwrap();
    assert_eq!(
        int(&c, "SELECT reducer_version FROM history_control"),
        crate::history::index::REDUCER_VERSION
    );
    assert_eq!(
        int(&c, "SELECT generation FROM history_control"),
        before + 1
    );
    assert_eq!(int(&c, "SELECT phase FROM history_run_state"), 0);
    assert_eq!(int(&c, "SELECT source FROM history_run_state"), 4);
    assert_eq!(int(&c, "SELECT generation FROM history_run_state"), 5);
    assert_eq!(
        c.query_row("SELECT payload FROM events", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "source"
    );
    c.execute(
        "INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,subkind,run_id,payload)
         VALUES (1,0,0,'Tool','ArtifactsPresented','presenting','{}')",
        [],
    )
    .unwrap();
    assert_eq!(
        int(
            &c,
            "SELECT COUNT(*) FROM history_run_state WHERE run_id='presenting'"
        ),
        1
    );
}

// Given: a v82 database
// When: its migration is undone
// Then: the triggers watch the v81 set again, the reducer version is the previous one and the
//       indices are stale
#[test]
fn undoing_restores_the_previous_triggers_and_version() {
    let c = Connection::open_in_memory().unwrap();
    apply_up_to(&c, 81).unwrap();
    let before = triggers(&c);
    apply_up_to(&c, 82).unwrap();
    assert_ne!(triggers(&c), before);
    c.execute_batch(
        "INSERT INTO history_run_state(run_id,phase,source,applied,generation,checkpoint)
         VALUES ('ready',2,3,3,4,10);",
    )
    .unwrap();
    c.execute_batch(include_str!("../../undo/undo_v82.sql"))
        .unwrap();
    assert_eq!(triggers(&c), before);
    assert_eq!(int(&c, "SELECT reducer_version FROM history_control"), 4);
    assert_eq!(int(&c, "SELECT phase FROM history_run_state"), 0);
}
