//! Fault/lifecycle contracts. All races have explicit barriers or injected clocks.
use super::*;
use storage::pull_request_auto_fix::*;
use storage::pull_requests::{LinkPullRequest, PullRequestKey};
fn key(number: u64) -> PullRequestKey {
    PullRequestKey::github("owner/repo", number).unwrap()
}
fn notice(f: &Fixture, head: u32, number: u64, mode: PrNoticeMode) -> PrNotice {
    let h = f.storage.handle();
    f.sql("INSERT OR IGNORE INTO threads(thread_id,archived,created_at_ns,updated_at_ns,source) VALUES ('delivery-lead',0,1,1,'runtime')");
    if !h
        .active_pull_request_links(0, 200)
        .unwrap()
        .iter()
        .any(|l| l.key == key(number))
    {
        // Each test uses one active PR on the lead.
        h.link_pull_request(LinkPullRequest {
            thread_id: "delivery-lead".into(),
            key: key(number),
            expected_revision: 0,
            at_ns: 1,
        })
        .unwrap();
    }
    let mut policy = h.pr_auto_fix_state(key(number)).unwrap();
    if mode == PrNoticeMode::Auto && !policy.enabled {
        policy = h
            .update_pr_auto_fix_policy(PrPolicyUpdate {
                key: key(number),
                expected_policy_revision: policy.policy_revision,
                expected_destination_thread_id: policy.destination_thread_id,
                destination_thread_id: "delivery-lead".into(),
                link_revision: 1,
                action: PrPolicyAction::SetEnabled(true),
            })
            .unwrap();
    }
    let n = h
        .reserve_pr_notice(PrNoticeReservation {
            key: key(number),
            head_sha: format!("{head:040x}"),
            mode,
            destination_thread_id: "delivery-lead".into(),
            link_revision: 1,
            expected_policy_revision: policy.policy_revision,
            success_seq: 1,
            auto_fix_enabled: true,
            now_ns: 100,
        })
        .unwrap();
    let payload =
        prepare_pr_notice_payload(&n, &["unit: failure".into()], "2026-01-01T00:00:00Z", None);
    h.ready_pr_notice(n.notice_id, payload).unwrap()
}
fn occurrences(f: &Fixture, run: RunId, head: u32) -> usize {
    let record = f
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    let messages: Vec<Message> = serde_json::from_str(&record.messages_json).unwrap();
    messages.iter().flat_map(|m| &m.content).filter(|b| matches!(b,ContentBlock::Text{text} if text.contains(&format!("head={head:040x}]")))).count()
}
fn phase(f: &Fixture, n: &PrNotice) -> PrNoticePhase {
    f.storage
        .handle()
        .pr_notice(n.notice_id.clone())
        .unwrap()
        .unwrap()
        .phase
}
fn internal(f: &Fixture, run: RunId) {
    let review = f.runtime.shared.review_runs.lock().unwrap();
    let requests = review[&run].requests().0;
    assert_eq!(
        requests.iter().map(|r| r.text.as_str()).collect::<Vec<_>>(),
        requests
            .iter()
            .map(|r| r.text.as_str())
            .filter(|t| matches!(*t, "coordinate" | "finish independently"))
            .collect::<Vec<_>>()
    );
}
async fn delivered(f: &Fixture) {
    assert_eq!(
        f.runtime
            .drive_lane_deliveries("delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Delivered { notifications: 1 }
    );
}
#[tokio::test]
async fn waiting_and_done_use_harness_receipts_without_lane_credit_or_human_authority() {
    let mut f = Fixture::new();
    let (run, host) = f.lead().await;
    let n = notice(&f, 1, 42, PrNoticeMode::Manual);
    let before = f.credits(run);
    delivered(&f).await;
    f.phase(run, AgentRunPhase::Waiting).await;
    assert_eq!(occurrences(&f, run, 1), 1);
    assert_eq!(phase(&f, &n), PrNoticePhase::Adopted);
    internal(&f, run);
    assert_eq!(f.credits(run), before);
    assert_eq!(
        f.runtime
            .drive_lane_deliveries("delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Empty
    );
    f.finish(run).await;
    let n = notice(&f, 2, 42, PrNoticeMode::Manual);
    delivered(&f).await;
    f.runtime.wait(run).await.unwrap();
    assert_eq!(occurrences(&f, run, 2), 1);
    assert_eq!(phase(&f, &n), PrNoticePhase::Adopted);
    assert_eq!(host.restores.load(Ordering::SeqCst), 1);
    internal(&f, run);
    let guard = f.storage.handle().pr_wake_guard(key(42)).unwrap();
    assert_eq!(guard.no_progress_batches, 1);
    let receipts = f
        .storage
        .handle()
        .host_input_receipts_for_run(&run.to_string())
        .unwrap();
    // Public human-history readers must exclude internal delivery receipts.
    assert!(
        receipts
            .iter()
            .all(|r| r.provenance == storage::HostInputProvenance::User)
    );
    let conn = rusqlite::Connection::open(&f.config.db_path).unwrap();
    let (provenance, text, images, evidence): (String, String, i64, Option<String>) = conn.query_row(
        "SELECT provenance_json,text,image_count,evidence_id FROM host_input_receipts WHERE json_extract(provenance_json,'$.notification_id')=?1",
        [&n.notice_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    ).unwrap();
    assert_eq!(
        serde_json::from_str::<storage::HostInputProvenance>(&provenance).unwrap(),
        storage::HostInputProvenance::HarnessNotice {
            notification_id: n.notice_id,
            destination_thread_id: "delivery-lead".into()
        }
    );
    assert!(text.is_empty());
    assert_eq!((images, evidence), (0, None));
}
#[tokio::test]
async fn failed_restore_admission_retries_same_notice_without_explicit_resume() {
    let mut f = Fixture::new();
    let (run, host) = f.lead().await;
    f.finish(run).await;
    let n = notice(&f, 1, 42, PrNoticeMode::Manual);
    f.model.admission_required.store(true, Ordering::SeqCst);
    f.model.fail_admission_next.store(true, Ordering::SeqCst);
    assert_eq!(
        f.runtime
            .drive_lane_deliveries("delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::RestoreFailed)
    );
    assert_eq!(phase(&f, &n), PrNoticePhase::Ready);
    assert_eq!(occurrences(&f, run, 1), 0);
    assert!(f.runtime.wait_admission(run).await.is_err());
    // A new delivery kick, not ExplicitUserResume, retries the durable notice.
    delivered(&f).await;
    f.runtime.wait(run).await.unwrap();
    assert_eq!(phase(&f, &n), PrNoticePhase::Adopted);
    assert_eq!(occurrences(&f, run, 1), 1);
    assert_eq!(host.restores.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn commit_failure_retries_same_id_and_ack_failure_never_reappends() {
    let mut f = Fixture::new();
    let (run, _) = f.lead().await;
    let n = notice(&f, 1, 42, PrNoticeMode::Auto);
    f.sql("CREATE TRIGGER fail_pr_receipt BEFORE INSERT ON host_input_receipts WHEN json_extract(NEW.provenance_json,'$.kind')='harness_notice' BEGIN SELECT RAISE(ABORT,'injected'); END;");
    assert_eq!(
        f.runtime
            .drive_lane_deliveries("delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::StorageFailed)
    );
    assert_eq!(phase(&f, &n), PrNoticePhase::Ready);
    assert_eq!(occurrences(&f, run, 1), 0);
    let policy = f.storage.handle().pr_auto_fix_state(key(42)).unwrap();
    assert_eq!((policy.used, policy.reserved), (0, 1));
    f.sql("DROP TRIGGER fail_pr_receipt; CREATE TRIGGER fail_pr_ack BEFORE UPDATE OF state ON lane_notifications WHEN NEW.state='acked' AND NEW.source_kind='pull_request_checks_failed' BEGIN SELECT RAISE(ABORT,'injected'); END;");
    assert!(
        f.runtime
            .drive_lane_deliveries("delivery-lead")
            .await
            .is_err()
    );
    f.phase(run, AgentRunPhase::Waiting).await;
    assert_eq!(phase(&f, &n), PrNoticePhase::Adopted);
    assert_eq!(occurrences(&f, run, 1), 1);
    let policy = f.storage.handle().pr_auto_fix_state(key(42)).unwrap();
    assert_eq!((policy.used, policy.reserved), (1, 0));
    f.sql("DROP TRIGGER fail_pr_ack;");
    f.runtime.drive_pending_lane_deliveries().await.unwrap();
    assert!(
        f.storage
            .handle()
            .pending_pr_delivery_destinations(String::new())
            .unwrap()
            .is_empty()
    );
    assert_eq!(occurrences(&f, run, 1), 1);
    internal(&f, run);
    f.runtime.cancel(run).unwrap();
    f.runtime.wait(run).await.unwrap();
}
#[tokio::test]
async fn readonly_unknown_and_flush_time_owner_loss_keep_notice_pending() {
    let mut f = Fixture::new();
    let (run, host) = f.lead().await;
    let n = notice(&f, 1, 42, PrNoticeMode::Manual);
    for reason in [
        LaneDeliveryNack::Readonly,
        LaneDeliveryNack::UnknownThread,
        LaneDeliveryNack::OwnershipLost,
    ] {
        *host.deny.lock().unwrap() = Some(reason);
        assert_eq!(
            f.runtime
                .drive_lane_deliveries("delivery-lead")
                .await
                .unwrap(),
            LaneDeliveryOutcome::Nacked(reason)
        );
        assert_eq!(phase(&f, &n), PrNoticePhase::Ready);
    }
    *host.deny.lock().unwrap() = None;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let runtime = f.runtime.clone();
    let gate = barrier.clone();
    let task = tokio::spawn(BEFORE_QUEUE.scope(gate, async move {
        runtime.drive_lane_deliveries("delivery-lead").await
    }));
    barrier.wait().await;
    *host.deny.lock().unwrap() = Some(LaneDeliveryNack::OwnershipLost);
    barrier.wait().await;
    assert_eq!(
        task.await.unwrap().unwrap(),
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::OwnershipLost)
    );
    assert_eq!(occurrences(&f, run, 1), 0);
    *host.deny.lock().unwrap() = None;
    delivered(&f).await;
    f.phase(run, AgentRunPhase::Waiting).await;
    f.runtime.cancel(run).unwrap();
    f.runtime.wait(run).await.unwrap();
}
#[tokio::test]
async fn unresolved_payload_at_flush_releases_lease_without_accepting_reference() {
    let mut f = Fixture::new();
    let (run, _) = f.lead().await;
    let lane = f.lane(run, "mixed-payload").await;
    f.report(&lane, Some(1), LaneStatus::Ready);
    let n = notice(&f, 1, 42, PrNoticeMode::Manual);
    let saved = n.payload.clone().unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let runtime = f.runtime.clone();
    let gate = barrier.clone();
    let task = tokio::spawn(BEFORE_QUEUE.scope(gate, async move {
        runtime.drive_lane_deliveries("delivery-lead").await
    }));
    barrier.wait().await;
    barrier.wait().await; // Lane receipt commits first.
    barrier.wait().await; // PR is independently claimed, before flush.
    f.sql("UPDATE pr_notices SET payload=NULL");
    barrier.wait().await;
    assert_eq!(
        task.await.unwrap().unwrap(),
        LaneDeliveryOutcome::Nacked(LaneDeliveryNack::PayloadUnavailable)
    );
    assert_eq!(f.receipts(), 1); // Do not hide the PR NACK behind Lane success.
    assert_eq!(occurrences(&f, run, 1), 0);
    assert_eq!(phase(&f, &n), PrNoticePhase::Ready);
    let c = rusqlite::Connection::open(&f.config.db_path).unwrap();
    let nack: String = c
        .query_row(
            "SELECT last_nack FROM lane_delivery_destinations WHERE thread_id='delivery-lead'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<LaneDeliveryNack>(&nack).unwrap(),
        LaneDeliveryNack::PayloadUnavailable
    );
    c.execute("UPDATE pr_notices SET payload=?1", [saved])
        .unwrap();
    delivered(&f).await;
    f.phase(run, AgentRunPhase::Waiting).await;
    f.runtime.cancel(run).unwrap();
    f.runtime.wait(run).await.unwrap();
    f.runtime.cancel(lane.root_run_id).unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
}
#[tokio::test]
async fn stop_is_durable_across_policy_changes_and_requires_explicit_resume() {
    integration::MANUAL_SOURCES
        .scope((), async {
            let mut f = Fixture::new();
            let (run, host) = f.lead().await;
            let n = notice(&f, 1, 42, PrNoticeMode::Manual);
            let mut resumes = f.runtime.subscribe_harness_delivery_resumes();
            let guard = f.storage.handle().pr_wake_guard(key(42)).unwrap();
            f.storage
                .handle()
                .update_pr_wake_guard(
                    key(42),
                    guard.revision,
                    PrGuardSignal::WakeBatch {
                        batch_id: "pre-stop".into(),
                    },
                )
                .unwrap();
            f.runtime.stop(run, crate::StopScope::SelfOnly).unwrap();
            f.runtime.wait(run).await.unwrap();
            let s = f.storage.handle().pr_auto_fix_state(key(42)).unwrap();
            f.storage
                .handle()
                .update_pr_auto_fix_policy(PrPolicyUpdate {
                    key: key(42),
                    expected_policy_revision: s.policy_revision,
                    expected_destination_thread_id: s.destination_thread_id,
                    destination_thread_id: "delivery-lead".into(),
                    link_revision: 1,
                    action: PrPolicyAction::ResetBudget,
                })
                .unwrap();
            assert!(matches!(
                f.runtime
                    .drive_lane_deliveries("delivery-lead")
                    .await
                    .unwrap(),
                LaneDeliveryOutcome::Stopped
                    | LaneDeliveryOutcome::Nacked(LaneDeliveryNack::Stopped)
            ));
            assert_eq!(host.restores.load(Ordering::SeqCst), 0);
            assert_eq!(phase(&f, &n), PrNoticePhase::Ready);
            assert!(
                !resumes.has_changed().unwrap(),
                "Stop and budget reset are not Resume"
            );
            assert!(
                f.runtime
                    .resume_lane_deliveries("foreign-thread", run, None)
                    .is_err()
            );
            assert!(
                !resumes.has_changed().unwrap(),
                "a rejected Resume emits no hint"
            );
            f.runtime
                .resume_chat(run, host.target.authority.clone())
                .unwrap();
            resumes.changed().await.unwrap();
            assert_eq!(*resumes.borrow_and_update(), 1);
            assert!(
                !f.storage
                    .handle()
                    .lane_delivery_destination("delivery-lead".into())
                    .unwrap()
                    .unwrap()
                    .stopped
            );
            assert_eq!(
                f.storage
                    .handle()
                    .pr_wake_guard(key(42))
                    .unwrap()
                    .no_progress_batches,
                0
            );
            f.phase(run, AgentRunPhase::Waiting).await;
            delivered(&f).await;
            f.phase(run, AgentRunPhase::Waiting).await;
            assert_eq!(occurrences(&f, run, 1), 1);
            f.runtime.cancel(run).unwrap();
            f.runtime.wait(run).await.unwrap();
        })
        .await;
}
#[tokio::test]
async fn lane_and_pr_share_a_flight_but_not_credit_or_guard() {
    let mut f = Fixture::new();
    let (run, _) = f.lead().await;
    let lane = f.lane(run, "mixed").await;
    f.report(&lane, Some(1), LaneStatus::Ready);
    let n = notice(&f, 1, 42, PrNoticeMode::Manual);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let runtime = f.runtime.clone();
    let gate = barrier.clone();
    let task = tokio::spawn(BEFORE_QUEUE.scope(gate, async move {
        runtime.drive_lane_deliveries("delivery-lead").await
    }));
    barrier.wait().await;
    assert_eq!(
        f.runtime
            .drive_lane_deliveries("delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Busy
    );
    barrier.wait().await;
    // The gate is entered for each source batch.
    barrier.wait().await;
    barrier.wait().await;
    assert_eq!(
        task.await.unwrap().unwrap(),
        LaneDeliveryOutcome::Delivered { notifications: 2 }
    );
    assert_eq!(phase(&f, &n), PrNoticePhase::Adopted);
    assert_eq!(f.receipts(), 1);
    assert_eq!(occurrences(&f, run, 1), 1);
    assert_eq!(
        f.storage
            .handle()
            .pr_wake_guard(key(42))
            .unwrap()
            .no_progress_batches,
        0
    );
    f.runtime.cancel(run).unwrap();
    f.runtime.wait(run).await.unwrap();
    f.runtime.cancel(lane.root_run_id).unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
}

#[tokio::test]
async fn expired_pr_lease_is_recovered_by_the_common_startup_worker() {
    let mut f = Fixture::new();
    let (run, host) = f.lead().await;
    f.finish(run).await;
    let n = notice(&f, 1, 42, PrNoticeMode::Manual);
    let h = f.storage.handle();
    let owner = host.target.owner();
    let epoch = h
        .bind_lane_delivery_destination(
            "delivery-lead".into(),
            h.lane_delivery_destination("delivery-lead".into())
                .unwrap()
                .map_or(0, |d| d.epoch),
            owner.clone(),
            LaneDestinationAction::Bind,
        )
        .unwrap();
    let request = LaneClaimRequest {
        destination_thread_id: "delivery-lead".into(),
        expected_destination_epoch: epoch,
        owner,
        token: "crashed-pr-claim".into(),
        now_ns: 100,
        lease_until_ns: 200,
        allow_non_waking: false,
    };
    let PrClaimResult::Claimed(old) = h.claim_pr_delivery(request.clone()).unwrap() else {
        panic!("claim")
    };
    assert!(matches!(
        h.claim_pr_delivery(LaneClaimRequest {
            token: "duplicate".into(),
            ..request.clone()
        })
        .unwrap(),
        PrClaimResult::Busy
    ));
    assert!(matches!(
        h.claim_lane_deliveries(LaneClaimRequest {
            token: "lane-contender".into(),
            ..request
        })
        .unwrap(),
        LaneClaimResult::Busy
    ));
    host.now.store(199, Ordering::SeqCst);
    assert_eq!(
        f.runtime
            .drive_lane_deliveries("delivery-lead")
            .await
            .unwrap(),
        LaneDeliveryOutcome::Busy
    );
    host.now.store(200, Ordering::SeqCst);
    assert!(h.ack_pr_delivery(old.clone(), 200).is_err());
    let bus = Arc::new(EventBus::new(1024));
    let restarted = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        f.model.clone(),
    )
    .with_run_store(crate::RunStore::open(&f.config, h.clone()).unwrap());
    let (tx, mut steps) = tokio::sync::mpsc::unbounded_channel();
    *restarted.shared.lanes.delivery.worker_steps.lock().unwrap() = Some(tx);
    let worker = restarted.bind_lane_delivery_host(host.clone()).unwrap();
    let step = steps.recv().await.unwrap();
    step.release.send(()).unwrap();
    step.completed.await.unwrap();
    restarted.wait_admission(run).await.unwrap();
    restarted.wait(run).await.unwrap();
    worker.abort();
    assert_eq!(phase(&f, &n), PrNoticePhase::Adopted);
    assert_eq!(occurrences(&f, run, 1), 1);
    // Once repaired, even the old owner may repeat an idempotent ACK.
    h.ack_pr_delivery(old, 200).unwrap();
    assert_eq!(host.restores.load(Ordering::SeqCst), 1);
    assert_eq!(h.pr_wake_guard(key(42)).unwrap().no_progress_batches, 1);
}
#[tokio::test]
async fn receipt_restart_recovers_unpresented_input_with_or_without_ack_repair() {
    for fail_ack in [false, true] {
        recover_unpresented_input(fail_ack, false).await;
    }
}

#[tokio::test]
async fn pr_receipt_recovers_error_snapshot_with_unpresented_lane_nudge() {
    for fail_ack in [false, true] {
        recover_unpresented_input(fail_ack, true).await;
    }
}

async fn recover_unpresented_input(fail_ack: bool, failed_nudge: bool) {
    let mut f = Fixture::new();
    let (run, host) = f.lead().await;
    f.finish(run).await;
    let n = notice(&f, 1, 42, PrNoticeMode::Manual);
    let before = f.model.requests.lock().unwrap().len();
    let (release, pending) = tokio::sync::oneshot::channel();
    *f.model.fail_after.lock().unwrap() = Some(pending);
    f.model.fail_next.store(true, Ordering::SeqCst);
    if fail_ack {
        f.sql("CREATE TRIGGER fail_recovery_ack BEFORE UPDATE OF state ON lane_notifications WHEN NEW.state='acked' AND NEW.source_kind='pull_request_checks_failed' BEGIN SELECT RAISE(ABORT,'injected'); END;");
    }
    let result = f.runtime.drive_lane_deliveries("delivery-lead").await;
    assert_eq!(result.is_err(), fail_ack);
    let conn = rusqlite::Connection::open(&f.config.db_path).unwrap();
    let state: String = conn
        .query_row(
            "SELECT state FROM lane_notifications WHERE notification_id=?1",
            [&n.notice_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, if fail_ack { "pending" } else { "acked" });
    // The provider rejection must occur after the ACK boundary, not race it.
    release.send(()).unwrap();
    f.runtime.wait(run).await.unwrap();
    assert_eq!(f.model.requests.lock().unwrap().len(), before);
    if fail_ack {
        f.sql("DROP TRIGGER fail_recovery_ack");
    }
    if failed_nudge {
        // A mixed persisted snapshot: the independently unpresented Lane
        // nudge must not veto recovery of an already adopted PR input.
        let presentation = serde_json::json!([{
            "nudge_episode_ids": ["unpresented-nudge"],
            "fingerprint": "unpresented-lane-input",
            "lanes": []
        }]);
        let mut bundle = f
            .runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_bundle(run)
            .unwrap()
            .unwrap();
        assert_eq!(bundle.record.terminal_phase, "Error");
        let mut descriptor: crate::restore::RunRestoreDescriptor =
            serde_json::from_str(&bundle.record.config_json).unwrap();
        assert!(descriptor.harness_notice_pending);
        descriptor.spawn_credits.lane_presentations = serde_json::from_value(presentation).unwrap();
        bundle.record.config_json = serde_json::to_string(&descriptor).unwrap();
        let provenance = bundle.provenance.unwrap();
        f.storage
            .handle()
            .save_snapshot_bundle(&storage::thread_index::SnapshotWrite {
                record: bundle.record,
                expected_snapshot_revision: Some(bundle.snapshot_revision),
                binding: None,
                status: provenance.status,
                map: provenance.map,
            })
            .unwrap();
    }
    assert_eq!(
        f.storage
            .handle()
            .pending_pr_delivery_destinations(String::new())
            .unwrap(),
        vec!["delivery-lead"],
        "an adopted but unpresented input must remain discoverable even after ACK"
    );
    let bus = Arc::new(EventBus::new(1024));
    let restarted = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        f.model.clone(),
    )
    .with_run_store(crate::RunStore::open(&f.config, f.storage.handle()).unwrap());
    *restarted.shared.lanes.delivery.host.lock().unwrap() = Some(host.clone());
    restarted.drive_pending_lane_deliveries().await.unwrap();
    assert_eq!(
        host.restores.load(Ordering::SeqCst),
        2,
        "the pending PR input must restore even with a failed Lane nudge"
    );
    restarted.wait_admission(run).await.unwrap();
    restarted.wait(run).await.unwrap();
    assert_eq!(phase(&f, &n), PrNoticePhase::Adopted);
    assert_eq!(occurrences(&f, run, 1), 1);
    assert_eq!(host.restores.load(Ordering::SeqCst), 2);
    assert!(f.model.requests.lock().unwrap().len() > before);
    assert!(
        f.storage
            .handle()
            .pending_pr_delivery_destinations(String::new())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.storage
            .handle()
            .pr_wake_guard(key(42))
            .unwrap()
            .no_progress_batches,
        1
    );
}

#[tokio::test(start_paused = true)]
async fn occupied_lane_destination_does_not_block_an_independent_pr_destination() {
    let mut f = Fixture::new();
    let (lead, first) = f.lead().await;
    let other = f
        .runtime
        .delegate_chat(
            "occupied-lane-lead",
            Role::Orchestrator,
            "coordinate independently".into(),
            first.target.authority.clone(),
        )
        .unwrap();
    f.phase(other, AgentRunPhase::Waiting).await;
    let lane = f.lane(other, "occupied-source").await;
    let host = Arc::new(super::lease::Hosts {
        targets: HashMap::from([
            ("delivery-lead".into(), first.target.clone()),
            (
                "occupied-lane-lead".into(),
                LaneDeliveryTarget {
                    root_run_id: other,
                    ..first.target.clone()
                },
            ),
        ]),
        now: AtomicI64::new(100),
    });
    *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host);
    f.model.block_next.store(true, Ordering::SeqCst);
    f.runtime
        .send_message(other, "hold this provider request".into())
        .unwrap();
    f.model.blocked.notified().await;
    f.report(&lane, Some(1), LaneStatus::Ready);
    let n = notice(&f, 1, 42, PrNoticeMode::Manual);
    let runtime = f.runtime.clone();
    let scan = tokio::spawn(async move { runtime.drive_pending_lane_deliveries().await });
    f.phase(lead, AgentRunPhase::Waiting).await;
    assert_eq!(phase(&f, &n), PrNoticePhase::Adopted);
    assert_eq!(occurrences(&f, lead, 1), 1);
    assert!(
        f.runtime
            .shared
            .lanes
            .delivery
            .pending
            .lock()
            .unwrap()
            .contains_key(&other),
        "PR must be delivered before the occupied Lane claim expires"
    );
    // Release only this scan; no wall-clock wait or provider completion is needed.
    scan.abort();
    assert!(scan.await.unwrap_err().is_cancelled());
    f.runtime.stop(other, crate::StopScope::SelfOnly).unwrap();
    f.runtime.wait(other).await.unwrap();
    f.runtime.cancel(lead).unwrap();
    f.runtime.wait(lead).await.unwrap();
}
#[tokio::test]
async fn provenance_survives_delegation_fork_restore_and_saved_compaction() {
    let mut f = Fixture::new();
    f.runtime = f.runtime.with_compaction(config::CompactionConfig {
        summarizer: config::SummarizerKind::Structural,
        keep_recent_tokens: 1,
        max_summary_bytes: 1024,
        ..Default::default()
    });
    let (run, host) = f.lead().await;
    let harness = notice(&f, 1, 42, PrNoticeMode::Manual);
    delivered(&f).await;
    f.phase(run, AgentRunPhase::Waiting).await;
    internal(&f, run);
    let bundle = f
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_bundle(run)
        .unwrap()
        .unwrap();
    let messages: Vec<Message> = serde_json::from_str(&bundle.record.messages_json).unwrap();
    let idx = messages
        .iter()
        .position(|m| {
            m.content.iter().any(
                |b| matches!(b,ContentBlock::Text{text} if text.starts_with("[harness-notice")),
            )
        })
        .unwrap();
    let map = &bundle.provenance.as_ref().unwrap().map;
    assert!(
        map.messages
            .iter()
            .find(|m| m.raw_index == idx)
            .unwrap()
            .blocks
            .iter()
            .all(|b| b.excluded_reason
                == Some(storage::thread_index::ExcludedReason::GeneratedBoundary))
    );
    assert!(map.messages.iter().find(|m| m.raw_index == idx).unwrap().blocks.iter().all(|b| matches!(&b.origin, storage::thread_index::BlockOrigin::HarnessNotice { notification_id } if notification_id == &harness.notice_id)));
    let fork = f
        .runtime
        .delegate_chat_seeded(
            "harness-fork",
            Role::Orchestrator,
            "new fork task".into(),
            host.target.authority.clone(),
            Some(crate::ChatForkSeed {
                source_run_id: run.to_string(),
                context_len: crate::restore::conversation_len(&messages) as u64,
            }),
        )
        .unwrap();
    f.phase(fork, AgentRunPhase::Waiting).await;
    let child = f
        .runtime
        .delegate_background_as_child(
            run,
            Role::Worker,
            "inspect the failure",
            RunConfig::default(),
        )
        .unwrap();
    f.runtime.wait(child).await.unwrap();
    {
        let reviews = f.runtime.shared.review_runs.lock().unwrap();
        for id in [fork, child] {
            assert!(
                reviews[&id]
                    .requests()
                    .0
                    .iter()
                    .all(|r| !r.text.contains("[harness-notice"))
            );
        }
    }
    let fork_bundle = f
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_bundle(fork)
        .unwrap()
        .unwrap();
    assert!(
        fork_bundle
            .provenance
            .unwrap()
            .map
            .messages
            .iter()
            .any(|m| m.blocks.iter().any(|b| matches!(
                b.origin,
                storage::thread_index::BlockOrigin::ForkCopy { .. }
            ) && b.excluded_reason.is_some()))
    );
    f.finish(run).await;
    f.runtime
        .compact_saved_chat(run, host.target.authority.clone())
        .await
        .unwrap();
    f.runtime
        .wake_chat(run, host.target.authority.clone())
        .unwrap();
    f.runtime.wait_admission(run).await.unwrap();
    f.phase(run, AgentRunPhase::Waiting).await;
    internal(&f, run);
    let restored = f
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_bundle(run)
        .unwrap()
        .unwrap();
    assert!(restored.provenance.unwrap().map.messages.iter().flat_map(|m| &m.blocks).any(|b| matches!(&b.origin, storage::thread_index::BlockOrigin::HarnessNotice { notification_id } if notification_id == &harness.notice_id)));
    f.runtime.cancel(run).unwrap();
    f.runtime.wait(run).await.unwrap();
    f.runtime.cancel(fork).unwrap();
    f.runtime.wait(fork).await.unwrap();
}
#[tokio::test]
async fn processed_bodies_are_bounded_and_lookalike_human_input_is_still_human() {
    let mut f = Fixture::new();
    let (run, _) = f.lead().await;
    let n = notice(&f, 1, 42, PrNoticeMode::Manual);
    let body = prepare_pr_notice_payload(
        &n,
        &["日本語".repeat(10000)],
        "now",
        Some(&crate::pull_requests::log_source::CiLogBundle::refused(
            &n.head_sha,
            crate::pull_requests::log_source::LogSkipReason::LogEmpty,
        )),
    );
    f.sql("UPDATE pr_notices SET payload=NULL");
    rusqlite::Connection::open(&f.config.db_path)
        .unwrap()
        .execute("UPDATE pr_notices SET payload=?1", [body])
        .unwrap();
    delivered(&f).await;
    f.phase(run, AgentRunPhase::Waiting).await;
    let text = "[harness-notice kind=pr_checks_failed pr=#42 head=human-lookalike]";
    f.runtime.send_message(run, text.into()).unwrap();
    f.phase(run, AgentRunPhase::Waiting).await;
    {
        let reviews = f.runtime.shared.review_runs.lock().unwrap();
        assert!(reviews[&run].requests().0.iter().any(|r| r.text == text));
    }
    let record = f
        .runtime
        .shared
        .run_store
        .get()
        .unwrap()
        .restore_record(run)
        .unwrap()
        .unwrap();
    let messages: Vec<Message> = serde_json::from_str(&record.messages_json).unwrap();
    let delivered = messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::Text { text }
                if text.contains("head=0000000000000000000000000000000000000001]") =>
            {
                Some(text)
            }
            _ => None,
        })
        .unwrap();
    assert!(delivered.len() <= storage::lane_delivery::LANE_DELIVERY_PAGE_BYTES);
    f.runtime.cancel(run).unwrap();
    f.runtime.wait(run).await.unwrap();
}

#[tokio::test]
async fn stable_pr_guard_is_independent_of_lane_progress_and_budget_policy() {
    integration::MANUAL_SOURCES
        .scope((), async {
            let mut f = Fixture::new();
            let (run, _) = f.lead().await;
            f.finish(run).await;
            let first = notice(&f, 1, 42, PrNoticeMode::Manual);
            let second = notice(&f, 2, 42, PrNoticeMode::Manual);
            let h = f.storage.handle();
            for n in 0..20 {
                let g = h.pr_wake_guard(key(42)).unwrap();
                h.update_pr_wake_guard(
                    key(42),
                    g.revision,
                    PrGuardSignal::WakeBatch {
                        batch_id: format!("no-progress-{n}"),
                    },
                )
                .unwrap();
            }
            let guard = h.pr_wake_guard(key(42)).unwrap();
            assert!(guard.suppressed);
            let s = h.pr_auto_fix_state(key(42)).unwrap();
            h.update_pr_auto_fix_policy(PrPolicyUpdate {
                key: key(42),
                expected_policy_revision: s.policy_revision,
                expected_destination_thread_id: s.destination_thread_id,
                destination_thread_id: "delivery-lead".into(),
                link_revision: 1,
                action: PrPolicyAction::ResetBudget,
            })
            .unwrap();
            assert_eq!(h.pr_wake_guard(key(42)).unwrap(), guard);
            assert_eq!(
                f.runtime
                    .drive_lane_deliveries("delivery-lead")
                    .await
                    .unwrap(),
                LaneDeliveryOutcome::Empty
            );
            assert_eq!(phase(&f, &first), PrNoticePhase::Ready);
            assert_eq!(phase(&f, &second), PrNoticePhase::Ready);
            f.runtime
                .explicitly_resume_lane_delivery(run, None)
                .unwrap();
            delivered(&f).await;
            f.runtime.wait(run).await.unwrap();
            delivered(&f).await;
            f.runtime.wait(run).await.unwrap();
            assert_eq!(occurrences(&f, run, 1), 1);
            assert_eq!(occurrences(&f, run, 2), 1);
            assert_eq!(h.pr_wake_guard(key(42)).unwrap().no_progress_batches, 2);
        })
        .await;
}

fn plain_notice() -> PrNotice {
    PrNotice {
        notice_id: "prn-view".into(),
        reservation_revision: 0,
        key: key(42),
        head_sha: "a".repeat(40),
        mode: PrNoticeMode::Manual,
        destination_thread_id: Some("delivery-lead".into()),
        link_revision: 1,
        policy_revision: 0,
        budget_epoch: 0,
        success_seq: 1,
        phase: PrNoticePhase::Ready,
        payload: None,
    }
}

// Given: a payload prepared for the agent, with a log whose text contains the characters the
// wrapper escapes / When: it is read back for display / Then: the facts and the excerpt come
// out as they were written, and nothing of the wrapper does
#[test]
fn a_prepared_payload_reads_back_as_facts_and_excerpt_without_the_wrapper() {
    use crate::pull_requests::log_source::{CiLogBundle, CiLogRun};
    let notice = plain_notice();
    let bundle = CiLogBundle {
        head_sha: notice.head_sha.clone(),
        runs: vec![CiLogRun {
            run_id: 7,
            checks: vec!["unit".into()],
            excerpt: "error: expected </ci-log-data> but found `<T>`\nsecond line".into(),
            lines_omitted: 3,
            raw_truncated: false,
            line_cut: false,
            lossy_utf8: false,
            redactions: 0,
        }],
        skipped: Vec::new(),
        truncated: true,
    };
    let payload = prepare_pr_notice_payload(
        &notice,
        &["unit: failure".into(), "lint: timed out".into()],
        "2026-01-01T00:00:00Z",
        Some(&bundle),
    );

    let view = view_pr_notice_payload(&payload);

    assert_eq!(view.checks, ["unit: failure", "lint: timed out"]);
    assert_eq!(view.observed_at.as_deref(), Some("2026-01-01T00:00:00Z"));
    assert_eq!(view.logs.len(), 1);
    assert_eq!(view.logs[0].checks, ["unit"]);
    assert_eq!(
        view.logs[0].excerpt,
        "error: expected </ci-log-data> but found `<T>`\nsecond line"
    );
    assert!(view.logs_truncated);
}

// Given: a payload without logs, and text that has none of the expected shape / When: read
// for display / Then: what can be read is given, and nothing is an error
#[test]
fn a_payload_without_logs_or_of_another_shape_reads_as_less_not_as_text() {
    let notice = plain_notice();
    let payload = prepare_pr_notice_payload(&notice, &["unit: failure".into()], "now", None);
    let view = view_pr_notice_payload(&payload);
    assert_eq!(view.checks, ["unit: failure"]);
    assert!(view.logs.is_empty());

    assert_eq!(view_pr_notice_payload("free text"), PrNoticeView::default());
    assert_eq!(
        view_pr_notice_payload("<ci-log-data>\nnot json\n</ci-log-data>"),
        PrNoticeView::default()
    );
}
