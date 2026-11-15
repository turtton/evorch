//! Measures how long the ledger takes to confirm a stop (07b S6): from
//! `stop_all_children` (or the owners dropping their children) to
//! `reap_all_children`, and from the end of workers to `join_workers`. It prints
//! percentiles and asserts nothing; the numbers feed the choice of the reap and
//! join confirmation deadlines.
//!
//! `cargo run --release -p sandbox --example ledger_latency -- [rounds] [raw.csv] [load-threads]`
//!
//! `raw.csv` receives every sample as `label,microseconds`; `load-threads`
//! busy-loop threads emulate CPU load for the loaded run. The report starts
//! with the environment (kernel, CPU, parallelism) and prints the normal and
//! the deliberately-stuck shapes (TERM ignored, grandchild, pipe held)
//! separately. Children use a self-STOP, so they are provably alive and
//! blocked before the stop starts; nothing sleeps to synchronize.

use std::time::{Duration, Instant};

use sandbox::{ChildKind, SpawnLedger};

fn command(script: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("sh");
    command
        .args(["-c", script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    sandbox::process::apply_launch_policy(command.as_std_mut());
    command
}

/// What a child does while the core stops it.
const SHAPES: [(&str, &str); 4] = [
    ("plain", "kill -STOP $$"),
    ("term-ignoring", "trap '' TERM; kill -STOP $$"),
    ("grandchild", "sh -c 'kill -STOP $$' & wait"),
    ("pipe-holder", "(exec sleep 3600) & kill -STOP $$"),
];

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let index = ((sorted.len() as f64 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[index]
}

static RAW: std::sync::Mutex<Option<std::fs::File>> = std::sync::Mutex::new(None);

fn environment() {
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let cpu = read("/proc/cpuinfo")
        .lines()
        .find(|line| line.starts_with("model name"))
        .and_then(|line| line.split(':').nth(1))
        .map(|name| name.trim().to_owned())
        .unwrap_or_default();
    println!(
        "env: os={} kernel={} cpu={cpu:?} parallelism={} loadavg={:?}",
        std::env::consts::OS,
        read("/proc/sys/kernel/osrelease").trim(),
        std::thread::available_parallelism().map_or(0, usize::from),
        read("/proc/loadavg").trim(),
    );
    println!(
        "env: profile={}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
}

fn report(label: &str, mut samples: Vec<Duration>) {
    if let Some(file) = RAW.lock().unwrap().as_mut() {
        use std::io::Write;
        for sample in &samples {
            let _ = writeln!(file, "{label},{}", sample.as_micros());
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

async fn start(
    ledger: &SpawnLedger,
    script: &str,
    children: usize,
) -> Vec<sandbox::process::GroupChild> {
    let mut activity = ledger.watch_children();
    let mut owned = Vec::new();
    for _ in 0..children {
        let admission = ledger.admit().unwrap();
        let mut command = command(script);
        owned.push(
            admission
                .spawn_group(ChildKind::Other, Some("latency"), &mut command)
                .unwrap(),
        );
    }
    activity
        .wait_for(|activity| activity.live == children)
        .await
        .unwrap();
    owned
}

async fn stop_to_reap(script: &str, children: usize) -> Duration {
    let ledger = SpawnLedger::new();
    let owned = start(&ledger, script, children).await;
    // Each owner reaps its own child, like a tool that is told to stop.
    let waiters: Vec<_> = owned
        .into_iter()
        .map(|mut child| tokio::spawn(async move { child.wait().await }))
        .collect();
    ledger.close_fence();
    let started = Instant::now();
    ledger.stop_all_children();
    ledger.reap_all_children().await.unwrap();
    let elapsed = started.elapsed();
    for waiter in waiters {
        waiter.await.unwrap().unwrap();
    }
    elapsed
}

async fn dropped_to_reap(script: &str, children: usize) -> Duration {
    let ledger = SpawnLedger::new();
    let owned = start(&ledger, script, children).await;
    ledger.close_fence();
    let started = Instant::now();
    drop(owned);
    ledger.reap_all_children().await.unwrap();
    started.elapsed()
}

/// Two joiners at once, the shape the S4 fix wakes: the second must not wait
/// for a notification that never comes.
async fn concurrent_join(workers: usize) -> Duration {
    let ledger = SpawnLedger::new();
    let (end, _) = tokio::sync::watch::channel(false);
    for _ in 0..workers {
        let admission = ledger.admit().unwrap();
        let mut end = end.subscribe();
        admission.spawn_worker("latency", async move {
            let _ = end.wait_for(|ended| *ended).await;
        });
    }
    ledger.close_fence();
    let started = Instant::now();
    end.send_replace(true);
    let (first, second) = tokio::join!(ledger.join_workers(), ledger.join_workers());
    first.unwrap();
    second.unwrap();
    started.elapsed()
}

async fn no_children() -> Duration {
    let ledger = SpawnLedger::new();
    ledger.close_fence();
    let started = Instant::now();
    ledger.stop_all_children();
    ledger.reap_all_children().await.unwrap();
    ledger.join_workers().await.unwrap();
    started.elapsed()
}

async fn end_to_join(workers: usize) -> Duration {
    let ledger = SpawnLedger::new();
    let (end, _) = tokio::sync::watch::channel(false);
    for _ in 0..workers {
        let admission = ledger.admit().unwrap();
        let mut end = end.subscribe();
        admission.spawn_worker("latency", async move {
            let _ = end.wait_for(|ended| *ended).await;
        });
    }
    ledger.close_fence();
    let started = Instant::now();
    end.send_replace(true);
    ledger.join_workers().await.unwrap();
    started.elapsed()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|rounds| rounds.parse().ok())
        .unwrap_or(100);
    if let Some(path) = std::env::args().nth(2) {
        *RAW.lock().unwrap() = Some(std::fs::File::create(path).unwrap());
    }
    let load: usize = std::env::args()
        .nth(3)
        .and_then(|load| load.parse().ok())
        .unwrap_or(0);
    let stop_load = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let busy: Vec<_> = (0..load)
        .map(|_| {
            let stop = stop_load.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            })
        })
        .collect();
    environment();
    println!("rounds per cell: {rounds}, busy load threads: {load}");
    let mut empty = Vec::new();
    for _ in 0..rounds {
        empty.push(no_children().await);
    }
    report("stop+reap+join without children", empty);
    for (name, script) in SHAPES {
        for children in [1_usize, 8, 64] {
            let mut stopped = Vec::new();
            let mut dropped = Vec::new();
            for _ in 0..rounds {
                stopped.push(stop_to_reap(script, children).await);
                dropped.push(dropped_to_reap(script, children).await);
            }
            report(&format!("stop->reap   {name} x{children}"), stopped);
            report(&format!("drop->reap   {name} x{children}"), dropped);
        }
    }
    for workers in [1_usize, 8, 64, 512] {
        let mut samples = Vec::new();
        for _ in 0..rounds {
            samples.push(end_to_join(workers).await);
        }
        report(&format!("worker end->join x{workers}"), samples);
        let mut concurrent = Vec::new();
        for _ in 0..rounds {
            concurrent.push(concurrent_join(workers).await);
        }
        report(&format!("worker end->2 joiners x{workers}"), concurrent);
    }
    stop_load.store(true, std::sync::atomic::Ordering::Relaxed);
    for thread in busy {
        thread.join().unwrap();
    }
}
