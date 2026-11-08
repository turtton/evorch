use super::*;
use crate::lane_management::*;
use crate::runtime::questions::routing::LaneAnswerRequest;
use event_bus::lane::QuestionResponder;

fn request(
    lane: &crate::lanes::LaneOpened,
    id: &str,
    revision: u64,
    action: LaneManagementAction,
) -> LaneManagementRequest {
    LaneManagementRequest {
        operation_id: id.into(),
        lane_id: lane.lane_id.clone(),
        expected_management_revision: revision,
        action,
    }
}
fn user() -> LaneManagementContext {
    LaneManagementContext::User(LaneHumanContext)
}
fn detach(f: &Fixture, lane: &crate::lanes::LaneOpened, revision: u64) -> LaneManagementOutcome {
    f.runtime
        .detach_lane(
            user(),
            request(
                lane,
                &format!("detach-{revision}"),
                revision,
                LaneManagementAction::Detach,
            ),
        )
        .unwrap()
}
fn policy(
    f: &Fixture,
    lane: &crate::lanes::LaneOpened,
    revision: u64,
    policy: ManagedLanePolicy,
) -> LaneManagementOutcome {
    f.runtime
        .set_lane_policy(
            user(),
            request(
                lane,
                &format!("policy-{revision}"),
                revision,
                LaneManagementAction::SetPolicy { policy },
            ),
        )
        .unwrap()
}
fn attach_request(
    lane: &crate::lanes::LaneOpened,
    id: &str,
    revision: u64,
    lead: RunId,
    thread: &str,
) -> LaneManagementRequest {
    request(
        lane,
        id,
        revision,
        LaneManagementAction::Reattach {
            lead_thread_id: thread.into(),
            expected_lead_root_run_id: lead.to_string(),
        },
    )
}
fn project(f: &Fixture) {
    f.sql("UPDATE threads SET project_id='known-project'");
}
async fn shutdown(f: &Fixture, runs: &[RunId]) {
    for run in runs {
        f.runtime.cancel(*run).unwrap();
        f.runtime.wait(*run).await.unwrap();
    }
}

#[tokio::test]
async fn policy_roundtrip_preserves_execution_progress_and_routes_existing_questions_only_to_user()
{
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host.clone());
    let lane = f.lane(lead, "policy").await;
    let q = f
        .runtime
        .request_user_question(lane.root_run_id, "scope?".into(), vec![], true)
        .unwrap();
    assert!(matches!(q.responder, Some(QuestionResponder::Lead { .. })));
    let before = f.storage.handle().lane(&lane.lane_id).unwrap().unwrap();
    let guard = f
        .storage
        .handle()
        .lane_wake_guard(lane.lane_id.clone())
        .unwrap();
    let outcome = policy(&f, &lane, 1, ManagedLanePolicy::Summary);
    assert_eq!(outcome.operation.lane.management_revision, 2);
    assert!(!outcome.refresh_pending);
    let rerouted = f.runtime.user_question(&q.id).unwrap().unwrap();
    assert_eq!(rerouted.run_id, q.run_id);
    assert_eq!(rerouted.responder, Some(QuestionResponder::User));
    assert_eq!(rerouted.routing_revision, q.routing_revision + 1);
    assert!(
        matches!(f.runtime.wait_or_blocked_on_user(lane.root_run_id).await.unwrap(), crate::RunWait::BlockedOnUser(qs) if qs[0].id == q.id)
    );
    policy(&f, &lane, 2, ManagedLanePolicy::Attached);
    assert_eq!(
        f.runtime.user_question(&q.id).unwrap().unwrap().responder,
        Some(QuestionResponder::User)
    );
    let next = f
        .runtime
        .request_user_question(lane.root_run_id, "new?".into(), vec![], false)
        .unwrap();
    assert!(matches!(
        next.responder,
        Some(QuestionResponder::Lead { .. })
    ));
    let after = f.storage.handle().lane(&lane.lane_id).unwrap().unwrap();
    assert_eq!(
        (
            after.root_run_id,
            after.generation,
            after.last_activity_at_ns,
            after.latest_report_seq
        ),
        (
            before.root_run_id,
            before.generation,
            before.last_activity_at_ns,
            before.latest_report_seq
        )
    );
    assert_eq!(
        f.storage
            .handle()
            .lane_wake_guard(lane.lane_id.clone())
            .unwrap(),
        guard
    );
    assert_eq!(host.restores.load(Ordering::SeqCst), 0);
    shutdown(&f, &[lead, lane.root_run_id]).await;
}

