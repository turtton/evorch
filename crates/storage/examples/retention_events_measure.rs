//! Measures how long the live observation-event retention keeps the writer busy.
//!
//! Builds a synthetic file-backed database of about 1.3 million events with the
//! mix seen in a real history (leftover `SubscriberLagged` / `RunProgress`, the
//! four provider observations, and kept rows with larger payloads), then drives
//! the real writer's maintenance (`checkpoint_now`, which runs the retention
//! tick together with the other maintenance steps) until the removable rows are
//! gone. A second thread appends events through the public handle the whole
//! time, so its latency is what other writers wait during maintenance, and a
//! third holds a read snapshot open so the WAL cannot be reset. The scenario
//! runs once with the `subkind` column filled and once with it unfilled.
//!
//! Run with `--release`: `cargo run --release -p storage --example
//! retention_events_measure -- <work-dir>`. Output is plain text on stdout.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use event_bus::{Event, MessageEvent};
use rusqlite::{Connection, OpenFlags, params};
use storage::{Database, Storage, StorageConfig};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const TOTAL_ROWS: i64 = 1_320_000;
const DAY_NS: i64 = 86_400 * 1_000_000_000;
const RETENTION_DAYS: i64 = 90;

/// (category, tag, share per mille, payload padding bytes)
const MIX: [(&str, &str, u32, usize); 8] = [
    ("Fault", "SubscriberLagged", 201, 220),
    ("Lifecycle", "RunProgress", 228, 250),
    ("Provider", "RequestStarted", 77, 310),
    ("Provider", "RequestCompleted", 77, 310),
    ("Provider", "FirstTokenObserved", 77, 310),
    ("Provider", "CacheReuseObserved", 77, 310),
    ("Message", "MessageCompleted", 227, 800),
    ("Lifecycle", "Started", 36, 150),
];

fn now_ns() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap()
}

fn percentile(values: &mut [f64], fraction: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[((values.len() as f64 - 1.0) * fraction) as usize]
}

fn build(config: &StorageConfig, converted: bool) -> Result<i64> {
    drop(Database::open(config)?);
    let path = &config.db_path;
    let conn = Connection::open(path)?;
    conn.execute(
        "INSERT INTO sessions (id, status, created_at_ns, updated_at_ns) VALUES ('s', 'running', 0, 0)",
        [],
    )?;
    let now = now_ns();
    let span = 120 * DAY_NS;
    let mut seed: u64 = 12345;
    let mut removable = 0_i64;
    let tx = conn.unchecked_transaction()?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO events (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload, subkind) \
             VALUES ('s', 1, 1, ?1, ?2, ?3, ?4)",
        )?;
        for index in 0..TOTAL_ROWS {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let mut pick = ((seed >> 33) % 1000) as u32;
            let mut chosen = &MIX[7];
            for entry in &MIX {
                if pick < entry.2 {
                    chosen = entry;
                    break;
                }
                pick -= entry.2;
            }
            let wall = now - span + index * span / TOTAL_ROWS;
            let payload = format!(
                "{{\"kind\":\"{}\",\"payload\":{{\"kind\":\"{}\",\"payload\":{{\"pad\":\"{}\"}}}}}}",
                chosen.0,
                chosen.1,
                "x".repeat(chosen.3)
            );
            let expiring = matches!(
                chosen.1,
                "RequestCompleted" | "FirstTokenObserved" | "CacheReuseObserved"
            ) && wall < now - RETENTION_DAYS * DAY_NS;
            if matches!(chosen.1, "SubscriberLagged" | "RunProgress") || expiring {
                removable += 1;
            }
            insert.execute(params![
                wall,
                chosen.0,
                payload,
                converted.then_some(chosen.1)
            ])?;
        }
    }
    tx.commit()?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    Ok(removable)
}

fn count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?)
}

fn scenario(dir: &Path, converted: bool) -> Result<()> {
    let path: PathBuf = dir.join(format!("retention-measure-{converted}.db"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let mut config = StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    };
    config.thread_index.enabled = false;
    let removable = build(&config, converted)?;
    let reader = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let rows = count(&reader)?;
    println!(
        "== subkind filled={converted}: {rows} rows, {removable} removable, {} MiB",
        std::fs::metadata(&path)?.len() >> 20
    );

    let storage = Storage::open(config)?;
    let handle = storage.handle();
    let stop = Arc::new(AtomicBool::new(false));
    let appended = Arc::new(AtomicU64::new(0));
    let appender = {
        let (handle, stop, appended) = (handle.clone(), stop.clone(), appended.clone());
        std::thread::spawn(move || {
            let mut latencies = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                let event = Event::new(MessageEvent::MessageCompleted {
                    run_id: "run-measure".into(),
                    text: "ok".into(),
                });
                let started = Instant::now();
                handle.append_event(None, &event).unwrap();
                latencies.push(started.elapsed().as_secs_f64() * 1000.0);
                appended.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(2));
            }
            latencies
        })
    };
    // A read snapshot that stays open keeps the WAL from being reset.
    reader.execute_batch("BEGIN")?;
    let _: i64 = reader.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;

    let mut drain = Vec::new();
    let begun = Instant::now();
    loop {
        let started = Instant::now();
        handle.checkpoint_now()?;
        drain.push(started.elapsed().as_secs_f64() * 1000.0);
        let probe = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let remaining =
            count(&probe)? - (rows + appended.load(Ordering::Relaxed) as i64 - removable);
        if remaining <= 0 || drain.len() >= 5_000 {
            break;
        }
    }
    let drain_wall = begun.elapsed();
    let mut steady = Vec::new();
    for _ in 0..30 {
        let started = Instant::now();
        handle.checkpoint_now()?;
        steady.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    stop.store(true, Ordering::Relaxed);
    let mut latencies = appender.join().unwrap();
    reader.execute_batch("COMMIT")?;
    storage.close();

    println!(
        "drain: {} maintenance runs in {:.1}s; per run ms p50={:.1} p99={:.1} max={:.1}; first={:.1}",
        drain.len(),
        drain_wall.as_secs_f64(),
        percentile(&mut drain.clone(), 0.5),
        percentile(&mut drain.clone(), 0.99),
        percentile(&mut drain.clone(), 1.0),
        drain[0],
    );
    println!(
        "steady (nothing left to delete): per run ms p50={:.1} max={:.1}",
        percentile(&mut steady.clone(), 0.5),
        percentile(&mut steady, 1.0),
    );
    println!(
        "concurrent append (n={}): ms p50={:.2} p99={:.2} max={:.1}",
        latencies.len(),
        percentile(&mut latencies.clone(), 0.5),
        percentile(&mut latencies.clone(), 0.99),
        percentile(&mut latencies, 1.0),
    );
    Ok(())
}

fn main() -> Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).ok_or("usage: <work-dir>")?);
    std::fs::create_dir_all(&dir)?;
    for converted in [true, false] {
        scenario(&dir, converted)?;
    }
    Ok(())
}
