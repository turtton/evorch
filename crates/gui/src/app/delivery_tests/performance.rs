//! Work budgets and timer-driven recovery without wall-clock sleeps.
use super::*;
use std::time::{Duration, Instant};

struct CountingReceipts {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    receipts: Arc<Mutex<Vec<storage::HostInputReceipt>>>,
}
impl CommandSink for CountingReceipts {
    fn has_host_input_receipts(&self) -> bool {
        true
    }
    fn host_input_receipts(
        &self,
        ids: &[String],
    ) -> Option<Result<Vec<storage::HostInputReceipt>, storage::StorageError>> {
        self.calls.lock().unwrap().push(ids.to_vec());
        Some(Ok(self
            .receipts
            .lock()
            .unwrap()
            .iter()
            .filter(|receipt| ids.contains(&receipt.prompt_id))
            .cloned()
            .collect()))
    }
    fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
        vec![]
    }
}

fn install_counter(
    state: &mut WorkbenchState<DemoSource>,
    receipts: Arc<Mutex<Vec<storage::HostInputReceipt>>>,
) -> Arc<Mutex<Vec<Vec<String>>>> {
    let calls = Arc::new(Mutex::new(vec![]));
    state.sink = Box::new(CountingReceipts {
        calls: calls.clone(),
        receipts,
    });
    calls
}

#[test]
fn event_batch_queries_only_unsettled_ids_once_and_never_probes_empty_ids() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.path().join("sidebar.json"));
    state.history[0].status = DeliveryStatus::Queued;
    for i in 0..2000 {
        let mut message = state.history[0].clone();
        message.id = format!("settled-{i}");
        message.status = DeliveryStatus::Delivered;
        state.history.push(message);
    }
    let mut resolved = state.history[0].clone();
    resolved.id = "resolved".into();
    resolved.resolution = Some(Resolution::Dismissed);
    state.history.push(resolved);
    let mut rejected = state.history[0].clone();
    rejected.id = "rejected".into();
    rejected.status = DeliveryStatus::Rejected {
        reason: "no".into(),
    };
    state.history.push(rejected);
    let calls = install_counter(
        &mut state,
        Arc::new(Mutex::new(vec![review::receipt(
            storage::HostInputStatus::Delivered,
        )])),
    );
    state.receipt_reconciliation.last_repair = Some(Instant::now());
    state.apply_events((0..100).map(|_| {
        event_bus::Event::new(event_bus::LifecycleEvent::AgentRunStateChanged {
            run_id: "run-1".into(),
            from: event_bus::AgentRunPhase::Running,
            to: event_bus::AgentRunPhase::Done,
            reason: None,
        })
    }));
    state.reconcile_follow_ups(); // render's second entry point must not query again
    assert_eq!(*calls.lock().unwrap(), vec![vec!["old".to_string()]]);
    assert_eq!(state.history[0].status, DeliveryStatus::Delivered);
    assert_eq!(state.sidebar_write_count.get(), 1);
    // Periodic passes include delivered rows, never rejected/resolved rows.
    let due = Instant::now() + super::super::delivery::RECEIPT_REPAIR_INTERVAL;
    state.reconcile_receipts_at(due);
    assert_eq!(calls.lock().unwrap()[1].len(), 2001);
    state.history.clear();
    state.reconcile_receipts_at(due + Duration::from_secs(4));
    assert_eq!(calls.lock().unwrap().len(), 2);

    // Protect the work budget against reintroducing comparison or save clones.
    let delivery = include_str!("../delivery.rs");
    let reconciliation = delivery
        .split("pub(super) fn apply_receipts")
        .nth(1)
        .unwrap()
        .split("pub(super) fn rebuild_delivery_rows")
        .next()
        .unwrap();
    assert!(!reconciliation.contains("self.history.clone()"));
    let history = include_str!("../history.rs");
    let writer = history
        .split("pub(super) fn write_history_sidebar")
        .nth(1)
        .unwrap()
        .split("pub fn restore_history")
        .next()
        .unwrap();
    assert!(!writer.contains("self.history.clone()"));
}

