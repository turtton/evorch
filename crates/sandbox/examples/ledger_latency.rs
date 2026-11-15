//! Measures how long the ledger takes to confirm a stop (07b S6): from
//! `stop_all_children` (or the owners dropping their children) to
//! `reap_all_children`, and from the end of workers to `join_workers`. It prints
//! percentiles and asserts nothing; the numbers feed the choice of the reap and
//! join confirmation deadlines.
//!
//! `cargo run --release -p sandbox --example ledger_latency -- [rounds]`

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

fn report(label: &str, mut samples: Vec<Duration>) {
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
    println!("rounds per cell: {rounds}");
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
    }
}
