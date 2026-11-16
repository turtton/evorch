//! Offline measurement, NOT a wall-clock correctness test.
//! cargo run -p runtime --features recovery-test-support --example measure_learning_suspend -- 100 [raw.csv] [load-threads]
//! Uses real SQLite/run journals, pending providers and normal dispatch sealing.
//! 20 is the default daily admission quota, not a global concurrency limit.
use std::{sync::Arc, time::Instant};

use event_bus::EventBus;
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use runtime::{
    AgentInvocationContext, AgentModel, AgentRuntime, ModelPreference, Role, RunConfig, RunStore,
    RuntimeError,
};
use storage::{Storage, StorageConfig};
use tokio::sync::mpsc;

struct PendingProvider(mpsc::UnboundedSender<()>);
#[async_trait::async_trait]
impl AgentModel for PendingProvider {
    async fn complete(
        &self,
        invocation: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        if invocation.category.as_deref() == Some("lesson") {
            self.0.send(()).unwrap();
            return std::future::pending().await;
        }
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "completed source".into(),
                }],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "offline-measurement".into()
    }
}

async fn sample(concurrency: usize) -> f64 {
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: dir.path().join("journal.db"),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(8192));
    let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    ));
    let (started, mut arrivals) = mpsc::unbounded_channel();
    let runtime = AgentRuntime::new(bus, executor, Arc::new(PendingProvider(started)))
        .with_learning_origin(runtime::learning_jobs::current_origin(
            runtime::learning_jobs::LearningOriginKind::Core,
            Some("offline-measurement".into()),
        ))
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap())
        .with_learning(runtime::memory_queue::LearningSettings {
            writer: storage.handle(),
            storage: config,
            project: "offline".into(),
            quick: ModelPreference {
                profile: "offline".into(),
                model: None,
                reasoning_effort: None,
            },
        });
    for _ in 0..concurrency {
        runtime.delegate_background(
            Role::Worker,
            "Complete an offline source task".into(),
            RunConfig::default(),
        );
    }
    for _ in 0..concurrency {
        arrivals.recv().await.unwrap();
    }
    let start = Instant::now();
    let pause = runtime.pause_learning();
    let suspension = runtime.suspend_learning(&pause).unwrap();
    suspension.wait().await.unwrap();
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    // Outside the timed interval: prove that the measured receipt really
    // precedes a successful seal/capture, rather than a terminal-event shortcut.
    let _attempt = runtime.begin_restart_quiesce().unwrap();
    runtime.wait_quiesced().await.unwrap();
    let fence = runtime.fence_dispatch_restart().await.unwrap();
    runtime.drain_dispatch_tails(&fence).await.unwrap();
    runtime
        .capture_recovery_records(&fence, &Default::default())
        .unwrap();
    elapsed
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let repeats = std::env::args()
        .nth(1)
        .map(|n| n.parse::<usize>().expect("positive sample count"))
        .unwrap_or(100);
    assert!(repeats > 0);
    let mut raw = std::env::args()
        .nth(2)
        .map(|p| std::fs::File::create(p).unwrap());
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
    println!(
        "parallelism={} load_threads={load} profile={}",
        std::thread::available_parallelism().map_or(0, usize::from),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    for concurrency in [1, 4, 20] {
        let mut samples = Vec::with_capacity(repeats);
        for _ in 0..repeats {
            let measured = sample(concurrency).await;
            if let Some(file) = raw.as_mut() {
                use std::io::Write;
                writeln!(file, "cohort={concurrency},{measured:.6}").unwrap();
            }
            samples.push(measured);
        }
        samples.sort_by(f64::total_cmp);
        let percentile = |p: usize| samples[(repeats * p).div_ceil(100).saturating_sub(1)];
        println!(
            "cohort={concurrency} n={repeats} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} max_ms={:.3}",
            percentile(50),
            percentile(95),
            percentile(99),
            samples.last().unwrap()
        );
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for thread in busy {
        thread.join().unwrap();
    }
}
