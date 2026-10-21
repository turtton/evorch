//! Cancel/Stop while an ownership checkpoint waits for a contended registry,
//! and the finalization checkpoint that runs before the terminal state.
mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use event_bus::{AgentRunPhase, EventBus, EventKind, EventReceiver, LifecycleEvent};
use runtime::ownership::{Lease, OwnerPermit, Registry, RetryPolicy, ThreadOwner};
use runtime::{AgentRuntime, Role, RunConfig, RunId, StopScope};
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};
use tools::{Permissions, Tool, ToolError, ToolExecutor, ToolResult};
use tracing_subscriber::layer::SubscriberExt;

const OTHER_RUN: &str = "other-run-of-the-thread";
const TOOL_OUTPUT: &str = "tool-output-before-contention";

// ---- captured ownership records ----

type Record = BTreeMap<String, String>;

struct Fields<'a>(&'a mut Record);

impl tracing::field::Visit for Fields<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
}

#[derive(Clone, Default)]
struct Records(Arc<Mutex<Vec<Record>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Records {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != runtime::ownership::DB_TRACE_TARGET {
            return;
        }
        let mut fields = Record::new();
        event.record(&mut Fields(&mut fields));
        self.0.lock().unwrap().push(fields);
    }
}

/// Registry records of every thread in this binary; tests filter by thread.
fn records() -> &'static Records {
    static RECORDS: OnceLock<Records> = OnceLock::new();
    RECORDS.get_or_init(|| {
        let records = Records::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(records.clone()),
        )
        .expect("the only subscriber of this test binary");
        records
    })
}

fn thread_records(thread: &str) -> Vec<Record> {
    records()
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|record| record.get("thread_id").map(String::as_str) == Some(thread))
        .cloned()
        .collect()
}

fn is_checkpoint_failure(record: &Record) -> bool {
    record.get("message").map(String::as_str) == Some("ownership registry operation failed")
        && record.get("operation").map(String::as_str) == Some("checkpoint")
}

/// Wait until `seen` holds for the records of `thread`.
async fn wait_for_records(thread: &str, seen: impl Fn(&[Record]) -> bool) {
    timeout(Duration::from_secs(30), async {
        while !seen(&thread_records(thread)) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("records of {thread}: {:?}", thread_records(thread)));
}

/// The in-loop checkpoint was interrupted, and finalization's checkpoint has
/// since met the same contention.
fn finalization_contended(records: &[Record]) -> bool {
    records
        .iter()
        .position(|record| {
            record.get("message").map(String::as_str)
                == Some("ownership registry retry interrupted")
                && record.get("operation").map(String::as_str) == Some("checkpoint")
        })
        .is_some_and(|interrupted| records[interrupted..].iter().any(is_checkpoint_failure))
}

// ---- run harness ----

/// Signals entry, then waits for the test to release it or interrupt the run.
/// Named `read` because the worker role only offers its allowed tools.
struct Gate {
    entered: Arc<Notify>,
    proceed: Arc<Notify>,
}

#[async_trait::async_trait]
impl Tool for Gate {
    fn name(&self) -> &'static str {
        "read"
    }
    fn schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }
    fn permissions(&self) -> Permissions {
        Permissions::read_only()
    }
    async fn execute(&self, _: serde_json::Value) -> Result<ToolResult, ToolError> {
        self.entered.notify_one();
        self.proceed.notified().await;
        Ok(ToolResult::success(TOOL_OUTPUT))
    }
}

#[derive(Clone, Copy)]
enum Interrupt {
    Cancel,
    Stop,
}

struct Harness {
    _directory: tempfile::TempDir,
    path: PathBuf,
    thread: String,
    permit: OwnerPermit,
    runtime: AgentRuntime,
    events: EventReceiver,
    entered: Arc<Notify>,
    proceed: Arc<Notify>,
}

