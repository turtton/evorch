//! Baseline of event appends through the production writer path (history paging, unit 0).
//! Measurement only, not a CI test.
//!
//! Path: `StorageHandle::append_event` -> writer thread -> `repo::event::append_event`
//! (IMMEDIATE transaction) -> `append_event_in_transaction` (secret guard, serialization,
//! accounting, tombstone check, INSERT with its triggers/indexes) -> COMMIT (WAL,
//! `synchronous=NORMAL`) -> reply. The interval of one append is the producer-observed wall
//! time of `append_event` (channel send -> reply received), so it includes the writer's
//! whole transaction and the channel hops. The writer's own per-append time is reported as a
//! mean from `StorageHandle::statistics` (`append_total_micros / events_written`).
//!
//! Event mix (cyclic, 16 slots; a synthetic approximation of the kinds in a real database):
//! ToolStarted x3, ToolCompleted x3, ReasoningDelta x3, MessageCompleted x2,
//! RequestStarted x2, FirstTokenObserved x2, MessageDelta x1. Payloads 100 B - 2 KiB
//! (below the 2 KiB event limit). `run_id` rotates over 32 runs, a new run every 200 events.
//! 5 of 16 slots are "committed" rows (ToolCompleted/MessageCompleted), as in history reads.
//!
//! ```text
//! AB_N=5000 AB_PRODUCERS=1,8 [AB_DB=<copy of a database>] [AB_OUT=file.jsonl] \
//!   cargo run --release --offline -p storage --example history_append_measure
//! ```
//! Without `AB_DB` a fresh database in a temp dir is used; with it, the writer opens that
//! file (it is WRITTEN: pass a copy). Each phase: 1 cold append and `AB_WARMUP` warm-up
//! appends (not measured), then `AB_N` measured appends split evenly over the producers.

use event_bus::{Event, EventKind, EventMeta, MessageEvent, ProviderEvent, ToolEvent};
use serde_json::{Value, json};
use std::{
    io::Write as _,
    sync::{Arc, Barrier},
    time::{Duration, Instant, SystemTime},
};
use storage::{Database, Storage, StorageConfig};

fn text(len: usize, seed: u64) -> String {
    let mut s = String::with_capacity(len);
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    while s.len() < len {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.push((b'a' + ((x >> 33) % 26) as u8) as char);
        if (x >> 40).is_multiple_of(7) {
            s.push(' ');
        }
    }
    s.truncate(len);
    s
}

fn make_event(i: u64) -> Event {
    let run = Some(format!("hb-run-{}", (i / 200) % 32));
    let run_id = run.clone().unwrap();
    let size = |lo: u64, hi: u64| (lo + (i.wrapping_mul(2654435761) % (hi - lo + 1))) as usize;
    let kind: EventKind = match i % 16 {
        0..=2 => ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: format!("call-{i}"),
            input: Some(json!({"command": text(size(100, 900), i)})),
            run_id: run,
            parent_call_id: None,
        }
        .into(),
        3..=5 => ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: format!("call-{}", i - 3),
            is_error: false,
            output: Some(text(size(200, 1800), i)),
            detail: None,
            run_id: run,
            parent_call_id: None,
        }
        .into(),
        6..=8 => MessageEvent::ReasoningDelta {
            delta: text(size(100, 500), i),
            run_id: run,
        }
        .into(),
        9 | 10 => MessageEvent::MessageCompleted {
            run_id,
            text: text(size(100, 1800), i),
        }
        .into(),
        11 | 12 => ProviderEvent::RequestStarted {
            request_id: format!("req-{i}"),
            provider: "local".into(),
            profile: Some("local".into()),
            protocol: "openai-completions".into(),
            model: "mock-model".into(),
            streaming: true,
            run_id: run,
            reasoning_effort: None,
            purpose: None,
        }
        .into(),
        13 | 14 => ProviderEvent::FirstTokenObserved {
            request_id: format!("req-{}", i - 2),
            provider: "local".into(),
            profile: Some("local".into()),
            protocol: "openai-completions".into(),
            model: "mock-model".into(),
            ttft_ms: 120,
            run_id: run,
        }
        .into(),
        _ => MessageEvent::MessageDelta {
            delta: text(size(100, 200), i),
            run_id: run,
        }
        .into(),
    };
    Event {
        meta: EventMeta {
            schema_version: event_bus::SCHEMA_VERSION,
            monotonic: Duration::from_millis(i),
            wall_clock: SystemTime::now(),
        },
        kind,
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil().max(1.0) as usize;
    sorted[rank.min(n) - 1]
}
fn summarize(mut v: Vec<f64>) -> Value {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    json!({"n": v.len(), "min_us": v[0], "p50_us": pct(&v, 50.0), "p95_us": pct(&v, 95.0),
           "p99_us": pct(&v, 99.0), "max_us": v[v.len() - 1],
           "mean_us": v.iter().sum::<f64>() / v.len() as f64})
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .map_or(default, |v| v.parse().expect(name))
}

