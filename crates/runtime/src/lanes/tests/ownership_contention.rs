//! Hold a real ownership writer through the first failed child BEGIN.
use super::*;
use crate::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
use std::sync::mpsc;
use tracing_subscriber::layer::SubscriberExt;

#[derive(Default)]
struct ChildBeginFailure {
    child: bool,
    begin: bool,
}

impl tracing::field::Visit for ChildBeginFailure {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        match field.name() {
            "operation" => self.child = value == "child",
            "phase" => self.begin = value == "begin",
            _ => {}
        }
    }
    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

struct ReleaseWriterOnFailure(Mutex<Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ReleaseWriterOnFailure {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        let mut failure = ChildBeginFailure::default();
        event.record(&mut failure);
        if failure.child
            && failure.begin
            && let Some((release, committed)) = self.0.lock().unwrap().take()
        {
            release.send(()).unwrap();
            committed.recv().unwrap();
        }
    }
}

#[tokio::test]
async fn lane_creation_retries_begin_while_an_ownership_writer_is_active() {
    let mut f = Fixture::new();
    let path = f._dir.path().join("owners.db");
    let mut registry = Registry::open(&path).unwrap();
    let owner = ThreadOwner::new(
        "owned-lead".into(),
        Lease {
            owner_id: "test-host".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    registry.start(&owner).unwrap();
    let permit = OwnerPermit {
        registry_path: path.clone(),
        thread_id: owner.thread_id.clone(),
        lease: owner.lease.clone(),
        run_id: None,
    };
    let lead = f
        .runtime
        .delegate_chat(
            "owned-lead",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                ownership: Some(permit),
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    f.phase(lead, AgentRunPhase::Waiting).await;

    // A RESERVED writer allows the lane's generation preflight to read but
    // blocks its EXCLUSIVE BEGIN, as a heartbeat/checkpoint writer can do.
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE thread_owners SET state = state")
        .unwrap();
    let (release, released) = mpsc::channel();
    let (committed, commit) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        released.recv().unwrap();
        writer.execute_batch("COMMIT").unwrap();
        committed.send(()).unwrap();
    });
    // Only an actual failed BEGIN releases the writer. No sleep, elapsed-time
    // assertion, or test-side retry: the product must survive attempt 1 failing.
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::registry()
            .with(ReleaseWriterOnFailure(Mutex::new(Some((release, commit))))),
    );
    let result = tracing::dispatcher::with_default(&dispatch, || {
        f.runtime.open_lane(lead, "contended-lane", brief())
    });
    worker.join().unwrap();
    let lane = result.expect("lane creation must survive ownership BEGIN contention");
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    let child = registry.attach(&lane.thread_id).unwrap();
    assert_eq!(child.lease.owner_id, owner.lease.owner_id);
    assert_eq!(
        registry.attach(&owner.thread_id).unwrap().lease,
        owner.lease
    );
    let stored = f.storage.handle().lane(&lane.lane_id).unwrap().unwrap();
    assert_eq!(stored.root_run_id, Some(lane.root_run_id.to_string()));
    f.cancel(lane.root_run_id).await;
    f.cancel(lead).await;
}