#[tokio::test]
async fn rejects_lane_child_wrong_lead_and_stale_root_without_mutating() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "auth").await;
    let other = f
        .runtime
        .delegate_chat(
            "other",
            Role::Orchestrator,
            "other".into(),
            RunConfig {
                conversation: true,
                keep_alive: true,
                interactive: true,
                ..Default::default()
            },
        )
        .unwrap();
    f.phase(other, AgentRunPhase::Waiting).await;
    let worker = f
        .runtime
        .spawn_reserved_child(
            lead,
            f.runtime.reserve_run_id(),
            Role::Worker,
            "child",
            Default::default(),
        )
        .unwrap();
    f.runtime.wait(worker).await.unwrap();
    for caller in [lane.root_run_id, worker, other] {
        assert_eq!(
            f.runtime
                .detach_lane(
                    LaneManagementContext::Run(caller),
                    request(
                        &lane,
                        &format!("deny-{caller}"),
                        1,
                        LaneManagementAction::Detach
                    )
                )
                .unwrap_err()
                .code,
            "not_current_lead"
        );
    }
    f.sql(&format!(
        "UPDATE threads SET root_run_id='{}' WHERE thread_id='delivery-lead'",
        other
    ));
    assert_eq!(
        f.runtime
            .detach_lane(
                LaneManagementContext::Run(lead),
                request(&lane, "stale-root", 1, LaneManagementAction::Detach)
            )
            .unwrap_err()
            .code,
        "not_current_lead"
    );
    assert_eq!(
        f.storage
            .handle()
            .lane(&lane.lane_id)
            .unwrap()
            .unwrap()
            .management_revision,
        1
    );
    shutdown(&f, &[lead, lane.root_run_id, other]).await;
}

#[tokio::test]
async fn stopped_management_and_replay_do_not_restart_or_project_old_snapshot() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "stop").await;
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::SelfOnly)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    let stopped = f
        .storage
        .handle()
        .lane_delivery_destination(lane.thread_id.clone())
        .unwrap();
    policy(&f, &lane, 1, ManagedLanePolicy::Summary);
    detach(&f, &lane, 2);
    assert_eq!(
        f.runtime.inspect_agent(lane.root_run_id).unwrap().phase,
        AgentRunPhase::Stopped
    );
    assert_eq!(
        f.storage
            .handle()
            .lane_delivery_destination(lane.thread_id.clone())
            .unwrap(),
        stopped
    );
    let replay = policy(&f, &lane, 1, ManagedLanePolicy::Summary);
    assert!(replay.operation.replayed);
    assert_eq!(replay.operation.lane.management_revision, 2);
    assert_eq!(replay.current_lane.unwrap().management_revision, 3);
    assert_eq!(host.restores.load(Ordering::SeqCst), 0);
    // A fresh runtime reconstructs management from SQLite, not in-memory lane scope.
    let bus = Arc::new(EventBus::new(128));
    let recovered = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        f.model.clone(),
    )
    .with_run_store(crate::RunStore::open(&f.config, f.storage.handle()).unwrap());
    let replay = recovered
        .detach_lane(
            user(),
            request(&lane, "detach-2", 2, LaneManagementAction::Detach),
        )
        .unwrap();
    assert!(replay.operation.replayed);
    assert_eq!(
        replay.current_lane.unwrap().policy,
        event_bus::LanePolicy::Detached
    );
    shutdown(&f, &[lead]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_detach_and_double_reattach_have_one_winner_and_default_summary() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "cas").await;
    project(&f);
    for (revision, attach) in [(1, false), (2, true)] {
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let mut tasks = vec![];
        for n in 0..2 {
            let runtime = f.runtime.clone();
            let barrier = barrier.clone();
            let r = if attach {
                attach_request(
                    &lane,
                    &format!("attach-{n}"),
                    revision,
                    lead,
                    "delivery-lead",
                )
            } else {
                request(
                    &lane,
                    &format!("detach-race-{n}"),
                    revision,
                    LaneManagementAction::Detach,
                )
            };
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                if attach {
                    runtime.reattach_lane(
                        LaneHumanContext,
                        r,
                        LaneAttachGuard {
                            pinned: false,
                            archived: false,
                        },
                    )
                } else {
                    runtime.detach_lane(user(), r)
                }
            }));
        }
        barrier.wait().await;
        let mut wins = 0;
        for t in tasks {
            match t.await.unwrap() {
                Ok(_) => wins += 1,
                Err(e) => assert_eq!(e.code, "stale_management_revision"),
            }
        }
        assert_eq!(wins, 1);
    }
    let current = f.storage.handle().lane(&lane.lane_id).unwrap().unwrap();
    assert_eq!(current.policy, event_bus::LanePolicy::Summary);
    assert_eq!(current.root_run_id, Some(lane.root_run_id.to_string()));
    shutdown(&f, &[lead, lane.root_run_id]).await;
}

