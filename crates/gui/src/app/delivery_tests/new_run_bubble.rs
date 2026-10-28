//! A message that starts a run is shown as sent at once; only a message that
//! waits in a live run's inbox stays above the composer until delivered.
use super::*;
use crate::model::transcript::TranscriptEntry;

/// Receipt-backed sink whose run is live (inbox open) or not yet started.
struct Sink {
    receipts: Arc<Mutex<Vec<storage::HostInputReceipt>>>,
    live: bool,
}
impl CommandSink for Sink {
    fn has_host_input_receipts(&self) -> bool {
        true
    }
    fn host_input_receipts(
        &self,
        ids: &[String],
    ) -> Option<Result<Vec<storage::HostInputReceipt>, storage::StorageError>> {
        let mut receipts = self.receipts.lock().unwrap().clone();
        receipts.retain(|receipt| ids.contains(&receipt.prompt_id));
        Some(Ok(receipts))
    }
    fn follow_up_status(&self, _: &str) -> Option<runtime::FollowUpStatus> {
        self.live.then(runtime::FollowUpStatus::default)
    }
    fn withdraw_follow_up(
        &mut self,
        _: &str,
        prompt_id: &str,
    ) -> Result<(), runtime::RuntimeError> {
        let mut receipts = self.receipts.lock().unwrap();
        let receipt = receipts
            .iter_mut()
            .find(|receipt| receipt.prompt_id == prompt_id)
            .ok_or(runtime::FollowUpWithdrawalError::Unknown)?;
        if receipt.status != storage::HostInputStatus::Accepted {
            return Err(runtime::FollowUpWithdrawalError::AlreadyConsumed.into());
        }
        receipt.status = storage::HostInputStatus::Withdrawn;
        Ok(())
    }
    fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
        Vec::new()
    }
}

fn bubbles(state: &WorkbenchState<DemoSource>) -> Vec<String> {
    state
        .transcripts
        .thread_model("thread")
        .map(|model| model.entries().to_vec())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::UserMessage { text } => Some(text),
            _ => None,
        })
        .collect()
}

fn send(
    live: bool,
) -> (
    WorkbenchState<DemoSource>,
    Arc<Mutex<Vec<storage::HostInputReceipt>>>,
) {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.keep().join("sidebar.json"));
    // Drop the fixture's old undelivered message; this scenario starts clean.
    state.history.clear();
    state.rebuild_delivery_rows();
    let receipts = Arc::new(Mutex::new(Vec::new()));
    state.sink = Box::new(Sink {
        receipts: receipts.clone(),
        live,
    });
    state.composer.input = "hello".into();
    state.submit_composer();
    (state, receipts)
}

fn receipt_for(
    state: &WorkbenchState<DemoSource>,
    status: storage::HostInputStatus,
) -> storage::HostInputReceipt {
    storage::HostInputReceipt {
        prompt_id: state.history[0].id.clone(),
        text: "hello".into(),
        image_count: 0,
        ..super::review::receipt(status)
    }
}

#[test]
fn new_run_message_is_a_bubble_without_a_queued_row() {
    let (mut state, _) = send(false);
    assert_eq!(state.history[0].status, DeliveryStatus::Queued);
    state.reconcile_follow_ups();
    assert!(state.composer.queued.is_empty());
    assert_eq!(bubbles(&state), ["hello"]);
}

#[test]
fn delivered_receipt_keeps_the_single_bubble() {
    let (mut state, receipts) = send(false);
    receipts
        .lock()
        .unwrap()
        .push(receipt_for(&state, storage::HostInputStatus::Delivered));
    state.reconcile_receipts();
    assert_eq!(state.history[0].status, DeliveryStatus::Delivered);
    assert_eq!(bubbles(&state), ["hello"]);
    assert!(state.composer.queued.is_empty());
}

#[test]
fn accepted_receipt_does_not_turn_the_bubble_into_a_queued_row() {
    let (mut state, receipts) = send(false);
    receipts
        .lock()
        .unwrap()
        .push(receipt_for(&state, storage::HostInputStatus::Accepted));
    state.reconcile_receipts();
    state.rebuild_delivery_rows();
    assert_eq!(state.history[0].status, DeliveryStatus::Queued);
    assert!(state.composer.queued.is_empty());
    assert_eq!(bubbles(&state), ["hello"]);
}

#[test]
fn dropped_receipt_withdraws_the_bubble_and_returns_to_undelivered() {
    let (mut state, receipts) = send(false);
    receipts
        .lock()
        .unwrap()
        .push(receipt_for(&state, storage::HostInputStatus::Dropped));
    state.reconcile_receipts();
    assert_eq!(state.history[0].status, DeliveryStatus::Dropped);
    assert!(bubbles(&state).is_empty());
    assert_eq!(state.composer.queued.len(), 1);
    assert!(state.composer.queued[0].undelivered);
}

#[test]
fn live_run_follow_up_still_waits_above_the_composer() {
    let (mut state, receipts) = send(true);
    state.reconcile_follow_ups();
    assert!(bubbles(&state).is_empty());
    assert_eq!(state.composer.queued.len(), 1);
    assert!(!state.composer.queued[0].undelivered);
    receipts
        .lock()
        .unwrap()
        .push(receipt_for(&state, storage::HostInputStatus::Delivered));
    state.reconcile_receipts();
    state.reconcile_follow_ups();
    assert_eq!(bubbles(&state), ["hello"]);
    assert!(state.composer.queued.is_empty());
}