fn phase(
    label: &str,
    db_path: &std::path::Path,
    producers: usize,
    n: usize,
    warmup: usize,
) -> Value {
    let config = StorageConfig {
        db_path: db_path.to_path_buf(),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config).expect("open storage");
    let handle = storage.handle();
    let counter = Arc::new(std::sync::atomic::AtomicU64::new(1_000_000));
    let next =
        |c: &std::sync::atomic::AtomicU64| c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let t = Instant::now();
    handle
        .append_event(Some("hb"), &make_event(next(&counter)))
        .expect("cold append");
    let cold_ms = t.elapsed().as_secs_f64() * 1000.0;
    for _ in 0..warmup {
        handle
            .append_event(Some("hb"), &make_event(next(&counter)))
            .expect("warmup");
    }
    let before = handle.statistics().expect("statistics");
    let per = n / producers;
    let barrier = Arc::new(Barrier::new(producers + 1));
    let mut threads = Vec::new();
    for _ in 0..producers {
        let (handle, counter, barrier) = (handle.clone(), counter.clone(), barrier.clone());
        threads.push(std::thread::spawn(move || {
            let events: Vec<Event> = (0..per)
                .map(|_| make_event(counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed)))
                .collect();
            barrier.wait();
            let mut lat = Vec::with_capacity(per);
            for e in &events {
                let t = Instant::now();
                handle.append_event(Some("hb"), e).expect("append");
                lat.push(t.elapsed().as_secs_f64() * 1e6);
            }
            lat
        }));
    }
    barrier.wait();
    let wall = Instant::now();
    let lat: Vec<f64> = threads
        .into_iter()
        .flat_map(|t| t.join().unwrap())
        .collect();
    let wall_s = wall.elapsed().as_secs_f64();
    let after = handle.statistics().expect("statistics");
    let written = after.events_written - before.events_written;
    let writer_mean_us =
        (after.append_total_micros - before.append_total_micros) as f64 / written as f64;
    let total = lat.len();
    let report = json!({
        "phase": label,
        "producers": producers,
        "measured_appends": total,
        "warmup_appends": warmup,
        "cold_first_append_ms": cold_ms,
        "producer_observed": summarize(lat),
        "throughput_events_per_s": total as f64 / wall_s,
        "wall_s": wall_s,
        "writer_internal_mean_us": writer_mean_us,
        "writer_internal_max_us_cumulative_since_open": after.max_append_micros,
        "events_written_in_phase": written,
        "write_failures": after.event_write_failures - before.event_write_failures,
    });
    storage.close();
    report
}

fn main() {
    let n = env_usize("AB_N", 5000);
    let warmup = env_usize("AB_WARMUP", 500);
    let producers: Vec<usize> = std::env::var("AB_PRODUCERS")
        .unwrap_or_else(|_| "1,8".into())
        .split(',')
        .map(|s| s.trim().parse().unwrap())
        .collect();
    let (_dir, db_path, source) = match std::env::var("AB_DB") {
        Ok(path) => (
            None,
            std::path::PathBuf::from(path),
            "existing database (copy)",
        ),
        Err(_) => {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("append-baseline.db");
            (Some(dir), path, "fresh empty database")
        }
    };
    let config = StorageConfig {
        db_path: db_path.clone(),
        ..StorageConfig::default()
    };
    drop(Database::open(&config).expect("open/migrate"));
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO sessions (id, status, created_at_ns, updated_at_ns) VALUES ('hb','running',0,0)",
            [],
        )
        .unwrap();
        let rows: i64 = conn
            .query_row("SELECT max(id) FROM events", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .unwrap()
            .unwrap_or(0);
        let uv: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        println!(
            "{}",
            json!({"source": source, "events_max_id": rows, "user_version": uv, "n": n})
        );
    }
    for p in producers {
        let report = phase(source, &db_path, p, n, warmup);
        let line = serde_json::to_string(&report).unwrap();
        println!("{line}");
        if let Ok(out) = std::env::var("AB_OUT") {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(out)
                .unwrap();
            writeln!(f, "{line}").unwrap();
        }
    }
}