#[tokio::test]
async fn claim_detach_accept_rejects_old_report_but_preserves_accepted_prefix_before_ack() {
    for received_first in [false, true] {
        let mut f = Fixture::new();
        let (lead, host) = f.lead().await;
        let lane = f.lane(lead, "claim").await;
        *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host);
        f.report(&lane, Some(1), LaneStatus::Ready);
        let runtime = f.runtime.clone();
        let r = request(&lane, "claim-detach", 1, LaneManagementAction::Detach);
        let outcome = if received_first {
            let hook: Box<dyn FnOnce()> = Box::new(move || {
                runtime.detach_lane(user(), r).unwrap();
            });
            AFTER_RECEIPT
                .scope(
                    std::cell::RefCell::new(Some(hook)),
                    f.runtime.drive_lane_deliveries("delivery-lead"),
                )
                .await
                .unwrap()
        } else {
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let gate = barrier.clone();
            let task = tokio::spawn(BEFORE_QUEUE.scope(gate, async move {
                runtime.drive_lane_deliveries("delivery-lead").await
            }));
            barrier.wait().await;
            f.runtime.detach_lane(user(), r).unwrap();
            barrier.wait().await;
            task.await.unwrap().unwrap()
        };
        assert_eq!(f.receipts(), usize::from(received_first));
        if !received_first {
            assert!(matches!(outcome, LaneDeliveryOutcome::Nacked(_)));
        }
        let receipts = f.receipts();
        f.runtime
            .drive_lane_deliveries("delivery-lead")
            .await
            .unwrap();
        assert_eq!(f.receipts(), receipts);
        shutdown(&f, &[lead, lane.root_run_id]).await;
    }
}

#[tokio::test]
async fn answer_detach_orders_reject_late_lead_but_deliver_previously_accepted_answer() {
    for answer_first in [false, true] {
        let mut f = Fixture::new();
        let (lead, host) = f.lead().await;
        let lane = f.lane(lead, "answer").await;
        *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host.clone());
        let q = f
            .runtime
            .request_user_question(lane.root_run_id, "decision".into(), vec![], false)
            .unwrap();
        let r = LaneAnswerRequest {
            question_id: q.id.clone(),
            management_revision: 1,
            routing_revision: q.routing_revision,
            operation_id: "answer".into(),
            answer: Some("accepted scope".into()),
        };
        if answer_first {
            f.runtime.resolve_lane_question(lead, r).unwrap();
            detach(&f, &lane, 1);
        } else {
            detach(&f, &lane, 1);
            assert!(f.runtime.resolve_lane_question(lead, r).is_err());
        }
        if answer_first {
            let hosts = lease::Hosts {
                targets: HashMap::from([(
                    lane.thread_id.clone(),
                    LaneDeliveryTarget {
                        root_run_id: lane.root_run_id,
                        generation: 1,
                        authority: host.target.authority.clone(),
                    },
                )]),
                now: AtomicI64::new(100),
            };
            *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(Arc::new(hosts));
            assert!(matches!(
                f.runtime
                    .drive_lane_deliveries(&lane.thread_id)
                    .await
                    .unwrap(),
                LaneDeliveryOutcome::Delivered { .. }
            ));
            f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
            assert_eq!(
                f.runtime
                    .user_question(&q.id)
                    .unwrap()
                    .unwrap()
                    .answer
                    .as_deref(),
                Some("accepted scope")
            );
            assert_eq!(f.receipts(), 1);
        } else {
            assert_eq!(
                f.runtime.user_question(&q.id).unwrap().unwrap().responder,
                Some(QuestionResponder::User)
            );
        }
        shutdown(&f, &[lead, lane.root_run_id]).await;
    }
}