#[test]
fn periodic_repair_delivers_and_retracts_without_notifications() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.path().join("sidebar.json"));
    state.history[0].status = DeliveryStatus::Queued;
    let receipts = Arc::new(Mutex::new(vec![review::receipt(
        storage::HostInputStatus::Accepted,
    )]));
    let calls = install_counter(&mut state, receipts.clone());
    let now = Instant::now();
    state.reconcile_receipts_at(now);
    receipts.lock().unwrap()[0].status = storage::HostInputStatus::Delivered;
    state.reconcile_receipts_at(now + Duration::from_secs(1));
    assert_eq!(state.history[0].status, DeliveryStatus::Queued);
    assert_eq!(calls.lock().unwrap().len(), 1);
    state.reconcile_receipts_at(now + Duration::from_secs(3));
    assert_eq!(state.history[0].status, DeliveryStatus::Delivered);
    assert_eq!(state.transcripts.thread().entries().len(), 1);
    receipts.lock().unwrap()[0].status = storage::HostInputStatus::Unknown;
    state.reconcile_receipts_at(now + Duration::from_secs(6));
    assert_eq!(state.history[0].status, DeliveryStatus::Unknown);
    assert!(state.transcripts.thread().entries().is_empty());
    assert_eq!(calls.lock().unwrap().len(), 3);
}

#[test]
fn delivered_notification_targets_one_settled_id_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.path().join("sidebar.json"));
    state.history[0].status = DeliveryStatus::Delivered;
    state.receipt_reconciliation.last_repair = Some(Instant::now());
    let receipts = Arc::new(Mutex::new(vec![review::receipt(
        storage::HostInputStatus::Unknown,
    )]));
    let calls = install_counter(&mut state, receipts);
    state.apply_events([event_bus::Event::new(
        event_bus::LifecycleEvent::HostInputUnknown {
            run_id: "run-1".into(),
            prompt_id: "old".into(),
            reason: None,
        },
    )]);
    assert_eq!(state.history[0].status, DeliveryStatus::Unknown);
    assert_eq!(*calls.lock().unwrap(), vec![vec!["old".to_string()]]);
}

#[test]
fn ownership_claim_in_event_batch_reuses_lookup_and_confirmed_settlement() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.path().join("sidebar.json"));
    state.history[0].status = DeliveryStatus::Queued;
    let receipts = Arc::new(Mutex::new(vec![review::receipt(
        storage::HostInputStatus::Accepted,
    )]));
    let calls = install_counter(&mut state, receipts);
    let host = Arc::new(
        runtime::ownership::OwnerHost::open(
            &dir.path().join("owner"),
            Default::default(),
            Arc::new(event_bus::EventBus::new(16)),
        )
        .unwrap(),
    );
    let owner_id = host.owner_id().to_owned();
    state.ownership = Some(host);
    state.receipt_reconciliation.last_repair = Some(Instant::now());
    state.apply_events([event_bus::Event::new(event_bus::OwnershipEvent {
        thread_id: "thread".into(),
        owner_id,
        generation: 1,
        action: event_bus::OwnershipAction::Claimed,
    })]);
    assert_eq!(*calls.lock().unwrap(), vec![vec!["old".to_string()]]);
    // This sink confirms no transitions; never invent Unknown from a liveness
    // check performed outside the storage/generation guard.
    assert_eq!(state.history[0].status, DeliveryStatus::Queued);
}

#[test]
fn logic_and_repeated_ui_passes_share_one_receipt_lookup_per_frame() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.path().join("sidebar.json"));
    state.history[0].status = DeliveryStatus::Queued;
    state.rebuild_delivery_rows();
    let calls = install_counter(&mut state, Default::default());
    let ctx = egui::Context::default();
    let mut output = ctx.run_ui(Default::default(), |ui| {
        state.logic(ui.ctx());
        state.ui(ui, &mut eframe::Frame::_new_kittest());
    });
    output.textures_delta.clear();
    assert_eq!(calls.lock().unwrap().len(), 1);
    let _ = ctx.run_logic(&Default::default(), |ctx| state.logic(ctx));
    assert_eq!(calls.lock().unwrap().len(), 2);
    let _ = ctx.run_logic(&Default::default(), |ctx| state.logic(ctx));
    assert_eq!(calls.lock().unwrap().len(), 3);
}
