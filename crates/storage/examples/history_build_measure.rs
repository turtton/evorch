//! Measure construction on a disposable copy: HM_DB=<copy> [HM_THREADS=id,id]
//! cargo run --release --offline -p storage --example history_build_measure
//! No source database is copied or opened implicitly; HM_DB is written.
use rusqlite::OptionalExtension;
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};
use storage::{Database, Storage, StorageConfig, history::background::Owner};

fn memory() -> serde_json::Value {
    let text = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut out = serde_json::Map::new();
    for key in ["RssAnon", "VmHWM"] {
        if let Some(value) = text
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key}:")))
        {
            let kib: u64 = value.split_whitespace().next().unwrap().parse().unwrap();
            out.insert(key.into(), (kib as f64 / 1024.0).into());
        }
    }
    out.into()
}
fn main() {
    let path = std::env::var("HM_DB").expect("HM_DB must name a disposable copy");
    let config = StorageConfig {
        db_path: path.into(),
        ..Default::default()
    };
    let initial_bytes = std::fs::metadata(&config.db_path).unwrap().len();
    let storage = Storage::open(config.clone()).unwrap();
    let writer = storage.handle();
    let reader = Database::open_read_only(&config.db_path).unwrap();
    let sql = rusqlite::Connection::open_with_flags(
        &config.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let threads = std::env::var("HM_THREADS").ok();
    let mut targets = BTreeSet::new();
    if let Some(threads) = &threads {
        for thread in threads.split(',') {
            targets.extend(
                reader
                    .runs_of_thread(thread)
                    .unwrap()
                    .into_iter()
                    .map(|r| r.run_id),
            );
        }
    }
    let before = memory();
    let started = Instant::now();
    let mut owner = Owner::start(config.clone(), writer.clone()).unwrap();
    let mut last_log = Instant::now();
    loop {
        let scanned: bool = sql.query_row("SELECT discovery_after >= (SELECT COALESCE(MAX(id),0) FROM events) FROM history_control", [], |r| r.get(0)).unwrap();
        let ready = if threads.is_some() {
            let mut statement = sql.prepare_cached("SELECT phase=2 AND source=applied AND generation IS NOT NULL FROM history_run_state WHERE run_id=?1").unwrap();
            targets.iter().all(|run| {
                statement
                    .query_row([run], |r| r.get::<_, bool>(0))
                    .optional()
                    .unwrap()
                    .unwrap_or(false)
            })
        } else {
            scanned && sql.query_row("SELECT NOT EXISTS(SELECT 1 FROM history_run_state WHERE (phase<>2 OR source IS NULL OR source IS NOT applied) AND run_id NOT IN (SELECT run_id FROM run_tombstones))", [], |r| r.get::<_,bool>(0)).unwrap()
        };
        if ready {
            break;
        }
        if last_log.elapsed() >= Duration::from_secs(10) {
            let done: i64 = sql
                .query_row(
                    "SELECT COUNT(*) FROM history_run_state WHERE phase=2 AND source=applied",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            eprintln!(
                "construction seconds={:.1} ready_runs={done} discovered_tail={scanned}",
                started.elapsed().as_secs_f64()
            );
            last_log = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    owner.stop();
    let elapsed = started.elapsed().as_secs_f64();
    let statistics = writer.statistics().unwrap();
    let mut counts = serde_json::Map::new();
    for table in [
        "history_run_state",
        "history_event_cuts",
        "history_messages",
        "history_message_sources",
        "history_pending_parts",
    ] {
        let rows: i64 = sql
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        counts.insert(table.into(), rows.into());
    }
    let wal_bytes =
        std::fs::metadata(format!("{}-wal", config.db_path.display())).map_or(0, |m| m.len());
    drop(sql);
    drop(reader);
    storage.close();
    println!(
        "{}",
        serde_json::json!({
            "construction_seconds": elapsed, "target_runs": targets.len(),
            "initial_db_bytes": initial_bytes, "final_db_bytes": std::fs::metadata(config.db_path).unwrap().len(),
            "wal_bytes_before_close": wal_bytes, "memory_before_MiB": before, "memory_after_MiB": memory(),
            "writer": statistics, "derived_rows": counts,
        })
    );
}
