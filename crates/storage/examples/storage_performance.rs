//! File-backed storage benchmark. Run with --release; prints JSON lines.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;
use std::time::{Duration, Instant, UNIX_EPOCH};

use event_bus::{Event, EventMeta, MessageEvent};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use storage::{Database, Storage, StorageConfig, StorageHandle};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

struct Options {
    histories: Vec<usize>,
    appends: usize,
    payload_bytes: usize,
}

fn options() -> Result<Options> {
    let mut result = Options {
        histories: vec![1_000, 10_000, 100_000],
        appends: 1_000,
        payload_bytes: 64,
    };
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--help" {
            println!(
                "storage_performance [--histories 1000,10000,100000,1000000] [--appends 1000] [--payload-bytes 64]\nUses disposable file-backed databases and the production storage limits/WAL policy. Does not clear the OS page cache. JSON counters are process-wide; unavailable counters are null."
            );
            std::process::exit(0);
        }
        let value = args.next().ok_or("missing option value")?;
        match argument.as_str() {
            "--histories" => {
                result.histories = value
                    .split(',')
                    .map(str::parse)
                    .collect::<std::result::Result<_, _>>()?;
            }
            "--appends" => result.appends = value.parse()?,
            "--payload-bytes" => result.payload_bytes = value.parse()?,
            _ => return Err(format!("unknown option: {argument}").into()),
        }
    }
    if result.histories.is_empty() || result.appends == 0 || result.payload_bytes == 0 {
        return Err("histories, appends, and payload bytes must be nonempty/positive".into());
    }
    Ok(result)
}

fn event(payload_bytes: usize) -> Event {
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_secs(1),
            wall_clock: UNIX_EPOCH + Duration::from_secs(1),
        },
        kind: MessageEvent::MessageDelta {
            delta: "x".repeat(payload_bytes),
            run_id: None,
        }
        .into(),
    }
}

fn append(handle: &StorageHandle, index: usize, event: &Event) -> Result<()> {
    match index % 3 {
        0 => handle.append_event(Some("session-a"), event),
        1 => handle.append_event(Some("session-b"), event),
        _ => handle.append_stream_event("gui-stream", event, None),
    }?;
    Ok(())
}

fn seed(config: &StorageConfig, history: usize, payload: &str) -> Result<Connection> {
    drop(Database::open(config)?);
    let mut connection = Connection::open(&config.db_path)?;
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "INSERT INTO sessions (id, status, created_at_ns, updated_at_ns)
         VALUES ('session-a', 'running', 0, 0), ('session-b', 'running', 0, 0);",
    )?;
    {
        let mut insert = transaction.prepare(
            "INSERT INTO events (session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload)
             VALUES (?1, ?2, 1000000000, 1000000000, 'Message', ?3)"
        )?;
        for index in 0..history {
            insert.execute(params![
                ["session-a", "session-b", "gui-stream"][index % 3],
                event_bus::SCHEMA_VERSION,
                payload
            ])?;
        }
    }
    transaction.execute_batch(
        "UPDATE sessions SET total_event_bytes =
         (SELECT COALESCE(SUM(OCTET_LENGTH(payload)), 0) FROM events WHERE session_id = sessions.id);"
    )?;
    transaction.commit()?;
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    Ok(connection)
}

fn process_io() -> Option<BTreeMap<String, u64>> {
    let text = std::fs::read_to_string("/proc/self/io").ok()?;
    text.lines()
        .map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.to_owned(), value.trim().parse().ok()?))
        })
        .collect()
}

fn cpu_ticks() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/stat").ok()?;
    let fields: Vec<_> = text.rsplit_once(')')?.1.split_whitespace().collect();
    // Fields 14/15 (utime/stime); fields[0] is field 3, the process state.
    Some(fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?)
}

fn ticks_per_second() -> Option<f64> {
    let output = std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let ticks: f64 = String::from_utf8(output.stdout).ok()?.trim().parse().ok()?;
    (ticks > 0.0).then_some(ticks)
}

struct Sample {
    io: Option<BTreeMap<String, u64>>,
    ticks: Option<u64>,
    time: Instant,
}