fn harness(thread: &str, policy: RetryPolicy) -> Harness {
    records();
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("owners.db");
    let owner = ThreadOwner::new(
        thread.into(),
        Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    let mut registry = Registry::open(&path).expect("registry");
    registry.start(&owner).expect("start");
    // Another run of the same thread keeps its turn open throughout.
    registry
        .update(thread, |owner| {
            owner.active_runs.insert(OTHER_RUN.into());
            owner.active_turn = true;
            Ok(())
        })
        .expect("other run");
    let permit = OwnerPermit {
        registry_path: path.clone(),
        thread_id: thread.into(),
        lease: owner.lease,
        run_id: None,
    };
    let bus = Arc::new(EventBus::new(1024));
    let events = bus.subscribe();
    let entered = Arc::new(Notify::new());
    let proceed = Arc::new(Notify::new());
    let mut executor = ToolExecutor::new(bus.clone());
    executor
        .register(Arc::new(Gate {
            entered: entered.clone(),
            proceed: proceed.clone(),
        }))
        .expect("gate tool");
    let model = support::ScriptedModel::new([
        Ok(support::tool_response("gate-call", "read", json!({}))),
        Ok(support::text_response(
            "unreachable",
            providers::FinishReason::Stop,
        )),
    ]);
    let runtime = AgentRuntime::new(bus, Arc::new(executor), Arc::new(model))
        .with_ownership_retry_policy(policy);
    Harness {
        _directory: directory,
        path,
        thread: thread.into(),
        permit,
        runtime,
        events,
        entered,
        proceed,
    }
}

impl Harness {
    /// Hold the run inside the tool, before any ownership checkpoint starts.
    async fn start_gated(&mut self) -> RunId {
        let run = self.runtime.delegate_background(
            Role::Worker,
            "contend".into(),
            RunConfig {
                ownership: Some(self.permit.clone()),
                ..RunConfig::default()
            },
        );
        timeout(Duration::from_secs(10), self.entered.notified())
            .await
            .expect("the run reaches its tool");
        run
    }

    /// Start the run and return once its post-tool checkpoint is retrying
    /// against `reader`, which the caller then holds. Only long-budget tests
    /// use this observation to inspect an in-progress checkpoint.
    async fn contend(&mut self) -> (RunId, Registry) {
        let run = self.start_gated().await;
        // A shared lock: the checkpoint commit waits for it, readers do not.
        let reader = self.permit.mutation_guard().expect("reader");
        self.proceed.notify_one();
        let thread = self.thread.clone();
        wait_for_records(&thread, |records| records.iter().any(is_checkpoint_failure)).await;
        (run, reader)
    }

    fn interrupt(&self, run: RunId, interrupt: Interrupt) {
        match interrupt {
            Interrupt::Cancel => self.runtime.cancel(run).expect("cancel"),
            Interrupt::Stop => self.runtime.stop(run, StopScope::SelfOnly).expect("stop"),
        }
    }

    fn terminal_events(&mut self, run: RunId) -> Vec<(AgentRunPhase, Option<String>)> {
        let mut terminal = Vec::new();
        for event in self.events.drain_pending_snapshot() {
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                ..
            }) = event.kind
                && run_id == run.to_string()
                && matches!(
                    to,
                    AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                )
            {
                terminal.push((to, reason));
            }
        }
        terminal
    }

    async fn assert_unpublished(&mut self, run: RunId) {
        assert!(
            timeout(Duration::ZERO, self.runtime.wait(run))
                .await
                .is_err(),
            "terminal phase published before finalization finished"
        );
        assert_eq!(self.terminal_events(run), []);
    }

    fn owner(&self) -> ThreadOwner {
        Registry::open_readonly(&self.path)
            .expect("registry")
            .attach(&self.thread)
            .expect("owner")
    }

    fn payload(&self) -> Option<String> {
        use rusqlite::OptionalExtension;
        rusqlite::Connection::open(&self.path)
            .expect("connection")
            .query_row(
                "SELECT payload FROM owner_checkpoints WHERE thread_id = ?1",
                [&self.thread],
                |row| row.get(0),
            )
            .optional()
            .expect("checkpoint query")
    }
}

fn long_budget() -> RetryPolicy {
    RetryPolicy {
        deadline: Duration::from_secs(120),
        attempt_timeout: Duration::from_millis(100),
        ..RetryPolicy::default()
    }
}