#[test]
fn editing_and_deleting_follow_ups_keeps_the_pending_new_run_bubble() {
    let (mut state, receipts) = send(false);
    let initial_id = state.history[0].id.clone();
    receipts
        .lock()
        .unwrap()
        .push(receipt_for(&state, storage::HostInputStatus::Accepted));
    state.sink = Box::new(Sink {
        receipts: receipts.clone(),
        live: true,
    });
    for text in ["edit follow-up", "delete follow-up"] {
        state.composer.input = text.into();
        state.submit_composer();
    }
    receipts
        .lock()
        .unwrap()
        .extend(
            state
                .history
                .iter()
                .skip(1)
                .map(|message| storage::HostInputReceipt {
                    prompt_id: message.id.clone(),
                    text: message.prompt.clone(),
                    ..super::review::receipt(storage::HostInputStatus::Accepted)
                }),
        );
    state.reconcile_receipts();
    state.reconcile_follow_ups();
    assert_eq!(bubbles(&state), ["hello"]);
    assert_eq!(state.composer.queued.len(), 2);
    assert!(state.history[0].starts_run);
    assert!(
        state
            .history
            .iter()
            .skip(1)
            .all(|message| !message.starts_run)
    );

    state.withdraw_queued_follow_up(0, true);
    assert_eq!(state.composer.input, "edit follow-up");
    assert_eq!(state.composer.queued.len(), 1);
    state.withdraw_queued_follow_up(0, false);
    state.reconcile_receipts();
    state.reconcile_follow_ups();
    assert_eq!(state.composer.input, "edit follow-up");
    assert!(state.composer.queued.is_empty());
    assert_eq!(bubbles(&state), ["hello"]);
    assert_eq!(state.history.len(), 1);
    assert_eq!(state.history[0].id, initial_id);
    assert!(state.history[0].starts_run);
    assert_eq!(state.history[0].status, DeliveryStatus::Queued);
}

/// Reload the saved sidebar against a store holding `receipt`, as after a restart.
fn restart_with(
    state: &WorkbenchState<DemoSource>,
    receipt: storage::HostInputReceipt,
) -> WorkbenchState<DemoSource> {
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    storage
        .handle()
        .append_host_input_accepted(&[], &[], None, &receipt)
        .unwrap();
    let mut restored =
        state_for_reload(state.sidebar_path.as_ref().unwrap(), state.sidebar.clone());
    restored
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    restored
}

#[test]
fn accepted_new_run_message_is_still_a_bubble_after_restart() {
    let (state, _) = send(false);
    let receipt = receipt_for(&state, storage::HostInputStatus::Accepted);
    let restored = restart_with(&state, receipt);
    assert_eq!(restored.history[0].status, DeliveryStatus::Queued);
    assert_eq!(bubbles(&restored), ["hello"]);
    assert!(restored.composer.queued.is_empty());
}

#[test]
fn accepted_live_follow_up_is_still_a_queued_row_after_restart() {
    let (state, _) = send(true);
    let receipt = receipt_for(&state, storage::HostInputStatus::Accepted);
    let restored = restart_with(&state, receipt);
    assert_eq!(restored.history[0].status, DeliveryStatus::Queued);
    assert!(bubbles(&restored).is_empty());
    let rows = &restored.follow_up_queues["thread"];
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].undelivered);
}

#[test]
fn synchronous_refusal_leaves_no_bubble_and_returns_the_text_to_the_composer() {
    struct Refuse;
    impl CommandSink for Refuse {
        fn has_host_input_receipts(&self) -> bool {
            true
        }
        fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
            vec![LoopEvent::ChatRejected {
                thread_id: "thread".into(),
                reason: "sealed".into(),
            }]
        }
    }
    let (mut state, _) = send(false);
    // Fresh submission through a sink that refuses at once.
    state.history.clear();
    state.sink = Box::new(Refuse);
    state.composer.input = "again".into();
    state.submit_composer();
    assert!(matches!(
        state.history[0].status,
        DeliveryStatus::Rejected { .. }
    ));
    assert_eq!(state.composer.input, "again");
    assert!(!bubbles(&state).contains(&"again".to_owned()));
    state.rebuild_delivery_rows();
    assert!(state.composer.queued.is_empty());
}

#[test]
fn refusal_after_the_bubble_withdraws_it() {
    let (mut state, _) = send(false);
    assert_eq!(bubbles(&state), ["hello"]);
    let id = state.history[0].id.clone();
    assert!(state.reject_submission(&id, "sealed"));
    assert!(bubbles(&state).is_empty());
    assert!(matches!(
        state.history[0].status,
        DeliveryStatus::Rejected { .. }
    ));
    // The text is back to edit and send again.
    assert_eq!(state.composer.input, "hello");
}

#[test]
fn unknown_receipt_withdraws_the_bubble_into_an_undelivered_row() {
    let (mut state, receipts) = send(false);
    receipts
        .lock()
        .unwrap()
        .push(receipt_for(&state, storage::HostInputStatus::Unknown));
    state.reconcile_receipts();
    assert_eq!(state.history[0].status, DeliveryStatus::Unknown);
    assert!(bubbles(&state).is_empty());
    assert_eq!(state.composer.queued.len(), 1);
    assert!(state.composer.queued[0].undelivered);
}
