//! Actual provider input-derived cache and independent wire-prefix assertions.
use super::*;
use event_bus::lane::{LaneNudgeState, LaneStallClassification, LaneStallEpisode};
use storage::lane_control::ObserveLaneStall;
pub(super) struct NudgeFixture {
    pub(super) f: Fixture,
    _owner: Arc<runtime::ownership::OwnerHost>,
    pub(super) observation: ObserveLaneStall,
}
impl NudgeFixture {
    pub(super) async fn new() -> Self {
        let mut f = Fixture::new().await;
        let lane = f.store.handle().lane(&f.lanes[0]).unwrap().unwrap();
        f.lead = lane.root_run_id.as_deref().unwrap().parse().unwrap();
        f.request_tool = "lane_report";
        f.request_marker = Some("delivery first lane".into());
        let owner = Arc::new(
            runtime::ownership::OwnerHost::open(
                f.h._directory.sockets(),
                Default::default(),
                Arc::new(event_bus::EventBus::new(128)),
            )
            .unwrap(),
        );
        let permit = owner.start(&lane.thread_id).unwrap();
        let delivery_owner = storage::lane_delivery::LaneDeliveryOwner {
            root_run_id: f.lead.to_string(),
            generation: lane.generation,
            ownership_generation: permit.lease.generation,
        };
        f.store
            .handle()
            .bind_lane_delivery_destination(
                lane.thread_id.clone(),
                0,
                delivery_owner.clone(),
                storage::lane_delivery::LaneDestinationAction::Bind,
            )
            .unwrap();
        let host = Arc::new(Host {
            thread: lane.thread_id,
            target: LaneDeliveryTarget {
                root_run_id: f.lead,
                generation: lane.generation,
                authority: RunConfig {
                    ownership: Some(permit),
                    conversation: true,
                    ..Default::default()
                },
            },
            denied: AtomicBool::new(false),
            checked: Notify::new(),
            restores: AtomicUsize::new(0),
        });
        f.h.runtime.bind_lane_delivery_host(host.clone());
        f.host = host;
        let observation = ObserveLaneStall {
            lane_id: lane.lane_id.clone(),
            management_revision: lane.management_revision,
            owner: delivery_owner,
            progress_anchor: f.store.handle().lane_progress_anchor(lane.lane_id).unwrap(),
            classification: LaneStallClassification::NoReport,
            at_ns: 100,
        };
        Self {
            f,
            _owner: owner,
            observation,
        }
    }
    fn queue(&self) -> LaneStallEpisode {
        let e = self
            .f
            .store
            .handle()
            .observe_lane_stall(self.observation.clone())
            .unwrap();
        self.f
            .store
            .handle()
            .request_lane_nudge(self.observation.clone(), e.episode_id, e.revision)
            .unwrap()
    }
    pub(super) fn receipts(&self) -> usize {
        rusqlite::Connection::open(&self.f.config.db_path).unwrap().query_row("SELECT count(*) FROM host_input_receipts WHERE json_extract(provenance_json,'$.kind')='lane_notification'",[],|r|r.get(0)).unwrap()
    }
    pub(super) async fn drive(&mut self) -> Result<LaneDeliveryOutcome, runtime::lanes::LaneError> {
        self.f
            .h
            .runtime
            .drive_lane_deliveries(&self.f.host.thread)
            .await
    }
    pub(super) async fn stopped(&mut self) {
        self.f
            .h
            .runtime
            .stop(self.f.lead, runtime::StopScope::SelfOnly)
            .unwrap();
        self.f.events.extend(
            through_phase(&mut self.f.h.receiver, self.f.lead, AgentRunPhase::Stopped).await,
        );
        self.f.h.runtime.wait(self.f.lead).await.unwrap();
    }
    pub(super) fn failures(&self) -> usize {
        self.f
            .events
            .iter()
            .filter(
                |e| matches!(&e.kind,EventKind::Diagnostic(d) if d.code=="lane_delivery_failed"),
            )
            .count()
    }
    pub(super) async fn next_failure(&mut self, previous: usize) {
        while self.failures() <= previous {
            self.f.events.push(self.f.h.receiver.recv().await.unwrap());
        }
    }
    pub(super) async fn cancel(self) {
        self.f.h.runtime.cancel(self.f.lead).unwrap();
        self.f.h.runtime.wait(self.f.lead).await.unwrap();
    }
}
#[tokio::test]
async fn live_nudge_and_silent_done_wake_keep_wire_prefix_and_share_durable_one_shot() {
    for done in [false, true] {
        let mut n = NudgeFixture::new().await;
        if done {
            n.f.finish().await;
        }
        n.observation.progress_anchor =
            n.f.store
                .handle()
                .lane_progress_anchor(n.observation.lane_id.clone())
                .unwrap();
        let pending = n.queue();
        n.f.h.mock.enqueue(if done {
            vec![call("nudge-working", "lane_report", json!({"status":"working","summary":"new working prose after nudge","artifact_refs":[]})),
                 call("nudge-finish", "finish", json!({"result":"silent normal completion"}))]
        } else { vec![text_response("live nudge received")] });
        assert_eq!(
            n.drive().await.unwrap(),
            LaneDeliveryOutcome::Delivered { notifications: 1 }
        );
        if done {
            n.f.events
                .extend(through_phase(&mut n.f.h.receiver, n.f.lead, AgentRunPhase::Done).await);
            n.f.h.runtime.wait(n.f.lead).await.unwrap();
        } else {
            n.f.delivered_request().await;
        }
        // State, sequence, summary and question churn cannot buy another nudge.
        for status in [
            event_bus::LaneStatus::Waiting,
            event_bus::LaneStatus::Working,
            event_bus::LaneStatus::Waiting,
            event_bus::LaneStatus::Working,
        ] {
            n.f.store
                .handle()
                .append_lane_report_with_delivery(
                    n.observation.lane_id.clone(),
                    1,
                    event_bus::LaneReportInput {
                        status,
                        summary: "state churn without artifacts".into(),
                        artifact_refs: vec![],
                        artifact_revision: None,
                        failure_episode: None,
                        needs_human: false,
                        question_refs: vec!["question-changing".into()],
                        reason_code: None,
                        producer: event_bus::LaneReportProducer::LaneAgent,
                    },
                    101,
                )
                .unwrap();
        }
        assert_eq!(
            n.f.store
                .handle()
                .lane_progress_anchor(n.observation.lane_id.clone())
                .unwrap(),
            n.observation.progress_anchor
        );
        let accepted = n.queue();
        assert_eq!(accepted.nudge_state, LaneNudgeState::Accepted);
        assert_eq!(accepted.notification_id, pending.notification_id);
        let before = n.f.requests().len();
        assert_eq!(n.drive().await.unwrap(), LaneDeliveryOutcome::Empty);
        assert_eq!(n.f.requests().len(), before);
        assert_eq!(n.receipts(), 1);
        assert_eq!(n.f.host.restores.load(Ordering::SeqCst), usize::from(done));
        if !done {
            n.f.followup("reuse after durable nudge").await;
        }
        assert!(
            n.f.requests()
                .iter()
                .any(|r| r.to_string().contains("Runtime nudge"))
        );
        n.f.verify(0);
        if !done {
            n.cancel().await;
        }
    }
}
#[tokio::test]
async fn nudge_receipt_survives_stop_ack_failure_compaction_and_restore_without_reinjection() {
    let mut n = NudgeFixture::new().await;
    let episode = n.queue();
    n.stopped().await;
    assert!(matches!(
        n.drive().await.unwrap(),
        LaneDeliveryOutcome::Stopped
            | LaneDeliveryOutcome::Nacked(storage::lane_delivery::LaneDeliveryNack::Stopped)
    ));
    assert_eq!(n.receipts(), 0);
    n.f.sql("CREATE TRIGGER fail_nudge_ack BEFORE UPDATE OF state ON lane_notifications WHEN NEW.state='acked' BEGIN SELECT RAISE(ABORT,'injected ACK failure'); END;");
    n.f.h
        .mock
        .enqueue(vec![text_response("nudge accepted after explicit resume")]);
    n.f.h
        .runtime
        .resume_chat(n.f.lead, n.f.host.target.authority.clone())
        .unwrap();
    n.f.delivered_request().await;
    n.next_failure(0).await;
    assert_eq!(n.receipts(), 1);
    input(
        &mut n.f.h,
        n.f.lead,
        "large old context",
        vec![read_response(1), text_response("large turn complete")],
        &mut n.f.events,
    )
    .await;
    n.stopped().await;
    n.f.h
        .runtime
        .compact_saved_chat(n.f.lead, n.f.host.target.authority.clone())
        .await
        .unwrap();
    loop {
        let e = n.f.h.receiver.recv().await.unwrap();
        let done = matches!(&e.kind,EventKind::Compaction(CompactionEvent::Compacted{run_id,..})if run_id==&n.f.lead.to_string());
        n.f.events.push(e);
        if done {
            break;
        }
    }
    let failures = n.failures();
    n.f.h
        .runtime
        .resume_chat(n.f.lead, n.f.host.target.authority.clone())
        .unwrap();
    n.f.events
        .extend(through_phase(&mut n.f.h.receiver, n.f.lead, AgentRunPhase::Waiting).await);
    n.next_failure(failures).await;
    n.f.sql("DROP TRIGGER fail_nudge_ack");
    assert_eq!(n.drive().await.unwrap(), LaneDeliveryOutcome::Empty);
    assert_eq!(n.receipts(), 1);
    assert_eq!(
        n.f.store
            .handle()
            .lane_stall_episode(episode.episode_id)
            .unwrap()
            .unwrap()
            .nudge_state,
        LaneNudgeState::Accepted
    );
    n.f.followup("first new compacted prefix").await;
    n.f.followup("reuse compacted prefix").await;
    n.f.verify(1);
    n.cancel().await;
}