async fn interrupted_contention_is_finalized_once_it_clears(
    thread: &str,
    interrupt: Interrupt,
    expected: (AgentRunPhase, &str),
) {
    let mut harness = harness(thread, long_budget());
    let (run, reader) = harness.contend().await;
    harness.interrupt(run, interrupt);
    wait_for_records(thread, finalization_contended).await;
    // Finalization is still checkpointing: nothing terminal is public yet.
    harness.assert_unpublished(run).await;
    assert!(harness.owner().active_runs.contains(&run.to_string()));

    drop(reader);
    let phase = timeout(Duration::from_secs(30), harness.runtime.wait(run))
        .await
        .expect("finalization completes once the contention clears")
        .expect("run");
    assert_eq!(phase, expected.0);
    let terminal = harness.terminal_events(run);
    assert_eq!(terminal, [(expected.0, Some(expected.1.to_owned()))]);
    let records = thread_records(thread);
    assert!(
        !records.iter().any(|record| {
            record.get("message").map(String::as_str)
                == Some("ownership registry contention outlasted the retry deadline")
        }),
        "{records:?}"
    );

    let owner = harness.owner();
    assert_eq!(
        owner.active_runs.iter().collect::<Vec<_>>(),
        [OTHER_RUN],
        "only this run leaves the thread's active runs"
    );
    assert!(owner.active_turn, "the other run's turn stays open");
    let payload = harness.payload().expect("finalization checkpoint");
    let checkpoint = runtime::ownership::OwnerCheckpoint::decode(&payload).expect("metadata");
    assert_eq!(
        checkpoint.boundary,
        runtime::ownership::CheckpointBoundary::Terminal
    );
    assert_eq!(
        checkpoint.runs[&run.to_string()].snapshot,
        runtime::ownership::SnapshotReference::none(
            runtime::ownership::SnapshotNoneReason::NoRunStore
        )
    );
    assert!(!payload.contains(TOOL_OUTPUT), "{payload}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_checkpoint_contention_finalizes_the_cancelled_run() {
    interrupted_contention_is_finalized_once_it_clears(
        "thread-cancel",
        Interrupt::Cancel,
        (AgentRunPhase::Error, event_bus::CANCELLED_REASON),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_during_checkpoint_contention_finalizes_the_stopped_run() {
    interrupted_contention_is_finalized_once_it_clears(
        "thread-stop",
        Interrupt::Stop,
        (AgentRunPhase::Stopped, "stopped"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalization_contention_past_its_deadline_ends_the_run_as_that_error() {
    let thread = "thread-expired";
    // Stop while the tool gate is still closed: no in-loop checkpoint can
    // start or expire before Stop is delivered. Only finalization contends.
    let mut harness = harness(
        thread,
        RetryPolicy {
            deadline: Duration::from_millis(300),
            attempt_timeout: Duration::from_millis(100),
            ..RetryPolicy::default()
        },
    );
    let run = harness.start_gated().await;
    let reader = harness.permit.mutation_guard().expect("reader");
    harness.interrupt(run, Interrupt::Stop);
    // The tool's interrupt drops its blocked future and enters finalization.
    // Do not infer an in-progress state from historical contention records:
    // the short budget may already be exhausted when this task resumes.

    let phase = timeout(Duration::from_secs(30), harness.runtime.wait(run))
        .await
        .expect("finalization gives up at its deadline")
        .expect("run");
    drop(reader);
    assert_eq!(phase, AgentRunPhase::Error);
    let terminal = harness.terminal_events(run);
    let [(AgentRunPhase::Error, Some(reason))] = terminal.as_slice() else {
        panic!("one terminal error: {terminal:?}");
    };
    assert!(
        reason.starts_with("ownership checkpoint failed at commit (attempt "),
        "{reason}"
    );
    assert!(reason.ends_with("): database is locked"), "{reason}");
    let records = thread_records(thread);
    let expired: Vec<_> = records
        .iter()
        .filter(|record| {
            record.get("message").map(String::as_str)
                == Some("ownership registry contention outlasted the retry deadline")
        })
        .collect();
    assert_eq!(expired.len(), 1, "only finalization expires: {records:?}");

    // Nothing was committed: the run stays active with the other one, and
    // no checkpoint payload exists.
    let owner = harness.owner();
    let mut active: Vec<_> = owner.active_runs.iter().cloned().collect();
    active.sort();
    let mut expected = vec![OTHER_RUN.to_owned(), run.to_string()];
    expected.sort();
    assert_eq!(active, expected);
    assert!(owner.active_turn);
    assert_eq!(harness.payload(), None);
}