fn report() -> event_bus::LaneReportInput {
    event_bus::LaneReportInput {
        status: LaneStatus::Ready,
        summary: "ready".into(),
        artifact_refs: vec![],
        artifact_revision: None,
        failure_episode: None,
        needs_human: false,
        question_refs: vec![],
        reason_code: None,
        producer: event_bus::LaneReportProducer::LaneAgent,
    }
}
#[tokio::test]
async fn report_and_detach_commit_orders_cannot_save_under_the_new_management() {
    for report_first in [false, true] {
        let mut f = Fixture::new();
        let (lead, _) = f.lead().await;
        let lane = f.lane(lead, "report-cas").await;
        if report_first {
            f.runtime
                .append_lane_agent_report(lane.root_run_id, None, report())
                .unwrap();
            detach(&f, &lane, 1);
        } else {
            let runtime = f.runtime.clone();
            let r = request(&lane, "report-detach", 1, LaneManagementAction::Detach);
            crate::lane_reports::AFTER_LANE_READ.with(|h| {
                *h.borrow_mut() = Some(Box::new(move || {
                    runtime.detach_lane(user(), r).unwrap();
                }))
            });
            assert!(
                f.runtime
                    .append_lane_agent_report(lane.root_run_id, None, report())
                    .is_err()
            );
        }
        let reports = f
            .storage
            .handle()
            .lane_reports(&lane.lane_id, 0, 20)
            .unwrap();
        assert_eq!(reports.len(), usize::from(report_first));
        assert!(reports.iter().all(|r| r.management_revision == 1));
        assert!(
            f.runtime
                .append_lane_agent_report(lane.root_run_id, None, report())
                .is_err()
        );
        assert!(
            f.runtime
                .append_lane_agent_report(lead, None, report())
                .is_err()
        );
        shutdown(&f, &[lead, lane.root_run_id]).await;
    }
}
#[tokio::test]
async fn new_lead_replaces_old_authority_and_host_guard_rejects_pin_archive_and_wrong_target() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "new-lead").await;
    let next = f
        .runtime
        .delegate_chat(
            "next-lead",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                conversation: true,
                keep_alive: true,
                interactive: true,
                ..Default::default()
            },
        )
        .unwrap();
    f.phase(next, AgentRunPhase::Waiting).await;
    project(&f);
    detach(&f, &lane, 1);
    for (id, guard, code) in [
        (
            "pin",
            LaneAttachGuard {
                pinned: true,
                archived: false,
            },
            "lane_pinned",
        ),
        (
            "archive",
            LaneAttachGuard {
                pinned: false,
                archived: true,
            },
            "lane_archived",
        ),
    ] {
        assert_eq!(
            f.runtime
                .reattach_lane(
                    LaneHumanContext,
                    attach_request(&lane, id, 2, next, "next-lead"),
                    guard
                )
                .unwrap_err()
                .code,
            code
        );
    }
    assert_eq!(
        f.runtime
            .reattach_lane(
                LaneHumanContext,
                attach_request(&lane, "wrong", 2, lane.root_run_id, &lane.thread_id),
                LaneAttachGuard {
                    pinned: false,
                    archived: false
                }
            )
            .unwrap_err()
            .code,
        "invalid_attach_target"
    );
    f.runtime
        .reattach_lane(
            LaneHumanContext,
            attach_request(&lane, "new", 2, next, "next-lead"),
            LaneAttachGuard {
                pinned: false,
                archived: false,
            },
        )
        .unwrap();
    assert_eq!(
        f.runtime
            .detach_lane(
                LaneManagementContext::Run(lead),
                request(&lane, "old-lead", 3, LaneManagementAction::Detach)
            )
            .unwrap_err()
            .code,
        "not_current_lead"
    );
    f.runtime
        .set_lane_policy(
            LaneManagementContext::Run(next),
            request(
                &lane,
                "new-lead",
                3,
                LaneManagementAction::SetPolicy {
                    policy: ManagedLanePolicy::Attached,
                },
            ),
        )
        .unwrap();
    shutdown(&f, &[lead, next, lane.root_run_id]).await;
}

