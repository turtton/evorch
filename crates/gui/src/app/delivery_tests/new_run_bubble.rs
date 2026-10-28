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
    assert!(state.sending.is_empty());
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
    assert!(state.sending.is_empty());
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
