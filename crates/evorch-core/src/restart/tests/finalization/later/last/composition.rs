//! Real bootstrap/recorder/SQLite/owner/control composition, not mock receipts.
//! Tracing is an observation seam only. Channels control progress; Instant is
//! used exclusively for reporting, never for a functional verdict.
use super::*;
pub(super) mod drain;
use std::time::Instant;
use tokio::sync::{Notify, oneshot};
use tracing_subscriber::{Layer, layer::SubscriberExt};

#[cfg(feature = "bootstrap-test-support")]
#[tokio::test]
async fn production_composition_empty_queue() {
    matrix("composition:0", 2, 0).await;
}
#[cfg(feature = "bootstrap-test-support")]
#[tokio::test]
async fn production_composition_normal_queue() {
    matrix("composition:100", 2, 0).await;
}
#[cfg(feature = "bootstrap-test-support")]
#[tokio::test]
async fn production_composition_near_capacity() {
    matrix("composition:16383", 2, 0).await;
}
#[cfg(feature = "bootstrap-test-support")]
#[tokio::test]
async fn production_composition_backpressure() {
    matrix("composition:16385", 2, 0).await;
}
#[cfg(feature = "bootstrap-test-support")]
#[tokio::test]
async fn production_composition_uncertain_ack_join_and_invalidation_cleanup() {
    matrix("composition_faults", 4, 0).await;
}

#[derive(Default)]
struct Fields {
    phase: String,
    pending: usize,
    capacity: usize,
    monotonic_ns: u64,
}
impl tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "phase" {
            self.phase = value.into();
        }
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        match field.name() {
            "pending" => self.pending = value as usize,
            "capacity" => self.capacity = value as usize,
            "monotonic_ns" => self.monotonic_ns = value,
            _ => (),
        }
    }
    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}
struct Block {
    phase: &'static str,
    monotonic_ns: Option<u64>,
    entered: oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}