#[tokio::test]
async fn report_rejects_replaced_root_and_original_execution_authority_loss() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "report-authority").await;
    let host = crate::ownership::OwnerHost::open(
        f._dir.path(),
        Default::default(),
        f.runtime.shared.bus.clone(),
    )
    .unwrap();
    let mut stale = host.start(&lane.thread_id).unwrap();
    stale.lease.generation += 1;
    assert_eq!(
        f.runtime
            .append_lane_agent_report(lane.root_run_id, Some(&stale), report())
            .unwrap_err()
            .code,
        "ownership_lost"
    );
    f.sql(&format!(
        "UPDATE lanes SET root_run_id='{lead}',generation=generation+1 WHERE lane_id='{}'",
        lane.lane_id
    ));
    assert_eq!(
        f.runtime
            .append_lane_agent_report(lane.root_run_id, None, report())
            .unwrap_err()
            .code,
        "lane_unavailable"
    );
    assert!(
        f.storage
            .handle()
            .lane_reports(&lane.lane_id, 0, 20)
            .unwrap()
            .is_empty()
    );
    shutdown(&f, &[lead, lane.root_run_id]).await;
}
#[tokio::test]
async fn old_lead_root_cannot_answer_even_if_delivery_binding_is_absent() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "answer-root").await;
    *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host);
    let q = f
        .runtime
        .request_user_question(lane.root_run_id, "decision".into(), vec![], false)
        .unwrap();
    f.sql(&format!(
        "UPDATE threads SET root_run_id='{}' WHERE thread_id='delivery-lead'",
        lane.root_run_id
    ));
    assert_eq!(
        f.runtime
            .resolve_lane_question(
                lead,
                LaneAnswerRequest {
                    question_id: q.id.clone(),
                    management_revision: 1,
                    routing_revision: q.routing_revision,
                    operation_id: "old-root-answer".into(),
                    answer: Some("late".into())
                }
            )
            .unwrap_err()
            .code,
        "not_current_lead"
    );
    assert!(
        f.runtime
            .user_question(&q.id)
            .unwrap()
            .unwrap()
            .answer
            .is_none()
    );
    shutdown(&f, &[lead, lane.root_run_id]).await;
}
#[tokio::test]
async fn post_commit_question_refresh_failure_returns_success_and_repair_hint() {
    let mut f = Fixture::new();
    let (lead, _) = f.lead().await;
    let lane = f.lane(lead, "refresh-fault").await;
    let q = f
        .runtime
        .request_user_question(lead, "unrelated".into(), vec![], false)
        .unwrap();
    f.sql(&format!(
        "UPDATE user_questions SET payload='invalid' WHERE id='{}'",
        q.id
    ));
    let outcome = detach(&f, &lane, 1);
    assert!(outcome.refresh_pending);
    assert!(outcome.operation.changed);
    assert_eq!(outcome.current_lane.unwrap().management_revision, 2);
    assert_eq!(
        f.storage
            .handle()
            .lane(&lane.lane_id)
            .unwrap()
            .unwrap()
            .policy,
        event_bus::LanePolicy::Detached
    );
    let replay = detach(&f, &lane, 1);
    assert!(replay.operation.replayed);
    shutdown(&f, &[lead, lane.root_run_id]).await;
}

