use super::super::tests::*;
use super::super::{KeyState, WallClock};
use super::*;
use crate::pull_requests::{CiLogBundle, FakePullRequestLogSource, PullRequestLogSource};
use async_trait::async_trait;
use storage::lane_delivery::{LaneDeliveryOwner, LaneDestinationAction};
use storage::pull_requests::PullRequestState;
use tokio::sync::Notify;

fn target(p: &Process) -> Target {
    let l = p.state("t").active_link.unwrap();
    Target {
        thread_id: "t".into(),
        key: l.key,
        link_id: l.link_id,
        link_revision: l.link_revision,
        permit: p.permit("t"),
    }
}
fn snapshot(p: &Process, head: char, pending: bool) -> u64 {
    let seq = p.storage_handle.begin_pull_request_fetch(&key(1)).unwrap();
    let mut f = facts(PullRequestState::Open, failing());
    f.head_sha = head.to_string().repeat(40);
    if pending {
        f.checks.extend(running());
    }
    for c in &mut f.checks {
        c.head_sha = f.head_sha.clone();
    }
    p.storage_handle
        .commit_pull_request_fetch(&key(1), seq, f, seq as i64)
        .unwrap();
    let mut k = KeyState::new(Instant::now(), false, seq);
    k.next_due = None;
    p.service.inner().lock().keys.insert(key(1), k);
    seq
}
fn attribute(p: &Process, t: &Target, seq: u64, head: char) {
    let mut s = p.service.inner().lock();
    s.attribution.succeeded("t", t.permit.lease.generation);
    let h = head.to_string().repeat(40);
    s.attribution
        .adopt("t", t.permit.lease.generation, 1, seq, &h, Some(&h));
}
fn notices(p: &Process) -> Vec<PrNotice> {
    p.storage_handle.pr_notices(key(1), String::new()).unwrap()
}
async fn enable(p: &Process, t: &Target) {
    let s = p.storage_handle.pr_auto_fix_state(key(1)).unwrap();
    p.service
        .set_auto_fix("t", 1, s.policy_revision, true, &t.permit)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn baseline_pending_completion_and_toggle_work_without_gui_notifications() {
    let w = World::new();
    let logs = Arc::new(FakePullRequestLogSource::new());
    let p = w.process_with_logs(enabled(), logs.clone());
    p.link_directly("t", 1);
    let t = target(&p);
    let seq = snapshot(&p, 'a', true);
    attribute(&p, &t, seq, 'a');
    enable(&p, &t).await;
    settle().await;
    assert!(notices(&p).is_empty());
    assert!(logs.requests().is_empty());
    let seq = snapshot(&p, 'a', false);
    p.service.inner().lock().attribution.adopt(
        "t",
        t.permit.lease.generation,
        1,
        seq,
        &"a".repeat(40),
        None,
    );
    p.service.notify_changed();
    settle().await;
    assert_eq!(logs.requests().len(), 1);
    let n = notices(&p);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0].phase, PrNoticePhase::Ready);
    assert!(
        n[0].payload
            .as_ref()
            .unwrap()
            .starts_with("[harness-notice kind=pr_checks_failed")
    );
    p.service.notify_changed();
    settle().await;
    assert_eq!(logs.requests().len(), 1);
}
#[tokio::test(start_paused = true)]
async fn off_unknown_and_non_owner_neither_reserve_nor_fetch_logs() {
    for (on, agent) in [(false, true), (true, false)] {
        let w = World::new();
        let logs = Arc::new(FakePullRequestLogSource::new());
        let p = w.process_with_logs(enabled(), logs.clone());
        p.link_directly("t", 1);
        let t = target(&p);
        let seq = snapshot(&p, 'a', false);
        if agent {
            attribute(&p, &t, seq, 'a');
        }
        if on {
            enable(&p, &t).await;
        }
        p.service.notify_changed();
        settle().await;
        assert!(notices(&p).is_empty());
        assert!(logs.requests().is_empty());
        let other_logs = Arc::new(FakePullRequestLogSource::new());
        let viewer = w.process_with_logs(enabled(), other_logs.clone());
        settle().await;
        assert!(viewer.source.calls().is_empty());
        assert!(other_logs.requests().is_empty());
    }
}
#[tokio::test(start_paused = true)]
async fn manual_request_drives_preparation_and_manual_auto_races_share_one_sha() {
    for manual_first in [false, true] {
        let w = World::new();
        let logs = Arc::new(FakePullRequestLogSource::new());
        let p = w.process_with_logs(enabled(), logs.clone());
        p.link_directly("t", 1);
        let t = target(&p);
        let seq = snapshot(&p, 'a', manual_first);
        attribute(&p, &t, seq, 'a');
        if manual_first {
            p.service
                .request_manual_notice("t", 1, &"a".repeat(40), &t.permit)
                .await
                .unwrap();
            enable(&p, &t).await;
        } else {
            enable(&p, &t).await;
            settle().await;
            assert!(
                p.service
                    .request_manual_notice("t", 1, &"a".repeat(40), &t.permit)
                    .await
                    .is_err()
            );
        }
        settle().await;
        assert_eq!(notices(&p).len(), 1);
        assert_eq!(logs.requests().len(), 1);
        let state = p.storage_handle.pr_auto_fix_state(key(1)).unwrap();
        assert_eq!(state.used, 0);
        assert_eq!(state.reserved, u32::from(!manual_first));
    }
}
#[derive(Default)]
struct GateLogs {
    arrived: Notify,
    release: Notify,
    fake: FakePullRequestLogSource,
}
#[async_trait]
impl PullRequestLogSource for GateLogs {
    async fn fetch_failed_logs(&self, request: &LogRequest) -> CiLogBundle {
        self.arrived.notify_one();
        self.release.notified().await;
        self.fake.fetch_failed_logs(request).await
    }
}
#[tokio::test(start_paused = true)]
async fn after_logs_revalidate_off_unlink_head_closed_global_and_stop() {
    for change in ["off", "unlink", "head", "closed", "global", "stop"] {
        let w = World::new();
        let logs = Arc::new(GateLogs::default());
        let p = w.process_with_logs(enabled(), logs.clone());
        p.link_directly("t", 1);
        let t = target(&p);
        let seq = snapshot(&p, 'a', false);
        attribute(&p, &t, seq, 'a');
        enable(&p, &t).await;
        logs.arrived.notified().await;
        match change {
            "off" => {
                let s = p.storage_handle.pr_auto_fix_state(key(1)).unwrap();
                p.service
                    .set_auto_fix("t", 1, s.policy_revision, false, &t.permit)
                    .await
                    .unwrap();
            }
            "unlink" => {
                p.service.unlink("t", 1, &t.permit).await.unwrap();
            }
            "head" => {
                snapshot(&p, 'b', false);
            }
            "closed" => {
                let seq = p.storage_handle.begin_pull_request_fetch(&key(1)).unwrap();
                p.storage_handle
                    .commit_pull_request_fetch(
                        &key(1),
                        seq,
                        facts(PullRequestState::Closed, failing()),
                        1,
                    )
                    .unwrap();
            }
            "global" => {
                p.service
                    .update_settings(config::PullRequestsConfig {
                        auto_fix_enabled: false,
                        ..enabled()
                    })
                    .unwrap();
            }
            "stop" => {
                p.storage_handle
                    .bind_lane_delivery_destination(
                        "t".into(),
                        0,
                        LaneDeliveryOwner {
                            root_run_id: "root".into(),
                            generation: 1,
                            ownership_generation: t.permit.lease.generation,
                        },
                        LaneDestinationAction::Stop,
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        logs.release.notify_one();
        settle().await;
        let n = notices(&p);
        assert_eq!(n.len(), 1);
        assert_eq!(
            n[0].phase,
            if change == "stop" {
                PrNoticePhase::Ready
            } else {
                PrNoticePhase::Cancelled
            },
            "{change}"
        );
    }
}
#[tokio::test(start_paused = true)]
async fn cooldown_599_600_and_reset_evaluate_only_the_latest_head() {
    let w = World::new();
    let logs = Arc::new(FakePullRequestLogSource::new());
    let p = w.process_with_logs(enabled(), logs.clone());
    p.link_directly("t", 1);
    let t = target(&p);
    let seq = snapshot(&p, 'a', false);
    attribute(&p, &t, seq, 'a');
    enable(&p, &t).await;
    // Fixture: policy observed after A/C's third atomic adoption (covered by A's IO
    // contracts). This test checks D reads the durable result and preserves cooldown.
    let sql = rusqlite::Connection::open(w.dir.path().join("evorch.db")).unwrap();
    sql.execute(
        "UPDATE pr_auto_fix_state SET used=3,enabled=0,cooldown_until_ns=?1",
        [w.clock.now_ns() + 600_000_000_000],
    )
    .unwrap();
    let s = p.storage_handle.pr_auto_fix_state(key(1)).unwrap();
    assert!(
        p.service
            .set_auto_fix("t", 1, s.policy_revision, true, &t.permit)
            .await
            .is_err()
    );
    p.service
        .reset_auto_fix_budget("t", 1, s.policy_revision, &t.permit)
        .await
        .unwrap();
    settle().await;
    w.clock.advance(599);
    tokio::time::advance(Duration::from_secs(599)).await;
    settle().await;
    assert!(logs.requests().is_empty());
    let seq = snapshot(&p, 'b', false);
    attribute(&p, &t, seq, 'b');
    p.service.notify_changed();
    settle().await;
    assert!(logs.requests().is_empty());
    w.clock.advance(1);
    tokio::time::advance(Duration::from_secs(1)).await;
    settle().await;
    assert_eq!(logs.requests().len(), 1);
    assert_eq!(logs.requests()[0].head_sha, "b".repeat(40));
}
#[tokio::test(start_paused = true)]
async fn restarting_recovers_reserved_work_but_not_an_unreserved_head() {
    let w = World::new();
    let logs = Arc::new(FakePullRequestLogSource::new());
    let p = w.process_with_logs(enabled(), logs.clone());
    p.link_directly("t", 1);
    let t = target(&p);
    let seq = snapshot(&p, 'a', false);
    attribute(&p, &t, seq, 'a');
    enable(&p, &t).await;
    let notice = p
        .service
        .inner()
        .reserve_notice(&t, PrNoticeMode::Auto, None)
        .unwrap();
    p.service.inner().lock().attribution = Default::default();
    p.service.notify_changed();
    settle().await;
    assert_eq!(
        p.storage_handle
            .pr_notice(notice.notice_id)
            .unwrap()
            .unwrap()
            .phase,
        PrNoticePhase::Ready
    );
    snapshot(&p, 'b', false);
    p.service.inner().lock().attribution = Default::default();
    p.service.notify_changed();
    settle().await;
    assert_eq!(logs.requests().len(), 1);
    assert_eq!(notices(&p).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn real_link_baseline_uses_observed_publishing_and_only_the_fresh_local_resolution() {
    use crate::pull_requests::{CheckoutMiss, CheckoutResolution, checkout::Checkout};
    use event_bus::{Event, ToolEvent};
    use serde_json::json;
    for (local, expected) in [(Some('a'), true), (Some('b'), false), (None, false)] {
        let w = World::new();
        let logs = Arc::new(FakePullRequestLogSource::new());
        let p = w.process_with_logs(enabled(), logs.clone());
        let permit = p.permit("t");
        p.environment.bind("run", "t");
        p.environment
            .set_directories(vec![w.dir.path().to_path_buf()]);
        p.environment.set_resolution(match local {
            Some(h) => CheckoutResolution::Found(Checkout {
                path: w.dir.path().to_path_buf(),
                head_sha: h.to_string().repeat(40),
                remote: "origin".into(),
            }),
            None => CheckoutResolution::Missing(CheckoutMiss::ReadFailed),
        });
        p.bus.emit(Event::new(ToolEvent::ToolStarted {
            tool_name: "shell".into(),
            call_id: "push".into(),
            input: Some(json!({"command":"git push"})),
            run_id: Some("run".into()),
            parent_call_id: None,
        }));
        p.bus.emit(Event::new(ToolEvent::ToolCompleted {
            tool_name: "shell".into(),
            call_id: "push".into(),
            is_error: true,
            output: None,
            detail: Some(json!({"exit_code":0})),
            run_id: Some("run".into()),
            parent_call_id: None,
        }));
        settle().await;
        p.source.push_ok(
            &key(1),
            fetched(1, facts(PullRequestState::Open, failing())),
        );
        p.source.repeat_last();
        p.service
            .link("t", "turtton/evorch#1", 0, &permit)
            .await
            .unwrap();
        // Even a persisted match cannot substitute for this fetch's unresolved local HEAD.
        p.storage_handle
            .record_pull_request_local_head("t", 1, &"a".repeat(40), 1)
            .unwrap();
        enable(&p, &target(&p)).await;
        settle().await;
        assert_eq!(
            logs.requests().len(),
            usize::from(expected),
            "local={local:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn an_owner_handoff_recovers_the_reservation_without_trusting_the_old_log_result() {
    let w = World::new();
    let old_logs = Arc::new(GateLogs::default());
    let new_logs = Arc::new(FakePullRequestLogSource::new());
    let first = w.process_with_logs(enabled(), old_logs.clone());
    first.link_directly("t", 1);
    let t = target(&first);
    let seq = snapshot(&first, 'a', false);
    attribute(&first, &t, seq, 'a');
    enable(&first, &t).await;
    old_logs.arrived.notified().await;
    let second = w.process_with_logs(enabled(), new_logs.clone());
    let mut parked = KeyState::new(Instant::now(), false, seq);
    parked.next_due = None;
    second.service.inner().lock().keys.insert(key(1), parked);
    first.host.handoff(&t.permit, &second.host).unwrap();
    old_logs.release.notify_one();
    settle().await;
    assert_eq!(notices(&first).len(), 1);
    assert_eq!(notices(&first)[0].phase, PrNoticePhase::Ready);
    assert_eq!(new_logs.requests().len(), 1);
    assert_eq!(
        first
            .storage_handle
            .pr_auto_fix_state(key(1))
            .unwrap()
            .reserved,
        1
    );
}
#[tokio::test(start_paused = true)]
async fn destination_changes_require_confirmation_and_revision_cas() {
    let w = World::new();
    let p = w.process(enabled());
    p.link_directly("t", 1);
    let t = target(&p);
    snapshot(&p, 'a', false);
    enable(&p, &t).await;
    p.link_directly("u", 1);
    let u = p.permit("u");
    let s = p.storage_handle.pr_auto_fix_state(key(1)).unwrap();
    assert!(
        p.service
            .set_auto_fix("u", 1, s.policy_revision, true, &u)
            .await
            .is_err()
    );
    assert!(
        p.service
            .change_auto_fix_destination("u", 1, s.policy_revision, None, &u)
            .await
            .is_err()
    );
    let changed = p
        .service
        .change_auto_fix_destination("u", 1, s.policy_revision, Some("t".into()), &u)
        .await
        .unwrap();
    assert_eq!(changed.destination_thread_id.as_deref(), Some("u"));
    assert!(
        p.service
            .set_auto_fix("u", 1, s.policy_revision, false, &u)
            .await
            .is_err()
    );
    assert!(
        p.service
            .request_manual_notice("u", 1, &"b".repeat(40), &u)
            .await
            .is_err()
    );
}
#[tokio::test(start_paused = true)]
async fn manual_does_not_resume_stop_but_is_independent_of_the_global_auto_switch() {
    let w = World::new();
    let logs = Arc::new(FakePullRequestLogSource::new());
    let p = w.process_with_logs(
        config::PullRequestsConfig {
            auto_fix_enabled: false,
            ..enabled()
        },
        logs.clone(),
    );
    p.link_directly("t", 1);
    let t = target(&p);
    snapshot(&p, 'a', true);
    let owner = LaneDeliveryOwner {
        root_run_id: "root".into(),
        generation: 1,
        ownership_generation: t.permit.lease.generation,
    };
    let epoch = p
        .storage_handle
        .bind_lane_delivery_destination("t".into(), 0, owner.clone(), LaneDestinationAction::Stop)
        .unwrap();
    assert!(
        p.service
            .request_manual_notice("t", 1, &"a".repeat(40), &t.permit)
            .await
            .is_err()
    );
    assert!(notices(&p).is_empty());
    p.storage_handle
        .bind_lane_delivery_destination(
            "t".into(),
            epoch,
            owner,
            LaneDestinationAction::ExplicitUserResume,
        )
        .unwrap();
    p.service
        .request_manual_notice("t", 1, &"a".repeat(40), &t.permit)
        .await
        .unwrap();
    settle().await;
    assert_eq!(notices(&p)[0].phase, PrNoticePhase::Ready);
    assert_eq!(logs.requests().len(), 1);
}