#[derive(Default)]
struct State {
    events: Vec<(String, Instant)>,
    pending: usize,
    capacity: usize,
    blocks: Vec<Block>,
    flush_pending: Option<usize>,
    panic_on: Option<&'static str>,
}
#[derive(Default)]
struct Trace {
    state: Mutex<State>,
    changed: Notify,
}
type Current = Arc<Mutex<Option<Arc<Trace>>>>;
struct Observer(Current);
impl<S: tracing::Subscriber> Layer<S> for Observer {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "restart_drain" {
            return;
        }
        let Some(trace) = self.0.lock().unwrap().clone() else {
            return;
        };
        let mut fields = Fields::default();
        event.record(&mut fields);
        let (block, fail) = {
            let mut state = trace.state.lock().unwrap();
            if fields.phase == "queue_pending" || fields.phase == "persist_end" {
                state.pending = fields.pending;
                if fields.capacity != 0 {
                    state.capacity = fields.capacity;
                }
            } else if fields.phase != "persist_begin" {
                if fields.phase == "flush_begin" {
                    state.flush_pending = Some(fields.pending);
                }
                state.events.push((fields.phase.clone(), Instant::now()));
            }
            let index = state.blocks.iter().position(|block| {
                block.phase == fields.phase
                    && block
                        .monotonic_ns
                        .is_none_or(|id| id == fields.monotonic_ns)
            });
            let fail = state.panic_on == Some(fields.phase.as_str());
            if fail {
                state.panic_on = None;
            }
            (index.map(|i| state.blocks.remove(i)), fail)
        };
        trace.changed.notify_one();
        assert!(
            !fail,
            "injected recorder worker failure before ACK delivery"
        );
        if let Some(block) = block {
            block.entered.send(()).unwrap();
            // Only independently owned blocking workers encounter these probes.
            block.release.recv().unwrap();
        }
    }
}
impl Trace {
    fn block(
        &self,
        phase: &'static str,
        monotonic_ns: Option<u64>,
    ) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, seen) = oneshot::channel();
        let (release, receive) = std::sync::mpsc::channel();
        self.state.lock().unwrap().blocks.push(Block {
            phase,
            monotonic_ns,
            entered,
            release: receive,
        });
        (seen, release)
    }
    async fn until(&self, ready: impl Fn(&State) -> bool) {
        loop {
            let changed = self.changed.notified();
            if ready(&self.state.lock().unwrap()) {
                return;
            }
            changed.await;
        }
    }
}
type Gate = (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>);
pub(super) struct Load {
    trace: Arc<Trace>,
    queue: usize,
    rows: usize,
    writer_release: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    ack: Mutex<Option<Gate>>,
}
fn event(index: usize) -> event_bus::Event {
    event_bus::Event::new(event_bus::MessageEvent::ReasoningDelta {
        run_id: None, // No coalescing: each accepted event must produce one row.
        delta: format!("composition-{index}"),
    })
}
impl Load {
    #[cfg(feature = "bootstrap-test-support")]
    pub(super) fn observe_web(&self, web: &workbench_web::WebServer) {
        let trace = self.trace.clone();
        web.set_restart_observer(Arc::new(move |phase| {
            trace
                .state
                .lock()
                .unwrap()
                .events
                .push((phase.into(), Instant::now()));
            trace.changed.notify_one();
        }));
    }
    pub(super) async fn open_web(
        &self,
        address: std::net::SocketAddr,
    ) -> tokio::task::JoinHandle<()> {
        let client = reqwest::Client::new();
        let reply = client
            .post(format!("http://{address}/api/v1/request"))
            .json(&serde_json::json!({
                "request_id":"composition", "session_id":"web",
                "kind":"query", "payload":{"type":"lifecycle","payload":{"type":"get_core_status"}}
            }))
            .send()
            .await
            .unwrap();
        assert!(reply.status().is_success());
        let _: serde_json::Value = reply.json().await.unwrap();
        // Consume like a live browser so the bounded SSE queue cannot replace
        // the stream with ResyncRequired during the recorder workload.
        let mut sse = client
            .get(format!("http://{address}/api/v1/events?scope=all"))
            .send()
            .await
            .unwrap();
        assert!(sse.status().is_success());
        let mut first = Vec::new();
        while !first
            .windows(b"subscription_started".len())
            .any(|w| w == b"subscription_started")
        {
            first.extend_from_slice(&sse.chunk().await.unwrap().unwrap());
        }
        tokio::spawn(async move { while sse.chunk().await.unwrap().is_some() {} })
    }
    pub(super) async fn prepare(&self, host: &BootstrappedHost) {
        let sql = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
        if self.rows != 0 {
            sql.execute(
                "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?1)
                 INSERT INTO events(session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload)
                 SELECT 'existing',1,0,x,'Message',?2 FROM n",
                rusqlite::params![self.rows, serde_json::to_string(&event(usize::MAX).kind).unwrap()],
            ).unwrap();
        }
        assert_eq!(
            sql.query_row(
                "SELECT count(*) FROM events WHERE session_id='existing'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            self.rows
        );
        let settled = event_bus::Event::new(event_bus::MessageEvent::ReasoningDelta {
            run_id: None,
            delta: "settle".into(),
        });
        let (seen, release) = self.trace.block(
            "persist_end",
            Some(settled.meta.monotonic.as_nanos() as u64),
        );
        host.runtime.event_bus().emit(settled);
        seen.await.unwrap();
        assert_eq!(self.trace.state.lock().unwrap().pending, 0);
        release.send(()).unwrap();
        let actual_rows: usize = sql
            .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
            .unwrap();
        let db_bytes: u64 = sql
            .query_row(
                "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                [],
                |row| row.get(0),
            )
            .unwrap();
        println!(
            "M2-SIZE {},{},{actual_rows},{db_bytes}",
            self.queue, self.rows
        );
        *self.ack.lock().unwrap() = Some(self.trace.block("flush_ack", None));
        if self.queue == 0 {
            return;
        }
        let first = event(0);
        let (entered, release) = self.trace.block(
            "persist_begin",
            Some(first.meta.monotonic.as_nanos() as u64),
        );
        *self.writer_release.lock().unwrap() = Some(release);
        host.runtime.event_bus().emit(first);
        entered.await.unwrap();
        // Pace the bus, not SQLite: the production bus has 4096 slots while
        // the recorder has 16384 permits. Never test accidental bus overflow.
        for start in (1..self.queue).step_by(512) {
            let end = (start + 512).min(self.queue);
            for index in start..end {
                host.runtime.event_bus().emit(event(index));
            }
            self.trace.until(|s| s.pending >= end.min(16_384)).await;
        }
        if self.queue > 16_384 {
            self.trace
                .until(|s| s.events.iter().any(|e| e.0 == "backpressure"))
                .await;
            assert_eq!(self.trace.state.lock().unwrap().capacity, 16_384);
        }
    }
    pub(super) async fn release(&self, core: &RestartCoordinator, host: &BootstrappedHost) {
        // This matrix proves durability/order and reports wall-clock intervals,
        // not scheduler speed. Freeze only through the real storage drain;
        // drain.rs separately crosses the production budget with an explicit clock.
        self.trace
            .until(|s| s.events.iter().any(|e| e.0 == "flush_begin"))
            .await;
        tokio::time::pause();
        assert_eq!(
            core.0.state.lock().unwrap().completed_stage,
            Some(ExitStage::WorkersJoined)
        );
        println!(
            "M2-DEPTH {},{},{}",
            self.queue,
            self.rows,
            self.trace.state.lock().unwrap().flush_pending.unwrap()
        );
        if let Some(release) = self.writer_release.lock().unwrap().take() {
            release.send(()).unwrap();
        }
        let (ack, release) = self.ack.lock().unwrap().take().unwrap();
        ack.await.unwrap();
        // A successful flush ACK cannot stand in for subscriber join or writer
        // termination. The caller is held AFTER receiving the real SQLite ACK.
        assert_eq!(
            core.0.state.lock().unwrap().completed_stage,
            Some(ExitStage::WorkersJoined)
        );
        let writer = host.composition.storage_writer.clone();
        writer.barrier().unwrap();
        assert!(
            !self
                .trace
                .state
                .lock()
                .unwrap()
                .events
                .iter()
                .any(|e| e.0 == "subscriber_join_end")
        );
        core.0.runtime.event_bus().emit(event(self.queue));
        release.send(()).unwrap();
        self.trace
            .until(|s| s.events.iter().any(|e| e.0 == "writer_barrier_end"))
            .await;
        tokio::time::resume();
    }
    pub(super) fn verify(&self, host: &BootstrappedHost, runs: &[RunId]) {
        host.composition.storage_writer.barrier().unwrap();
        let sql = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
        let mut statement = sql
            .prepare("SELECT payload FROM events WHERE session_id!='existing'")
            .unwrap();
        let payloads = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap();
        let mut actual = Vec::new();
        for payload in payloads {
            if let event_bus::EventKind::Message(event_bus::MessageEvent::ReasoningDelta {
                delta,
                ..
            }) = serde_json::from_str(&payload.unwrap()).unwrap()
                && let Some(index) = delta.strip_prefix("composition-")
            {
                actual.push(index.parse::<usize>().unwrap());
            }
        }
        actual.sort_unstable();
        assert_eq!(
            actual,
            (0..=self.queue).collect::<Vec<_>>(),
            "missing or duplicate accepted events, including the post-ACK final snapshot"
        );
        verify_checkpoints(host, runs, true);
        let state = self.trace.state.lock().unwrap();
        let position = |phase: &str| state.events.iter().position(|e| e.0 == phase).unwrap();
        for pair in [
            ("flush_ack", "subscriber_drain_begin"),
            ("final_flush_end", "bridge_worker_joined"),
            ("bridge_worker_joined", "subscriber_join_end"),
            ("subscriber_join_end", "writer_barrier_end"),
            ("writer_barrier_end", "owner_stop_begin"),
        ] {
            assert!(position(pair.0) < position(pair.1), "{pair:?}");
        }
        // Emit raw intervals only; there is deliberately no wall-clock bound.
        for (name, begin, end) in [
            ("flush_ack", "flush_begin", "flush_ack"),
            (
                "subscriber_drain_join",
                "subscriber_join_begin",
                "subscriber_join_end",
            ),
            ("final_flush", "final_flush_begin", "final_flush_end"),
            (
                "bridge_worker_join",
                "final_flush_end",
                "bridge_worker_joined",
            ),
            (
                "writer_barrier",
                "writer_barrier_begin",
                "writer_barrier_end",
            ),
            (
                "web_connections",
                "web_connections_begin",
                "web_connections_end",
            ),
            ("web_api_sse", "web_api_sse_begin", "web_api_sse_end"),
            ("web_api_request", "web_api_begin", "web_api_end"),
            ("web_sse_stop", "web_connections_begin", "web_sse_end"),
            ("web_backend", "web_backend_begin", "web_backend_end"),
            ("fd_prepare", "fd_prepare_begin", "fd_prepare_end"),
            (
                "signal_transfer",
                "signal_transfer_begin",
                "signal_transfer_end",
            ),
            ("owner_stop_join", "owner_stop_begin", "owner_stop_end"),
        ] {
            let elapsed = state.events[position(end)]
                .1
                .duration_since(state.events[position(begin)].1);
            println!(
                "M2-MEASURE {},{},{name},{:.6}",
                self.queue,
                self.rows,
                elapsed.as_secs_f64() * 1000.
            );
        }
    }
}
fn verify_checkpoints(host: &BootstrappedHost, runs: &[runtime::RunId], held: bool) {
    let tx = saved(host);
    assert_eq!(tx.expected.len(), runs.len());
    let db = storage::Database::open(&host.composition.storage).unwrap();
    for member in &tx.expected {
        let snapshot = db.run_context(&member.run_id).unwrap().unwrap();
        assert!(member.snapshot.as_ref().unwrap().matches(&snapshot));
    }
    // Early M2 failures precede the owner-progress capture (stage 6);
    // receipt/snapshot readback above still covers every parked run.
    if held {
        assert_eq!(
            tx.owner_progress
                .iter()
                .find(|p| p.thread_id == "active")
                .unwrap()
                .parked_runs,
            runs.iter()
                .map(ToString::to_string)
                .collect::<BTreeSet<_>>()
        );
    }
}
pub(super) async fn run() {
    let current = Current::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(Observer(current.clone())),
    )
    .unwrap();
    let samples: usize = std::env::var("EVORCH_M2_SAMPLES")
        .ok()
        .map(|n| n.parse().unwrap())
        .unwrap_or(1);
    assert!(samples > 0);
    let group = std::env::var("EVORCH_M2_MATRIX_GROUP").unwrap();
    let selected: Option<usize> = group.split_once(':').map(|(_, q)| q.parse().unwrap());
    for rows in [0, 10_000] {
        for queue in [0, 100, 16_383, 16_385] {
            if selected.is_some_and(|selected| selected != queue) {
                continue;
            }
            for _ in 0..samples {
                let trace = Arc::new(Trace::default());
                *current.lock().unwrap() = Some(trace.clone());
                let load = Load {
                    trace,
                    queue,
                    rows,
                    writer_release: Mutex::new(None),
                    ack: Mutex::new(None),
                };
                cell_loaded(ExitStage::OwnerStopped, Action::Success, true, Some(&load)).await;
                *current.lock().unwrap() = None;
            }
        }
    }
}