#[tokio::test]
async fn accepted_answer_survives_detach_between_claim_and_accept_and_between_accept_and_ack() {
    // Suppress source scheduling so the explicit driver owns the claim boundary.
    // Otherwise a queued automatic Answer drive can win before BEFORE_QUEUE and
    // leave the barrier waiting for a claim that has already been ACKed.
    integration::MANUAL_SOURCES
        .scope((), accepted_answer_receipt_orders())
        .await;
}
async fn accepted_answer_receipt_orders() {
    for received_first in [false, true] {
        let mut f = Fixture::new();
        let (lead, host) = f.lead().await;
        let lane = f.lane(lead, "answer-receipt").await;
        *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host.clone());
        let q = f
            .runtime
            .request_user_question(lane.root_run_id, "scope".into(), vec![], false)
            .unwrap();
        let accepted = f
            .runtime
            .resolve_lane_question(
                lead,
                LaneAnswerRequest {
                    question_id: q.id.clone(),
                    management_revision: 1,
                    routing_revision: q.routing_revision,
                    operation_id: "accepted-answer".into(),
                    answer: Some("scope decided".into()),
                },
            )
            .unwrap();
        *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(Arc::new(lease::Hosts {
            targets: HashMap::from([(
                lane.thread_id.clone(),
                LaneDeliveryTarget {
                    root_run_id: lane.root_run_id,
                    generation: 1,
                    authority: host.target.authority.clone(),
                },
            )]),
            now: AtomicI64::new(100),
        }));
        let runtime = f.runtime.clone();
        let r = request(
            &lane,
            "answer-receipt-detach",
            1,
            LaneManagementAction::Detach,
        );
        if received_first {
            let hook: Box<dyn FnOnce()> = Box::new(move || {
                runtime.detach_lane(user(), r).unwrap();
            });
            let outcome = AFTER_RECEIPT
                .scope(
                    std::cell::RefCell::new(Some(hook)),
                    f.runtime.drive_lane_deliveries(&lane.thread_id),
                )
                .await
                .unwrap();
            assert_eq!(
                outcome,
                LaneDeliveryOutcome::Nacked(LaneDeliveryNack::OwnershipLost)
            );
            f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
            assert_eq!(f.receipts(), 1);
        } else {
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let gate = barrier.clone();
            let thread = lane.thread_id.clone();
            let task = tokio::spawn(BEFORE_QUEUE.scope(gate, async move {
                runtime.drive_lane_deliveries(&thread).await
            }));
            barrier.wait().await;
            f.runtime.detach_lane(user(), r).unwrap();
            barrier.wait().await;
            assert!(matches!(
                task.await.unwrap().unwrap(),
                LaneDeliveryOutcome::Nacked(_)
            ));
            assert_eq!(f.receipts(), 0);
        }
        assert_eq!(
            f.runtime
                .drive_lane_deliveries(&lane.thread_id)
                .await
                .unwrap(),
            if received_first {
                LaneDeliveryOutcome::Empty
            } else {
                LaneDeliveryOutcome::Delivered { notifications: 1 }
            },
            "receipt repair ACKs without appending an accepted answer again"
        );
        if !received_first {
            f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
        }
        assert_eq!(f.receipts(), 1);
        assert!(
            f.storage
                .handle()
                .pending_lane_delivery_destinations(String::new())
                .unwrap()
                .is_empty()
        );
        let record = f
            .runtime
            .shared
            .run_store
            .get()
            .unwrap()
            .restore_record(lane.root_run_id)
            .unwrap()
            .unwrap();
        let messages: Vec<Message> = serde_json::from_str(&record.messages_json).unwrap();
        assert!(AgentRuntime::answer_is_in_history(&messages, &accepted));
        assert_eq!(messages.iter().flat_map(|m| &m.content).filter(|b| matches!(b, ContentBlock::Text { text } if text == &accepted.answer_message())).count(), 1);
        shutdown(&f, &[lead, lane.root_run_id]).await;
    }
}

#[tokio::test]
async fn detached_runtime_restart_restores_same_root_scope_and_durable_user_question() {
    let mut f = Fixture::new();
    let (lead, host) = f.lead().await;
    let lane = f.lane(lead, "restart").await;
    *f.runtime.shared.lanes.delivery.host.lock().unwrap() = Some(host);
    let q = f
        .runtime
        .request_user_question(lane.root_run_id, "restart question".into(), vec![], false)
        .unwrap();
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::SelfOnly)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    detach(&f, &lane, 1);
    let before = f.storage.handle().lane(&lane.lane_id).unwrap().unwrap();
    let bus = Arc::new(EventBus::new(4096));
    let mut events = bus.subscribe();
    let fresh = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        f.model.clone(),
    )
    .with_run_store(crate::RunStore::open(&f.config, f.storage.handle()).unwrap());
    assert_eq!(
        fresh.latest_chat_run(&lane.thread_id).unwrap(),
        Some(lane.root_run_id)
    );
    assert_eq!(
        fresh
            .resume_chat(lane.root_run_id, RunConfig::default())
            .unwrap(),
        lane.root_run_id
    );
    loop {
        if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
            events.recv().await.unwrap().kind
            && run_id == lane.root_run_id.to_string()
        {
            assert_ne!(to, AgentRunPhase::Error);
            if to == AgentRunPhase::Waiting {
                break;
            }
        }
    }
    assert_eq!(fresh.user_answers(lane.root_run_id).unwrap()[0].id, q.id);
    assert_eq!(
        fresh.user_question(&q.id).unwrap().unwrap().responder,
        Some(QuestionResponder::User)
    );
    assert_eq!(
        f.storage
            .handle()
            .lane(&lane.lane_id)
            .unwrap()
            .unwrap()
            .management_revision,
        before.management_revision
    );
    assert_eq!(
        f.storage
            .handle()
            .lane(&lane.lane_id)
            .unwrap()
            .unwrap()
            .policy,
        event_bus::LanePolicy::Detached
    );
    assert!(
        fresh
            .append_lane_agent_report(lane.root_run_id, None, report())
            .is_err()
    );
    assert!(
        fresh
            .open_lane(lane.root_run_id, "cannot-nest", before.brief)
            .is_err()
    );
    fresh.cancel(lane.root_run_id).unwrap();
    fresh.wait(lane.root_run_id).await.unwrap();
    shutdown(&f, &[lead]).await;
}
