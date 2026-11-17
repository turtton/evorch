//! Measures how long the storage bridge takes to confirm a stop (07b S6): the
//! `flush` barrier with N accepted events, and `try_shutdown` (final drain,
//! writer join, thread join) with N events still queued. It prints
//! percentiles and asserts nothing; the numbers feed the choice of the bridge
//! confirmation deadline.
//!
//! `cargo run --release -p recorder --example bridge_latency -- [rounds] [raw.csv] [load-threads]`

use std::sync::Arc;
use std::time::{Duration, Instant};

use event_bus::{Event, EventBus, MessageEvent};
use recorder::storage_bridge::{OwnedStorageBridge, StorageBridge};
use storage::{Storage, StorageConfig};

fn event(index: usize) -> Event {
    Event::new(MessageEvent::ReasoningDelta {
        run_id: Some(format!("run-{}", index % 8)),
        delta: format!("fragment {index}"),
    })
}

fn bridge(events: usize) -> (tempfile::TempDir, Arc<EventBus>, OwnedStorageBridge) {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(StorageConfig {
        db_path: dir.path().join("bridge.db"),
        ..StorageConfig::default()
    })
    .unwrap();
    let bus = Arc::new(EventBus::new(events.max(64) * 2));
    let owner = OwnedStorageBridge::spawn(
        bus.clone(),
        storage,
        |handle| StorageBridge::new(handle, "latency"),
        Duration::from_secs(60),
    )
    .unwrap();
    for index in 0..events {
        bus.emit(event(index));
    }
    (dir, bus, owner)
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let index = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[index]
}

fn report(label: &str, mut samples: Vec<Duration>, raw: &mut Option<std::fs::File>) {
    use std::io::Write;
    if let Some(file) = raw {
        for sample in &samples {
            writeln!(file, "{label},{}", sample.as_micros()).unwrap();
        }
    }
    samples.sort();
    println!(
        "{label:<34} n={:<5} p50={:>9.3?} p95={:>9.3?} p99={:>9.3?} max={:>9.3?}",
        samples.len(),
        percentile(&samples, 0.50),
        percentile(&samples, 0.95),
        percentile(&samples, 0.99),
        samples.last().copied().unwrap_or_default(),
    );
}

fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|rounds| rounds.parse().ok())
        .unwrap_or(50);
    assert!(rounds > 0);
    let load: usize = std::env::args()
        .nth(3)
        .map(|n| n.parse().unwrap())
        .unwrap_or(0);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let busy: Vec<_> = (0..load)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            })
        })
        .collect();
    let mut raw = std::env::args()
        .nth(2)
        .map(|path| std::fs::File::create(path).unwrap());
    println!(
        "env: kernel={} parallelism={} profile={}",
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim(),
        std::thread::available_parallelism().map_or(0, usize::from),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
    );
    println!("rounds per cell: {rounds}, busy load threads: {load}");
    for events in [0_usize, 100, 1_000, 10_000] {
        let mut flush = Vec::new();
        let mut stop = Vec::new();
        for _ in 0..rounds {
            let (_dir, _bus, mut owner) = bridge(events);
            let started = Instant::now();
            owner.flush().unwrap();
            flush.push(started.elapsed());
            let (_dir, _bus, mut owner) = bridge(events);
            let started = Instant::now();
            owner.try_shutdown().unwrap();
            stop.push(started.elapsed());
        }
        report(&format!("bridge flush x{events} events"), flush, &mut raw);
        report(
            &format!("bridge try_shutdown x{events} events"),
            stop,
            &mut raw,
        );
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for thread in busy {
        thread.join().unwrap();
    }
}
