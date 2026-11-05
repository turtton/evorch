//! PR C: notifications are append-only provider input, not mutable tool output.
use super::*;
use runtime::lane_delivery::{LaneDeliveryHost, LaneDeliveryOutcome, LaneDeliveryTarget};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
fn chat_authority() -> RunConfig {
    RunConfig {
        conversation: true,
        ..Default::default()
    }
}
mod nudge;
struct Host {
    thread: String,
    target: LaneDeliveryTarget,
    denied: AtomicBool,
    checked: Notify,
    restores: AtomicUsize,
}
impl LaneDeliveryHost for Host {
    fn resolve(
        &self,
        thread: &str,
    ) -> Result<LaneDeliveryTarget, storage::lane_delivery::LaneDeliveryNack> {
        self.checked.notify_one();
        if self.denied.load(Ordering::SeqCst) {
            return Err(storage::lane_delivery::LaneDeliveryNack::Readonly);
        }
        if thread != self.thread {
            return Err(storage::lane_delivery::LaneDeliveryNack::UnknownThread);
        }
        Ok(self.target.clone())
    }
    fn restore_root(
        &self,
        runtime: &AgentRuntime,
        target: &LaneDeliveryTarget,
    ) -> Result<(), storage::lane_delivery::LaneDeliveryNack> {
        self.restores.fetch_add(1, Ordering::SeqCst);
        runtime
            .wake_chat(target.root_run_id, target.authority.clone())
            .map(|_| ())
            .map_err(|_| storage::lane_delivery::LaneDeliveryNack::RestoreFailed)
    }
    fn now_ns(&self) -> i64 {
        100
    }
}
struct Fixture {
    h: Harness,
    store: storage::Storage,
    config: storage::StorageConfig,
    lead: RunId,
    lanes: Vec<String>,
    host: Arc<Host>,
    events: Vec<Event>,
    request_tool: &'static str,
    request_marker: Option<String>,
}
impl Fixture {
    async fn new() -> Self {
        let mut h = harness(vec![text_response("delivery lead idle")], 1_000_000);
        let config = storage::StorageConfig {
            db_path: h._directory.path().join("delivery-cache.db"),
            ..Default::default()
        };
        let store = storage::Storage::open(config.clone()).unwrap();
        h.runtime = h
            .runtime
            .with_run_store(runtime::RunStore::open(&config, store.handle()).unwrap());
        let lead = h
            .runtime
            .delegate_chat(
                "cache-delivery-lead",
                Role::Orchestrator,
                "cache delivery task".into(),
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    conversation: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut events = through_phase(&mut h.receiver, lead, AgentRunPhase::Waiting).await;
        open(
            &mut h,
            lead,
            "delivery-open-one",
            "delivery first lane",
            &mut events,
        )
        .await;
        let first = result(&h, "delivery-open-one")["lane_id"]
            .as_str()
            .unwrap()
            .to_owned();
        open(
            &mut h,
            lead,
            "delivery-open-two",
            "delivery second lane",
            &mut events,
        )
        .await;
        let second = result(&h, "delivery-open-two")["lane_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let host = Arc::new(Host {
            thread: "cache-delivery-lead".into(),
            target: LaneDeliveryTarget {
                root_run_id: lead,
                generation: 1,
                authority: chat_authority(),
            },
            denied: AtomicBool::new(false),
            checked: Notify::new(),
            restores: AtomicUsize::new(0),
        });
        // Abort just the optional timer task before its first poll: explicit drives in
        // this fixture are the lifecycle barriers, while the real driver is unchanged.
        if let Some(task) = h.runtime.bind_lane_delivery_host(host.clone()) {
            task.abort();
        }
        Self {
            h,
            store,
            config,
            lead,
            lanes: vec![first, second],
            host,
            events,
            request_tool: "lane_open",
            request_marker: None,
        }
    }
    fn report(&self, index: usize, status: event_bus::LaneStatus, revision: u32) {
        let report = self
            .store
            .handle()
            .append_lane_report_with_delivery(
                self.lanes[index].clone(),
                event_bus::LaneReportInput {
                    status,
                    summary: format!("immutable artifact revision {revision}"),
                    artifact_refs: vec![format!("artifact:{revision}")],
                    artifact_revision: Some(event_bus::lane::ArtifactRevision::CommitSha(format!(
                        "{revision:040x}"
                    ))),
                    failure_episode: None,
                    needs_human: false,
                    question_refs: vec![],
                    reason_code: None,
                    producer: event_bus::LaneReportProducer::LaneAgent,
                },
                100,
            )
            .unwrap()
            .report;
        self.h.runtime.record_lane_report_progress(&report).unwrap();
    }
    async fn finish(&mut self) {
        self.h.mock.enqueue(vec![call(
            "delivery-finish",
            "finish",
            json!({"result":"lead finished independently"}),
        )]);
        self.h
            .runtime
            .send_message(self.lead, "finish independently".into())
            .unwrap();
        self.events
            .extend(through_phase(&mut self.h.receiver, self.lead, AgentRunPhase::Done).await);
        self.h.runtime.wait(self.lead).await.unwrap();
    }
    async fn delivered_request(&mut self) {
        loop {
            let event = self.h.receiver.recv().await.unwrap();
            let done = matches!(&event.kind,EventKind::Provider(ProviderEvent::RequestCompleted{run_id:Some(run),..})if run==&self.lead.to_string());
            self.events.push(event);
            if done {
                break;
            }
        }
        self.events
            .extend(through_phase(&mut self.h.receiver, self.lead, AgentRunPhase::Waiting).await);
    }
    async fn followup(&mut self, text: &str) {
        input(
            &mut self.h,
            self.lead,
            text,
            vec![text_response("ordinary followup")],
            &mut self.events,
        )
        .await;
    }
    fn requests(&self) -> Vec<Value> {
        self.h
            .mock
            .recorded_requests()
            .iter()
            .filter(|r| {
                r.path == Wire::ChatCompletions.path()
                    && self
                        .request_marker
                        .as_ref()
                        .is_none_or(|marker| r.body["messages"].to_string().contains(marker))
                    && r.body["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|t| t["function"]["name"] == self.request_tool)
            })
            .map(|r| r.body.clone())
            .collect()
    }
    fn verify(&self, compactions: usize) {
        let requests = self.requests();
        // UsageEvent has no run identity, so the shared single-root aggregate
        // helper cannot attribute the two independently running lane roots.
        // Pair this root's actual provider events with independently captured wire.
        let mut starts = Vec::new();
        let mut completed = std::collections::HashMap::new();
        let mut checkpoint = None;
        let mut checkpoint_count = 0;
        for event in &self.events {
            match &event.kind {
                EventKind::Compaction(CompactionEvent::Compacted {
                    run_id,
                    checkpoint_id,
                    ..
                }) if run_id == &self.lead.to_string() => {
                    assert!(checkpoint.replace(checkpoint_id.clone()).is_none());
                    checkpoint_count += 1;
                }
                EventKind::Provider(ProviderEvent::RequestStarted {
                    request_id,
                    run_id: Some(run_id),
                    purpose,
                    ..
                }) if run_id == &self.lead.to_string()
                    && *purpose != Some(RequestPurpose::Compaction) =>
                {
                    starts.push((request_id.clone(), checkpoint.take()));
                }
                EventKind::Provider(ProviderEvent::RequestCompleted {
                    request_id,
                    run_id: Some(run_id),
                    input_tokens,
                    cache_read_tokens,
                    ..
                }) if run_id == &self.lead.to_string() => {
                    assert!(
                        completed
                            .insert(request_id.clone(), (*input_tokens, *cache_read_tokens))
                            .is_none()
                    );
                }
                EventKind::Diagnostic(d) => assert_ne!(d.code, "CacheRegression", "{d:?}"),
                _ => {}
            }
        }
        assert_eq!(checkpoint_count, compactions);
        assert!(checkpoint.is_none());
        assert_eq!(starts.len(), requests.len());
        assert_eq!(completed.len(), requests.len());
        assert!(requests.len() >= 3);
        for (index, (id, checkpoint)) in starts.iter().enumerate() {
            let (input, cached) = completed[id];
            assert!(input > 0 && cached <= input);
            if index == 0 {
                // Lane requests start after the lead and may already reuse the
                // common instruction prefix. Only the first lead starts cold.
                if self.request_marker.is_none() {
                    assert_eq!(cached, 0);
                }
                continue;
            }
            let previous = completed[&starts[index - 1].0].0;
            if let Some(checkpoint) = checkpoint {
                assert_compaction_transition(
                    CacheProtocol::OpenAi,
                    &requests[index - 1],
                    &requests[index],
                    checkpoint,
                )
                .unwrap();
                assert!(cached < previous);
            } else {
                assert_append_only(
                    CacheProtocol::OpenAi,
                    &requests[index - 1],
                    &requests[index],
                )
                .unwrap();
                assert!(cached >= previous);
            }
        }
        assert!(completed.values().any(|(_, cached)| *cached > 0));
    }
    fn sql(&self, sql: &str) {
        rusqlite::Connection::open(&self.config.db_path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }
}
#[tokio::test]
async fn important_batch_completed_wake_and_deferred_working_preserve_wire_prefix() {
    let mut f = Fixture::new().await;
    f.finish().await;
    // (6) Non-waking updates stay durable while normally finished.
    f.report(0, event_bus::LaneStatus::Working, 1);
    assert_eq!(
        f.h.runtime
            .drive_lane_deliveries("cache-delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Empty
    );
    assert_eq!(f.host.restores.load(Ordering::SeqCst), 0);
    // A genuine next turn makes the deferred batch eligible. Deny only the
    // automatic source trigger, then use the explicit driver completion barrier.
    f.host.denied.store(true, Ordering::SeqCst);
    f.h.mock
        .enqueue(vec![text_response("new actual user turn")]);
    f.h.runtime
        .continue_goal(f.lead, "next real user turn".into(), chat_authority())
        .unwrap();
    f.delivered_request().await;
    f.host.checked.notified().await;
    f.host.denied.store(false, Ordering::SeqCst);
    f.h.mock
        .enqueue(vec![text_response("working aggregate received")]);
    assert_eq!(
        f.h.runtime
            .drive_lane_deliveries("cache-delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    f.delivered_request().await;
    f.finish().await;
    // (3,4) Two sources are one important batch and one completed-root restore.
    f.report(0, event_bus::LaneStatus::Working, 4);
    assert_eq!(
        f.h.runtime
            .drive_lane_deliveries("cache-delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Empty
    );
    f.report(0, event_bus::LaneStatus::Ready, 2);
    f.report(1, event_bus::LaneStatus::Ready, 3);
    f.h.mock
        .enqueue(vec![text_response("important batch received")]);
    assert_eq!(
        f.h.runtime
            .drive_lane_deliveries("cache-delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Delivered { notifications: 3 }
    );
    f.delivered_request().await;
    assert_eq!(f.host.restores.load(Ordering::SeqCst), 1);
    f.followup("ordinary turn after wake").await;
    let requests = f.requests();
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    let last = requests.last().unwrap().to_string();
    assert!(
        last.contains("immutable artifact revision 2")
            && last.contains("immutable artifact revision 3")
            && last.contains("immutable artifact revision 4")
    );
    f.verify(0);
    f.h.runtime.cancel(f.lead).unwrap();
    f.h.runtime.wait(f.lead).await.unwrap();
}
#[tokio::test]
async fn receipt_ack_retry_stop_resume_and_compaction_restore_never_duplicate_notifications() {
    let mut f = Fixture::new().await;
    // (8) Stop never spends a wake or admits the notification; explicit resume is
    // the scheduling boundary, not artifact progress, ACK or a restored generation.
    f.h.runtime
        .stop(f.lead, runtime::StopScope::SelfOnly)
        .unwrap();
    f.events
        .extend(through_phase(&mut f.h.receiver, f.lead, AgentRunPhase::Stopped).await);
    f.h.runtime.wait(f.lead).await.unwrap();
    f.report(0, event_bus::LaneStatus::Ready, 4);
    assert!(matches!(
        f.h.runtime
            .drive_lane_deliveries("cache-delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Stopped
            | LaneDeliveryOutcome::Nacked(storage::lane_delivery::LaneDeliveryNack::Stopped)
    ));
    // (7) Crash-equivalent failure after receiver commit, before sender ACK.
    f.sql("CREATE TRIGGER fail_cache_ack BEFORE UPDATE OF state ON lane_notifications WHEN NEW.state='acked' BEGIN SELECT RAISE(ABORT,'injected ACK failure'); END;");
    f.h.mock
        .enqueue(vec![text_response("resumed notification accepted")]);
    f.h.runtime.resume_chat(f.lead, chat_authority()).unwrap();
    f.delivered_request().await;
    // ACK failure, not provider completion, is the sender's completion barrier.
    while !f
        .events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::Diagnostic(d) if d.code == "lane_delivery_failed"))
    {
        f.events.push(f.h.receiver.recv().await.unwrap());
    }
    // Keep the received notification unacknowledged across the history boundary.
    f.followup("ordinary turn before ACK repair").await;
    for pair in f.requests().windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    // Give structural compaction old, independently retained data to replace.
    input(
        &mut f.h,
        f.lead,
        "retain a large old turn",
        vec![read_response(1), text_response("large old turn complete")],
        &mut f.events,
    )
    .await;
    f.h.runtime
        .stop(f.lead, runtime::StopScope::SelfOnly)
        .unwrap();
    f.events
        .extend(through_phase(&mut f.h.receiver, f.lead, AgentRunPhase::Stopped).await);
    f.h.runtime.wait(f.lead).await.unwrap();
    // (9) Receipts are independent of both compacted history and the restored root.
    f.h.runtime
        .compact_saved_chat(f.lead, chat_authority())
        .await
        .unwrap();
    loop {
        let event = f.h.receiver.recv().await.unwrap();
        let done = matches!(&event.kind,EventKind::Compaction(CompactionEvent::Compacted{run_id,..})if run_id==&f.lead.to_string());
        f.events.push(event);
        if done {
            break;
        }
    }
    let failures_before_restore = f
        .events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::Diagnostic(d) if d.code == "lane_delivery_failed"))
        .count();
    f.h.runtime.resume_chat(f.lead, chat_authority()).unwrap();
    f.events
        .extend(through_phase(&mut f.h.receiver, f.lead, AgentRunPhase::Waiting).await);
    // A queued resume-triggered driver must finish before removing the injected
    // ACK failure. Its diagnostic is distinct from the pre-compaction failure.
    while f
        .events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::Diagnostic(d) if d.code == "lane_delivery_failed"))
        .count()
        <= failures_before_restore
    {
        f.events.push(f.h.receiver.recv().await.unwrap());
    }
    f.sql("DROP TRIGGER fail_cache_ack");
    let before = f.requests().len();
    assert_eq!(
        f.h.runtime
            .drive_lane_deliveries("cache-delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Empty
    );
    assert_eq!(f.requests().len(), before);
    f.followup("first post-compaction ordinary turn").await;
    f.followup("reuse the post-compaction prefix").await;
    let count:usize=rusqlite::Connection::open(&f.config.db_path).unwrap().query_row("SELECT count(*) FROM host_input_receipts WHERE json_extract(provenance_json,'$.kind')='lane_notification'",[],|r|r.get(0)).unwrap();
    assert_eq!(count, 1);
    f.verify(1);
    f.h.runtime.cancel(f.lead).unwrap();
    f.h.runtime.wait(f.lead).await.unwrap();
}
/// (5) A direct user input to a lane the lead manages: the input reaches the lane only and
/// the lead gets a reference-only notice, for an attached and for a summary lane alike.
async fn direct_lane_input_case(summary: bool) {
    let mut f = Fixture::new().await;
    let (lane_id, lane_root) = if summary {
        f.h.mock.enqueue(vec![text_response("summary lane idle")]);
        let opened =
            f.h.runtime
                .open_lane(
                    f.lead,
                    "summary-direct-input",
                    event_bus::LaneBrief {
                        verification_requirements: Default::default(),
                        objective: "summary lane".into(),
                        inputs: vec![],
                        deliverables: vec!["artifact".into()],
                        acceptance_criteria: vec!["review".into()],
                        constraints: vec![],
                        workspace: Default::default(),
                        report_policy: event_bus::LanePolicy::Summary,
                    },
                )
                .unwrap();
        f.events.extend(
            through_phase(
                &mut f.h.receiver,
                opened.root_run_id,
                AgentRunPhase::Waiting,
            )
            .await,
        );
        (opened.lane_id, opened.root_run_id)
    } else {
        let lane_id = f.lanes[0].clone();
        let root: RunId = f
            .store
            .handle()
            .lane(&lane_id)
            .unwrap()
            .unwrap()
            .root_run_id
            .unwrap()
            .parse()
            .unwrap();
        (lane_id, root)
    };
    // (5) The user talks to the lane directly: the input reaches the lane only, and the
    // runtime saves a reference-only intervention notice for its lead. Deny only the
    // automatic source trigger so that the explicit driver is the completion barrier.
    f.host.denied.store(true, Ordering::SeqCst);
    f.h.mock
        .enqueue(vec![text_response("lane saw the direct input")]);
    f.h.runtime
        .send_message(lane_root, "private direct input for the lane".into())
        .unwrap();
    f.events
        .extend(through_phase(&mut f.h.receiver, lane_root, AgentRunPhase::Waiting).await);
    f.host.checked.notified().await;
    f.host.denied.store(false, Ordering::SeqCst);

    f.h.mock
        .enqueue(vec![text_response("intervention received")]);
    assert_eq!(
        f.h.runtime
            .drive_lane_deliveries("cache-delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
    f.delivered_request().await;
    assert_eq!(
        f.host.restores.load(Ordering::SeqCst),
        0,
        "a live lead is not restored"
    );
    f.followup("ordinary turn after the intervention notice")
        .await;

    let requests = f.requests();
    for pair in requests.windows(2) {
        assert_append_only(CacheProtocol::OpenAi, &pair[0], &pair[1]).unwrap();
    }
    for request in &requests {
        assert!(
            !request.to_string().contains("private direct input"),
            "the input body is never copied to the lead"
        );
    }
    assert!(requests.last().unwrap().to_string().contains(&lane_id));
    f.verify(0);
    f.h.runtime.cancel(f.lead).unwrap();
    f.h.runtime.wait(f.lead).await.unwrap();
}
#[tokio::test]
async fn direct_input_to_an_attached_lane_notifies_the_live_lead_by_reference_and_preserves_wire_prefix()
 {
    direct_lane_input_case(false).await;
}
#[tokio::test]
async fn direct_input_to_a_summary_lane_notifies_the_live_lead_by_reference_and_preserves_wire_prefix()
 {
    direct_lane_input_case(true).await;
}