pub(super) async fn faults() {
    let current = Current::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(Observer(current.clone())),
    )
    .unwrap();
    for fault in [
        "ack_unknown",
        "subscriber_join_failure",
        "join_failure",
        "invalidation_unknown",
    ] {
        let trace = Arc::new(Trace::default());
        *current.lock().unwrap() = Some(trace.clone());
        let model = Arc::new(Model::default());
        let (dir, mut host) = boot_composition(model.clone(), true).await;
        let core = RestartCoordinator::new(&host);
        let (target, metadata) = target(dir.path()).await;
        let (runs, id) = owned_chats(&host, &core, 3).await;
        let sql = rusqlite::Connection::open(&host.composition.storage.db_path).unwrap();
        if fault == "ack_unknown" {
            // The real bridge worker drops the real reply sender before sending
            // it. SQLite may have committed; no fabricated negative ACK is used.
            trace.state.lock().unwrap().panic_on = Some("flush_reply");
        } else if fault == "subscriber_join_failure" {
            // Panic the real subscriber thread after its persistence worker
            // joined: flush ACK has succeeded, but OS-thread join must fail.
            trace.state.lock().unwrap().panic_on = Some("bridge_worker_joined");
        } else if fault == "join_failure" {
            host.services
                .start("composition-join-failure", |stop| async move {
                    stop.await?;
                    panic!("injected background join failure");
                });
        } else {
            sql.execute_batch("CREATE TRIGGER deny_invalidation BEFORE UPDATE ON restart_transactions WHEN NEW.phase='invalidated' BEGIN SELECT RAISE(ABORT,'invalidation unknown'); END;").unwrap();
        }
        let tx = create(&host, metadata);
        core.start_finalization(id, &mut host, tx, target).unwrap();
        assert!(core.wait_finalization(id).await.is_err());
        assert_eq!(core.snapshot().phase, RestartPhase::FailedClosed);
        let resources = core.0.state.lock().unwrap().finalization.clone().unwrap();
        let result = resources.lock().await.cleanup().await;
        assert!(result.is_err());
        assert_eq!(model.0.load(Ordering::SeqCst), 3);
        assert!(host.restart_producers().iter().all(|gate| !gate.is_open()));
        assert!(core.cancel(id).is_err());
        host.composition.storage_writer.barrier().unwrap();
        if fault == "invalidation_unknown" {
            assert_eq!(
                saved(&host).phase,
                storage::recovery::restart::RestartPhase::Armed
            );
            assert!(host.owner.attach("active").unwrap().handoff_hold.is_some());
            sql.execute_batch("DROP TRIGGER deny_invalidation").unwrap();
            assert!(
                resources.lock().await.cleanup().await.is_err(),
                "original cleanup error stays sticky after successful invalidation retry"
            );
            assert!(host.owner.attach("active").unwrap().handoff_hold.is_none());
        } else {
            let last = core.0.state.lock().unwrap().completed_stage;
            assert_eq!(
                last,
                Some(if fault != "join_failure" {
                    ExitStage::WorkersJoined
                } else {
                    ExitStage::ChildrenReaped
                })
            );
        }
        verify_checkpoints(&host, &runs, fault == "invalidation_unknown");
        assert_eq!(
            saved(&host).phase,
            storage::recovery::restart::RestartPhase::Invalidated
        );
        assert!(
            trace
                .state
                .lock()
                .unwrap()
                .events
                .iter()
                .all(|e| e.0 != "owner_stop_begin")
        );
        core.transition_to_shutdown();
        assert!(host.shutdown().await.is_err());
        println!("M2-FAILURE {fault}: retained cleanup, writer and owner; no business resume");
        println!("M2-CELL {fault}");
        std::io::stdout().flush().unwrap();
        *current.lock().unwrap() = None;
    }
}
