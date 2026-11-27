use super::apply_up_to;
use rusqlite::Connection;

#[test]
fn duration_version_invalidates_ready_indices_without_changing_schema_or_sources() {
    let c = Connection::open_in_memory().unwrap();
    apply_up_to(&c, 79).unwrap();
    c.execute_batch(
        "INSERT INTO events(schema_version,monotonic_ns,wall_clock_ns,kind,payload)
         VALUES (1,0,1000,'Tool','source');
         INSERT INTO history_run_state(run_id,phase,source,applied,generation,checkpoint)
         VALUES ('ready',2,3,3,4,10);
         INSERT INTO history_messages VALUES ('ready',10,0,'tool',1);",
    )
    .unwrap();
    let schema = || {
        c.prepare("SELECT sql FROM sqlite_schema ORDER BY type,name")
            .unwrap()
            .query_map([], |r| r.get::<_, Option<String>>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let before = schema();
    apply_up_to(&c, 80).unwrap();
    assert_eq!(schema(), before);
    assert_eq!(
        c.query_row(
            "SELECT phase,source,applied,generation,checkpoint FROM history_run_state",
            [],
            |r| Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?
            )),
        )
        .unwrap(),
        (0, 4, 3, 5, 10)
    );
    assert_eq!(
        c.query_row("SELECT reducer_version FROM history_control", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        crate::history::index::REDUCER_VERSION
    );
    assert_eq!(
        c.query_row("SELECT payload FROM events", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "source"
    );
}
