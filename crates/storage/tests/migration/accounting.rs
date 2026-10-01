use super::*;

#[test]
fn v12_backfills_bytes_once_including_streams_null_sessions_and_utc_boundaries() {
    let dir = TempDir::new().unwrap();
    let path = database_path(&dir);
    let conn = Connection::open(&path).unwrap();
    for migration in include_str!("../../src/migrations/sql.rs")
        .split("r#\"")
        .skip(1)
    {
        conn.execute_batch(migration.split("\"#;").next().unwrap())
            .unwrap();
    }
    for migration in [
        include_str!("../../src/migrations/v3.sql"),
        include_str!("../../src/migrations/v4.sql"),
        include_str!("../../src/migrations/v5.sql"),
        include_str!("../../src/migrations/v6.sql"),
        include_str!("../../src/migrations/v7.sql"),
        include_str!("../../src/migrations/v8.sql"),
        include_str!("../../src/migrations/v9.sql"),
        include_str!("../../src/migrations/v10.sql"),
        include_str!("../../src/migrations/v11.sql"),
    ] {
        conn.execute_batch(migration).unwrap();
    }
    conn.pragma_update(None, "user_version", 11).unwrap();
    conn.execute_batch(
        "INSERT INTO events(id,session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload)
         VALUES (-1,'session',1,0,0,'Message','日本語'),
                (NULL,'session',1,0,86400000000000,'Message','abcd'),
                (NULL,'gui-stream',1,0,86399999999999,'Message','xyz'),
                (NULL,NULL,1,0,86400000000001,'Message','hi'),
                (NULL,'before-epoch',1,0,-1,'Message','old');",
    )
    .unwrap();
    let before_events: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    drop(conn);

    let db = Database::open(&config_for(&path)).unwrap();
    assert_eq!(db.pragma_i64("user_version").unwrap(), 12);
    let conn = Connection::open(&path).unwrap();
    let session_rows = || {
        conn.prepare(
            "SELECT session_id, payload_bytes FROM event_session_bytes ORDER BY session_id",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
    };
    assert_eq!(
        session_rows(),
        vec![
            ("before-epoch".into(), 3),
            ("gui-stream".into(), 3),
            ("session".into(), 13)
        ]
    );
    let day_rows = || {
        conn.prepare("SELECT utc_day, payload_bytes FROM event_day_bytes ORDER BY utc_day")
            .unwrap()
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(day_rows(), vec![(-1, 3), (0, 12), (1, 6)]);
    let before_schema = db.pragma_i64("schema_version").unwrap();
    drop(db);
    let reopened = Database::open(&config_for(&path)).unwrap();
    assert_eq!(
        reopened.pragma_i64("schema_version").unwrap(),
        before_schema
    );
    assert_eq!(
        session_rows(),
        vec![
            ("before-epoch".into(), 3),
            ("gui-stream".into(), 3),
            ("session".into(), 13)
        ]
    );
    assert_eq!(day_rows(), vec![(-1, 3), (0, 12), (1, 6)]);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM events", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        before_events
    );
    // The BEFORE INSERT automatic-rowid sentinel must not mistake the legacy
    // negative ID for a conflicting ID; an explicit replacement still fails.
    conn.execute_batch("INSERT INTO events(session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload) VALUES(NULL,1,0,0,'Message','')").unwrap();
    assert!(conn.execute_batch("INSERT OR REPLACE INTO events(id,session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload) VALUES(-1,'bad',1,0,0,'Message','replacement')").is_err());
    assert_eq!(
        conn.query_row("SELECT payload FROM events WHERE id = -1", [], |row| row
            .get::<_, String>(
            0
        ))
        .unwrap(),
        "日本語"
    );
}
