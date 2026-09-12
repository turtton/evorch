use gui::model::telemetry::quota::{QuotaBackend, QuotaState};
use providers::provider::codex::quota::{QuotaError, QuotaSnapshot};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

struct Gated {
    started: mpsc::Sender<std::thread::ThreadId>,
    release: tokio::sync::mpsc::UnboundedReceiver<()>,
    completed: mpsc::Sender<()>,
}

#[async_trait::async_trait]
impl QuotaBackend for Gated {
    async fn fetch(&mut self) -> Result<QuotaSnapshot, QuotaError> {
        self.started
            .send(std::thread::current().id())
            .expect("started");
        self.release.recv().await.expect("release");
        self.completed.send(()).expect("completed");
        Err(QuotaError::Timeout)
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(120)
    }
}

#[test]
fn stop_does_not_cancel_in_flight_fetch_or_block_ui() {
    let (started, starts) = mpsc::channel();
    let (release, gate) = tokio::sync::mpsc::unbounded_channel();
    let (completed, completions) = mpsc::channel();
    let mut state = QuotaState::with_backend(Box::new(Gated {
        started,
        release: gate,
        completed,
    }));
    let now = Instant::now();
    state.poll(now);
    starts
        .recv_timeout(Duration::from_secs(5))
        .expect("worker started");
    for _ in 0..100 {
        state.poll(now + Duration::from_secs(600));
    }
    assert!(state.in_flight());
    assert!(starts.try_recv().is_err());
    state.stop();
    drop(state);
    release
        .send(())
        .expect("fetch was not aborted by stop/drop");
    completions
        .recv_timeout(Duration::from_secs(5))
        .expect("fetch finishes normally");
}

#[test]
fn repeated_fetch_uses_same_owner_thread_and_post_fetch_interval() {
    let (started, starts) = mpsc::channel();
    let (release, gate) = tokio::sync::mpsc::unbounded_channel();
    let (completed, completions) = mpsc::channel();
    let mut state = QuotaState::with_backend(Box::new(Gated {
        started,
        release: gate,
        completed,
    }));
    let now = Instant::now();
    state.poll(now);
    let first = starts
        .recv_timeout(Duration::from_secs(5))
        .expect("started");
    release.send(()).expect("release");
    completions
        .recv_timeout(Duration::from_secs(5))
        .expect("completed");
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.in_flight() {
        assert!(Instant::now() < deadline);
        state.poll(now);
        std::thread::yield_now();
    }
    assert!(state.snapshot.is_none());
    assert!(state.error.is_some());
    state.poll(now + Duration::from_secs(119));
    assert!(starts.try_recv().is_err());
    state.poll(now + Duration::from_secs(120));
    let second = starts
        .recv_timeout(Duration::from_secs(5))
        .expect("second fetch");
    assert_eq!(first, second);
    release.send(()).expect("release");
    completions
        .recv_timeout(Duration::from_secs(5))
        .expect("completed");
}