impl Sample {
    fn now() -> Self {
        Self {
            io: process_io(),
            ticks: cpu_ticks(),
            time: Instant::now(),
        }
    }

    fn finish(self, tick_rate: Option<f64>) -> Value {
        let elapsed = self.time.elapsed().as_secs_f64();
        let ticks = cpu_ticks()
            .zip(self.ticks)
            .and_then(|(end, start)| end.checked_sub(start));
        let cpu_seconds = ticks
            .zip(tick_rate)
            .map(|(ticks, rate)| ticks as f64 / rate);
        let end_io = process_io();
        let mut delta = serde_json::Map::new();
        let mut rates = serde_json::Map::new();
        for key in [
            "rchar",
            "wchar",
            "syscr",
            "syscw",
            "read_bytes",
            "write_bytes",
            "cancelled_write_bytes",
        ] {
            let difference = self
                .io
                .as_ref()
                .and_then(|start| start.get(key))
                .zip(end_io.as_ref().and_then(|end| end.get(key)))
                .and_then(|(start, end)| end.checked_sub(*start));
            delta.insert(key.into(), json!(difference));
            rates.insert(
                key.into(),
                json!(difference.map(|bytes| bytes as f64 / elapsed)),
            );
        }
        json!({"elapsed_seconds": elapsed, "cpu_seconds": cpu_seconds,
            "cpu_ticks": ticks, "io_delta": delta, "io_per_second": rates})
    }
}

fn file_bytes(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

fn benchmark(history: usize, options: &Options, tick_rate: Option<f64>) -> Result<Value> {
    let directory = tempfile::tempdir()?;
    let config = StorageConfig {
        db_path: directory.path().join("performance.db"),
        ..StorageConfig::default()
    };
    let event = event(options.payload_bytes);
    let payload = serde_json::to_string(&event.kind)?;
    let total = history
        .checked_add(options.appends)
        .and_then(|n| n.checked_add(3))
        .ok_or("row count overflow")? as u64;
    let bytes = total
        .checked_mul(payload.len() as u64)
        .ok_or("payload byte count overflow")?;
    if payload.len() as u64 > config.hard_limits.max_event_bytes
        || bytes > config.hard_limits.max_daily_event_bytes
        || total.div_ceil(3) * payload.len() as u64 > config.hard_limits.max_session_bytes
    {
        return Err("requested fixture would exceed production event/session/day limits; use fewer rows or a smaller payload".into());
    }
    let observer = seed(&config, history, &payload)?;
    let initial_database_bytes = file_bytes(&config.db_path)?;
    let storage = Storage::open(config)?;
    let handle = storage.handle();
    // Cold means fresh writer/accounting, not an artificially emptied OS cache.
    let cold = Sample::now();
    append(&handle, 0, &event)?;
    let cold_first_append = cold.finish(tick_rate);
    append(&handle, 1, &event)?;
    append(&handle, 2, &event)?;
    // Use the writer connection: externally resetting WAL after warm-up can
    // invalidate data_version and turn the next append into another cold seed.
    handle.checkpoint_now()?;
    let warm = Sample::now();
    for index in 0..options.appends {
        append(&handle, index, &event)?;
    }
    // Include final page propagation in the measurement, so recycled WAL size
    // cannot conceal the work. No other connection holds a read transaction.
    handle.checkpoint_now()?;
    let warm_appends_and_checkpoint = warm.finish(tick_rate);
    let count: i64 = observer.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
    if count != (history + options.appends + 3) as i64 {
        return Err("accepted event count mismatch".into());
    }
    storage.close();
    Ok(
        json!({"history_rows": history, "warm_accepted_appends": options.appends,
        "delta_text_bytes": options.payload_bytes, "event_payload_bytes": payload.len(),
        "initial_database_bytes": initial_database_bytes, "final_event_rows": count,
        "cold_first_append": cold_first_append, "warm_appends_and_checkpoint": warm_appends_and_checkpoint}),
    )
}

fn main() -> Result<()> {
    let options = options()?;
    let tick_rate = ticks_per_second();
    for history in &options.histories {
        println!("{}", benchmark(*history, &options, tick_rate)?);
    }
    Ok(())
}
