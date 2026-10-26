//! Host input receipts at the seal: a send racing Stop or a panic never ends
//! accepted, and a send after the seal leaves no trace.
use super::*;
use crate::runtime::user_inbox::SendPausePoint;
use storage::HostInputStatus;

mod restored;

/// Every request waits until released; then the model panics or keeps waiting.
struct GatedModel {
    release: Arc<tokio::sync::Notify>,
    entered: Arc<tokio::sync::Notify>,
    panics: bool,
}

#[async_trait::async_trait]
impl AgentModel for GatedModel {
    fn selected_model(&self, _role: Role, _category: Option<&str>) -> String {
        "gated-test-model".into()
    }

    async fn complete(
        &self,
        _invocation: &crate::AgentInvocationContext,
        _role: Role,
        _messages: &[providers::Message],
        _tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, RuntimeError> {
        self.entered.notify_one();
        self.release.notified().await;
        if self.panics {
            panic!("gated test model panicked");
        }
        std::future::pending().await
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    _storage: storage::Storage,
    runtime: AgentRuntime,
    events: event_bus::EventReceiver,
    release: Arc<tokio::sync::Notify>,
    entered: Arc<tokio::sync::Notify>,
    run: RunId,
}

fn fixture(panics: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("receipts.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let bus = Arc::new(EventBus::new(1024));
    let events = bus.subscribe();
    let release = Arc::new(tokio::sync::Notify::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(ToolExecutor::new(bus)),
        Arc::new(GatedModel {
            release: release.clone(),
            entered: entered.clone(),
            panics,
        }),
    )
    .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let run = runtime
        .delegate_chat(
            "receipts",
            Role::Worker,
            "initial".into(),
            RunConfig {
                initial_prompt_id: Some("prompt-initial".into()),
                ..RunConfig::default()
            },
        )
        .unwrap();
    Fixture {
        _dir: dir,
        _storage: storage,
        runtime,
        events,
        release,
        entered,
        run,
    }
}

impl Fixture {
    fn receipt(&self, id: &str) -> Option<storage::HostInputReceipt> {
        self.runtime
            .host_input_receipts(&[id.to_string()])
            .unwrap()
            .pop()
    }

    fn inbox(&self) -> Arc<user_inbox::UserInbox> {
        Arc::clone(&self.runtime.entry(self.run).unwrap().user_inbox)
    }

    /// Send `text` on a blocking thread that stops at `point`; returns once it stopped.
    async fn paused_send(
        &self,
        point: SendPausePoint,
        id: &str,
    ) -> (
        user_inbox::test_pause::PauseControl,
        tokio::task::JoinHandle<Result<(), RuntimeError>>,
    ) {
        let control = self.inbox().pause_next_send(point);
        let runtime = self.runtime.clone();
        let run = self.run;
        let id = id.to_string();
        let send = tokio::task::spawn_blocking(move || {
            runtime.send_message_with_images(run, format!("text of {id}"), Vec::new(), Some(id))
        });
        let user_inbox::test_pause::PauseControl { reached, resume } = control;
        let reached = tokio::task::spawn_blocking(move || {
            reached.recv().unwrap();
            reached
        })
        .await
        .unwrap();
        (
            user_inbox::test_pause::PauseControl { reached, resume },
            send,
        )
    }

    async fn wait_running(&self) {
        let mut phase = self.runtime.entry(self.run).unwrap().phase_rx.clone();
        while *phase.borrow_and_update() != AgentRunPhase::Running {
            phase.changed().await.unwrap();
        }
    }

    fn phase(&self) -> AgentRunPhase {
        *self.runtime.entry(self.run).unwrap().phase_rx.borrow()
    }

    fn notified(&mut self) -> Vec<LifecycleEvent> {
        self.events
            .drain_pending_snapshot()
            .into_iter()
            .filter_map(|event| match event.kind {
                event_bus::EventKind::Lifecycle(
                    event @ (LifecycleEvent::HostInputDelivered { .. }
                    | LifecycleEvent::HostInputDropped { .. }
                    | LifecycleEvent::HostInputUnknown { .. }
                    | LifecycleEvent::AgentRunStateChanged { .. }),
                ) => Some(event),
                _ => None,
            })
            .collect()
    }
}

async fn stop_while_a_send_holds_the_lock_drops_it() {
    let mut fixture = fixture(false);
    fixture.wait_running().await;
    let (control, send) = fixture
        .paused_send(SendPausePoint::AfterReserve, "prompt-late")
        .await;

    fixture
        .runtime
        .stop(fixture.run, StopScope::SelfOnly)
        .unwrap();
    // The seal waits for the send, so the run cannot finish yet.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(fixture.receipt("prompt-late").is_none());
    control.resume.send(()).unwrap();
    send.await.unwrap().unwrap();
    assert_eq!(
        fixture.runtime.wait(fixture.run).await.unwrap(),
        AgentRunPhase::Stopped
    );

    let late = fixture.receipt("prompt-late").unwrap();
    assert_eq!(
        (late.status, late.reason.as_deref()),
        (HostInputStatus::Dropped, Some("run_ended"))
    );
    // The initial prompt is in the terminal snapshot.
    assert_eq!(
        fixture.receipt("prompt-initial").unwrap().status,
        HostInputStatus::Delivered
    );
    let record = fixture
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(fixture.run)
        .unwrap()
        .unwrap();
    assert_eq!(record.host_inputs_json, r#"["prompt-initial"]"#);
    let events = fixture.notified();
    let dropped = events
        .iter()
        .position(|event| {
            matches!(event, LifecycleEvent::HostInputDropped { prompt_id, .. } if prompt_id == "prompt-late")
        })
        .expect("dropped notice");
    let stopped = events
        .iter()
        .position(|event| {
            matches!(
                event,
                LifecycleEvent::AgentRunStateChanged {
                    to: AgentRunPhase::Stopped,
                    ..
                }
            )
        })
        .expect("stopped");
    assert!(dropped < stopped, "{events:?}");
}

async fn a_send_after_the_seal_is_refused_without_a_trace() {
    let fixture = fixture(false);
    fixture.wait_running().await;
    let (control, send) = fixture
        .paused_send(SendPausePoint::BeforeLock, "prompt-sealed")
        .await;
    fixture
        .runtime
        .stop(fixture.run, StopScope::SelfOnly)
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(fixture.run).await.unwrap(),
        AgentRunPhase::Stopped
    );
    control.resume.send(()).unwrap();
    assert!(matches!(
        send.await.unwrap(),
        Err(RuntimeError::RunTerminated { .. })
    ));
    assert!(fixture.receipt("prompt-sealed").is_none());
    let evidence = fixture
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .review_evidence_for_lineage(&[fixture.run.to_string()])
        .unwrap();
    assert!(
        evidence
            .iter()
            .all(|row| row.text != "text of prompt-sealed")
    );
}

async fn a_panic_seals_before_error_and_leaves_no_input_accepted() {
    let mut fixture = fixture(true);
    fixture.wait_running().await;
    let (control, send) = fixture
        .paused_send(SendPausePoint::AfterReserve, "prompt-panic")
        .await;

    fixture.release.notify_one();
    // run_panicked waits for the send at the seal; Error is not public yet.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(fixture.phase(), AgentRunPhase::Running);
    control.resume.send(()).unwrap();
    send.await.unwrap().unwrap();
    assert_eq!(
        fixture.runtime.wait(fixture.run).await.unwrap(),
        AgentRunPhase::Error
    );

    let late = fixture.receipt("prompt-panic").unwrap();
    assert_eq!(
        (late.status, late.reason.as_deref()),
        (HostInputStatus::Unknown, Some("run_panicked"))
    );
    // The setup checkpoint delivered the prompt before the first request.
    assert_eq!(
        fixture.receipt("prompt-initial").unwrap().status,
        HostInputStatus::Delivered
    );
    let events = fixture.notified();
    let unknown = events
        .iter()
        .position(|event| {
            matches!(event, LifecycleEvent::HostInputUnknown { prompt_id, .. } if prompt_id == "prompt-panic")
        })
        .expect("unknown notice");
    let error = events
        .iter()
        .position(|event| {
            matches!(
                event,
                LifecycleEvent::AgentRunStateChanged {
                    to: AgentRunPhase::Error,
                    ..
                }
            )
        })
        .expect("error");
    assert!(unknown < error, "{events:?}");
}

#[tokio::test]
async fn stop_while_a_send_holds_the_lock_drops_it_current_thread() {
    stop_while_a_send_holds_the_lock_drops_it().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_while_a_send_holds_the_lock_drops_it_multi_thread() {
    stop_while_a_send_holds_the_lock_drops_it().await;
}

#[tokio::test]
async fn a_send_after_the_seal_is_refused_without_a_trace_current_thread() {
    a_send_after_the_seal_is_refused_without_a_trace().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_after_the_seal_is_refused_without_a_trace_multi_thread() {
    a_send_after_the_seal_is_refused_without_a_trace().await;
}

#[tokio::test]
async fn a_panic_seals_before_error_and_leaves_no_input_accepted_current_thread() {
    a_panic_seals_before_error_and_leaves_no_input_accepted().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_panic_seals_before_error_and_leaves_no_input_accepted_multi_thread() {
    a_panic_seals_before_error_and_leaves_no_input_accepted().await;
}

#[tokio::test]
async fn a_sealed_inbox_refuses_host_input_while_its_channel_is_open() {
    // The seal alone closes the incarnation, before any channel or phase change.
    let fixture = fixture(false);
    fixture.wait_running().await;
    fixture.inbox().seal();
    let runtime = fixture.runtime.clone();
    let run = fixture.run;
    let sent = tokio::task::spawn_blocking(move || {
        runtime.send_message_with_images(
            run,
            "after the seal".into(),
            Vec::new(),
            Some("prompt-after-seal".into()),
        )
    })
    .await
    .unwrap();
    assert!(matches!(sent, Err(RuntimeError::RunTerminated { .. })));
    assert!(fixture.receipt("prompt-after-seal").is_none());
    assert_eq!(fixture.runtime.follow_up_status(run).unwrap().pending, 0);
    // Internal notices are not host input and still arrive.
    fixture
        .runtime
        .send_internal_message(run, "notice".into())
        .unwrap();
    fixture.runtime.cancel(run).unwrap();
}

#[tokio::test]
async fn a_refused_acceptance_neither_enqueues_nor_writes_a_receipt() {
    let fixture = fixture(false);
    fixture.wait_running().await;
    let store = fixture.runtime.shared.run_store.get().unwrap();
    for (id, text) in [
        ("secret", format!("sk-{}", "a".repeat(32))),
        ("oversize", "x".repeat(1024 * 1024)),
    ] {
        let before = fixture.inbox().status().pending;
        assert!(
            fixture
                .runtime
                .send_message_with_images(fixture.run, text, vec![], Some(id.into()))
                .is_err()
        );
        assert_eq!(fixture.inbox().status().pending, before);
        assert!(fixture.receipt(id).is_none());
    }
    let conn = rusqlite::Connection::open(&store.db_path).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_input BEFORE INSERT ON host_input_receipts BEGIN SELECT RAISE(FAIL, 'injected receipt failure'); END;").unwrap();
    assert!(
        fixture
            .runtime
            .send_message_with_images(
                fixture.run,
                "sql failed input".into(),
                vec![],
                Some("sql-failed".into())
            )
            .is_err()
    );
    assert_eq!(fixture.inbox().status().pending, 0);
    assert!(fixture.receipt("sql-failed").is_none());
    assert!(
        store
            .review_evidence_for_lineage(&[fixture.run.to_string()])
            .unwrap()
            .iter()
            .all(|row| row.text != "sql failed input")
    );
    conn.execute_batch("DROP TRIGGER reject_input;").unwrap();
    fixture
        .runtime
        .stop(fixture.run, StopScope::SelfOnly)
        .unwrap();
    fixture.runtime.wait(fixture.run).await.unwrap();
}

#[tokio::test]
async fn a_failed_terminal_snapshot_leaves_no_false_delivery_or_unsettled_acceptance() {
    let mut fixture = fixture(false);
    fixture.wait_running().await;
    fixture
        .runtime
        .send_message_with_images(
            fixture.run,
            "not read".into(),
            vec![],
            Some("not-read".into()),
        )
        .unwrap();
    let store = fixture.runtime.shared.run_store.get().unwrap();
    let conn = rusqlite::Connection::open(&store.db_path).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_terminal BEFORE UPDATE ON run_contexts WHEN NEW.terminal_phase = 'Stopped' BEGIN SELECT RAISE(FAIL, 'injected terminal failure'); END;").unwrap();
    fixture
        .runtime
        .stop(fixture.run, StopScope::SelfOnly)
        .unwrap();
    assert_eq!(
        fixture.runtime.wait(fixture.run).await.unwrap(),
        AgentRunPhase::Stopped
    );
    assert_eq!(
        fixture.receipt("not-read").unwrap().status,
        HostInputStatus::Dropped
    );
    assert!(
        fixture
            .runtime
            .host_input_receipts(&["not-read".into(), "prompt-initial".into()])
            .unwrap()
            .iter()
            .all(|receipt| receipt.status != HostInputStatus::Accepted)
    );
    let record = store.restore_record(fixture.run).unwrap().unwrap();
    assert!(!record.messages_json.contains("not read"));
    assert!(!record.host_inputs_json.contains("not-read"));
    assert!(!fixture.notified().iter().any(|event| matches!(event, LifecycleEvent::HostInputDelivered { prompt_id, .. } if prompt_id == "not-read")));
}

#[tokio::test]
async fn closed_writer_refuses_before_enqueue_and_orphan_reconciliation_settles_existing_input() {
    let fixture = fixture(false);
    fixture.wait_running().await;
    fixture
        .runtime
        .send_message_with_images(
            fixture.run,
            "accepted before close".into(),
            vec![],
            Some("before-close".into()),
        )
        .unwrap();
    let inbox = fixture.inbox();
    fixture._storage.close();
    assert!(
        fixture
            .runtime
            .send_message_with_images(
                fixture.run,
                "never enqueued".into(),
                vec![],
                Some("after-close".into())
            )
            .is_err()
    );
    assert_eq!(inbox.status().pending, 1);
    fixture
        .runtime
        .stop(fixture.run, StopScope::SelfOnly)
        .unwrap();
    fixture.runtime.wait(fixture.run).await.unwrap();
    let config = storage::StorageConfig {
        db_path: fixture._dir.path().join("receipts.db"),
        ..Default::default()
    };
    let db = storage::Database::open(&config).unwrap();
    assert!(
        db.host_input_receipts(&["after-close".into()])
            .unwrap()
            .is_empty()
    );
    // Closed storage cannot promise a final receipt. On reopening, ownership's
    // proof of the dead run makes the durable Accepted receipt Unknown.
    assert_eq!(
        db.host_input_receipts(&["before-close".into()]).unwrap()[0].status,
        HostInputStatus::Accepted
    );
    let reopened = storage::Storage::open(config.clone()).unwrap();
    reopened
        .handle()
        .mark_host_inputs_unknown(&fixture.run.to_string(), "orphaned_host")
        .unwrap();
    assert_eq!(
        db.host_input_receipts(&["before-close".into()]).unwrap()[0].status,
        HostInputStatus::Unknown
    );
    assert!(
        db.review_evidence_for_target(&fixture.run.to_string())
            .unwrap()
            .iter()
            .all(|row| row.text != "never enqueued")
    );
}
