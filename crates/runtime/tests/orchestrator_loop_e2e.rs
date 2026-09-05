mod support;

use std::sync::Arc;

use event_bus::{
    AgentRunPhase, ApprovalDecision, CiState, EventBus, EventKind, GateEvidence, GateRejection,
    GoalStage, GoalState, OrchestratorEvent, RunPurpose,
};
use providers::FinishReason;
use runtime::orchestration::delivery::FixtureDeliveryAdapter;
use runtime::orchestration::ledger::OrchestrationSettings;
use runtime::orchestration::supervisor::{GoalSpec, GoalSupervisor};
use runtime::workspace::{Project, WorktreeManager};
use runtime::{AgentRuntime, Role, RunConfig};
use sandbox::DirectSandbox;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::{ScriptedModel, init_git_repo, recording_factory, text_response, tool_response};

const HEAD_A: &str = "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1";
const HEAD_B: &str = "a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2";

async fn wait_for_event(
    receiver: &mut event_bus::EventReceiver,
    predicate: impl Fn(&OrchestratorEvent) -> bool,
) -> OrchestratorEvent {
    timeout(Duration::from_secs(5), async {
        loop {
            if let EventKind::Orchestrator(event) =
                receiver.recv().await.expect("event bus open").kind
                && predicate(&event)
            {
                return event;
            }
        }
    })
    .await
    .expect("orchestrator event timeout")
}

fn spec() -> GoalSpec {
    GoalSpec {
        session_id: "session-loop".into(),
        project_id: "evorch".into(),
        thread_id: "thread-loop".into(),
        goal: "deliver issue 73".into(),
        references: vec![],
        constraints: vec!["tests green".into()],
        repo: "turtton/evorch".into(),
        base_ref: "main".into(),
    }
}

fn runtime_with_workspace(model: Arc<ScriptedModel>) -> (TempDir, AgentRuntime, Arc<EventBus>) {
    let (temp, repo) = init_git_repo();
    let bus = Arc::new(EventBus::new(1024));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let manager = WorktreeManager::new(Project::new(repo).expect("git repo"));
    let (factory, _) = recording_factory();
    (
        temp,
        AgentRuntime::with_workspace_context(Arc::clone(&bus), executor, model, manager, factory),
        bus,
    )
}

#[tokio::test]
async fn goal_runs_to_awaiting_merge_then_complete_with_one_request_update_round() {
    // Given: root が isolated worker を委譲し、review 1 が修正要求、review 2 が承認するモデル
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "IMPLEMENT-LOOP",
            [Ok(text_response("implemented", FinishReason::Stop))],
        )
        .await;
    model
        .add_keyed(
            "[evorch review round=1",
            [Ok(text_response(
                "```json\n{\"verdict\":\"request-update\",\"findings\":[\"fix one\"],\"criteria\":[{\"id\":\"ac-1\",\"status\":\"unmet\",\"note\":\"fix\"}]}\n```",
                FinishReason::Stop,
            ))],
        )
        .await;
    model
        .add_keyed(
            "[evorch repair round=1",
            [Ok(text_response("repaired", FinishReason::Stop))],
        )
        .await;
    model
        .add_keyed(
            "[evorch review round=2",
            [Ok(text_response(
                "```json\n{\"verdict\":\"approve\",\"findings\":[],\"criteria\":[{\"id\":\"ac-1\",\"status\":\"met\",\"note\":\"ok\"}]}\n```",
                FinishReason::Stop,
            ))],
        )
        .await;
    model
        .add_keyed(
            "[evorch continuation",
            [Ok(tool_response(
                "finish",
                "finish",
                json!({"result": "delivered"}),
            ))],
        )
        .await;
    let (_temp, runtime, bus) = runtime_with_workspace(Arc::clone(&model));
    let mut events = bus.subscribe();
    let delivery = Arc::new(FixtureDeliveryAdapter::scripted_happy_path());
    let settings = OrchestrationSettings {
        ci_poll_secs: 1,
        ci_timeout_secs: 5,
        stall_after_secs: 60,
        ..OrchestrationSettings::default()
    };
    let handle = GoalSupervisor::spawn(runtime.clone(), Arc::clone(&bus), delivery, settings);
    model
        .add_keyed(
            "ROOT-LOOP",
            [
                Ok(tool_response(
                    "worker",
                    "delegate_background",
                    json!({
                        "role": "worker",
                        "prompt": "IMPLEMENT-LOOP",
                        "workspace_mode": "isolated"
                    }),
                )),
                Ok(text_response("root done", FinishReason::Stop)),
            ],
        )
        .await;
    let root_gate = Arc::new(Notify::new());
    model.gate_key("ROOT-LOOP", Arc::clone(&root_gate)).await;
    let root =
        runtime.delegate_background(Role::Orchestrator, "ROOT-LOOP".into(), RunConfig::default());

    // When: goal を作成して finish acceptance まで supervisor に駆動させる
    let goal_id = handle.create_goal(spec(), root);
    timeout(Duration::from_secs(2), async {
        while handle.snapshot(&goal_id).is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("goal creation timeout");
    root_gate.notify_waiters();
    timeout(Duration::from_secs(2), async {
        while handle
            .snapshot(&goal_id)
            .is_some_and(|snapshot| snapshot.deliverable_branch.is_none())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("deliverable timeout");
    root_gate.notify_waiters();
    let (approval, observed_events) = timeout(Duration::from_secs(5), async {
        let mut observed = Vec::new();
        loop {
            if let EventKind::Orchestrator(event) =
                events.recv().await.expect("event bus open").kind
            {
                let approval = matches!(
                    &event,
                    OrchestratorEvent::MergeApprovalRequested { goal_id: id, .. } if id == &goal_id
                );
                observed.push(event.clone());
                if approval {
                    break (event, observed);
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("approval timeout snapshot={:?}", handle.snapshot(&goal_id)));
    let OrchestratorEvent::MergeApprovalRequested { binding, .. } = approval else {
        unreachable!()
    };

    // Then: delivery/review/repair/continuation/finish の主要イベントが契約順で発生する
    let snapshot = handle.snapshot(&goal_id).expect("goal snapshot");
    assert_eq!(snapshot.stage, GoalStage::AwaitingMergeApproval);
    assert!(
        snapshot
            .attached_runs
            .iter()
            .any(|run| run.purpose == RunPurpose::Implement)
    );
    assert_eq!(
        snapshot.deliverable_branch.as_deref(),
        Some("evorch/task/run-2")
    );
    assert_eq!(
        snapshot
            .pull_request
            .as_ref()
            .map(|pr| pr.head_sha.as_str()),
        Some(HEAD_B)
    );
    assert_eq!(
        snapshot.ci.as_ref().map(|ci| &ci.state),
        Some(&CiState::Green)
    );
    assert_eq!(snapshot.review_rounds, 2);
    assert_eq!(snapshot.repair_rounds, 1);
    assert!(snapshot.dispatched_epochs.contains(&1));
    assert!(snapshot.accepted_snapshot.is_some());
    let positions = [
        observed_events
            .iter()
            .position(|event| matches!(event, OrchestratorEvent::GoalCreated { .. })),
        observed_events.iter().position(|event| {
            matches!(
                event,
                OrchestratorEvent::RunAttached {
                    purpose: RunPurpose::Implement,
                    ..
                }
            )
        }),
        observed_events
            .iter()
            .position(|event| matches!(event, OrchestratorEvent::DeliverableBranchBound { .. })),
        observed_events.iter().position(|event| {
            matches!(
                event,
                OrchestratorEvent::ReviewRoundStarted { round: 1, .. }
            )
        }),
        observed_events.iter().position(|event| {
            matches!(event, OrchestratorEvent::RepairDispatched { round: 1, .. })
        }),
        observed_events.iter().position(|event| {
            matches!(
                event,
                OrchestratorEvent::ReviewRoundStarted { round: 2, .. }
            )
        }),
        observed_events.iter().position(|event| {
            matches!(
                event,
                OrchestratorEvent::GoalStageChanged {
                    to: GoalStage::ReadyToFinish,
                    ..
                }
            )
        }),
        observed_events
            .iter()
            .position(|event| matches!(event, OrchestratorEvent::ContinuationDispatched { .. })),
        observed_events
            .iter()
            .position(|event| matches!(event, OrchestratorEvent::FinishAccepted { .. })),
        observed_events
            .iter()
            .position(|event| matches!(event, OrchestratorEvent::MergeApprovalRequested { .. })),
    ];
    assert!(positions.iter().all(Option::is_some));
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));

    // When: SHA-bound token を承認する
    handle
        .decide_merge(binding.token_id, ApprovalDecision::Approved)
        .expect("merge decision accepted");
    timeout(Duration::from_secs(5), async {
        loop {
            if handle
                .snapshot(&goal_id)
                .is_some_and(|snapshot| snapshot.state == GoalState::Complete)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "completion timeout snapshot={:?}",
            handle.snapshot(&goal_id)
        )
    });

    // Then: merge、3 closeout、Complete まで到達する
    let completed = handle.snapshot(&goal_id).expect("completed snapshot");
    assert_eq!(completed.state, GoalState::Complete);
    assert_eq!(completed.stage, GoalStage::Done);
    assert!(
        completed
            .merge_result
            .as_ref()
            .is_some_and(|(_, head, ok, _)| *ok && head == HEAD_B)
    );
    assert_eq!(completed.closeout_steps.len(), 3);
    assert!(completed.closeout_steps.iter().all(|step| step.ok));
}

#[tokio::test]
async fn early_finish_is_rejected_with_no_deliverable_branch_and_goal_stays_active() {
    // Given: finish を即座に呼ぶ root run と未配信 goal
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [Ok(tool_response(
            "finish",
            "finish",
            json!({"result": "too early"}),
        ))],
        Arc::clone(&gate),
    ));
    let bus = Arc::new(EventBus::new(256));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model);
    let mut events = bus.subscribe();
    let handle = GoalSupervisor::spawn(
        runtime.clone(),
        Arc::clone(&bus),
        Arc::new(FixtureDeliveryAdapter::default()),
        OrchestrationSettings::default(),
    );
    let root =
        runtime.delegate_background(Role::Orchestrator, "EARLY".into(), RunConfig::default());
    let goal_id = handle.create_goal(spec(), root);
    timeout(Duration::from_secs(2), async {
        while handle.snapshot(&goal_id).is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("goal creation timeout");
    gate.notify_one();

    // When: gate 未充足で finish が評価される
    let rejected = wait_for_event(&mut events, |event| {
        matches!(event, OrchestratorEvent::FinishRejected { goal_id: id, .. } if id == &goal_id)
    })
    .await;

    // Then: NoDeliverableBranch を含み、finish は run/goal を完了させない
    assert!(
        matches!(rejected, OrchestratorEvent::FinishRejected { rejections, .. } if rejections.contains(&GateRejection::NoDeliverableBranch))
    );
    assert_eq!(
        handle.snapshot(&goal_id).expect("snapshot").state,
        GoalState::Active
    );
    assert_eq!(
        runtime.inspect_agent(root).expect("root inspection").phase,
        AgentRunPhase::Running
    );
    gate.notify_waiters();
    runtime.cancel(root).expect("cancel root");
}

#[tokio::test]
async fn finish_after_head_change_rejected_stale_head() {
    // Given: gate 証跡は HEAD_A だが remote PR status は HEAD_B を返す goal
    let model = Arc::new(ScriptedModel::new([Ok(tool_response(
        "finish",
        "finish",
        json!({"result": "stale"}),
    ))]));
    let bus = Arc::new(EventBus::new(256));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model);
    let delivery = Arc::new(FixtureDeliveryAdapter::default());
    delivery.script_pr_status(Ok(GateEvidence::PullRequest {
        repo: "turtton/evorch".into(),
        number: 101,
        url: "url".into(),
        base_ref: "main".into(),
        head_sha: HEAD_B.into(),
    }));
    let mut events = bus.subscribe();
    let handle = GoalSupervisor::spawn(
        runtime.clone(),
        Arc::clone(&bus),
        delivery,
        OrchestrationSettings::default(),
    );
    let root =
        runtime.delegate_background(Role::Orchestrator, "STALE".into(), RunConfig::default());
    let goal_id = handle.create_goal(spec(), root);
    for event in [
        OrchestratorEvent::DeliverableBranchBound {
            goal_id: goal_id.clone(),
            branch: "feature".into(),
            run_id: root.to_string(),
        },
        OrchestratorEvent::EvidenceRecorded {
            goal_id: goal_id.clone(),
            evidence: GateEvidence::PullRequest {
                repo: "turtton/evorch".into(),
                number: 101,
                url: "url".into(),
                base_ref: "main".into(),
                head_sha: HEAD_A.into(),
            },
        },
    ] {
        bus.emit(event_bus::Event::new(event));
    }

    // When: remote head refresh 後に finish を評価する
    let rejected = wait_for_event(&mut events, |event| matches!(event, OrchestratorEvent::FinishRejected { goal_id: id, .. } if id == &goal_id)).await;

    // Then: pull_request の stale head が明示される
    assert!(
        matches!(rejected, OrchestratorEvent::FinishRejected { rejections, .. } if rejections.iter().any(|item| matches!(item, GateRejection::StaleHead { evidence, evidence_head, current_head } if evidence == "pull_request" && evidence_head == HEAD_A && current_head == HEAD_B)))
    );
}

#[tokio::test]
async fn finish_is_rejected_when_remote_head_unavailable() {
    // Given: PR 証跡を持つ goal と、pr_status が常に失敗する delivery adapter
    let gate = Arc::new(Notify::new());
    let model = Arc::new(ScriptedModel::gated(
        [Ok(tool_response(
            "finish",
            "finish",
            json!({"result": "delivered"}),
        ))],
        Arc::clone(&gate),
    ));
    let bus = Arc::new(EventBus::new(256));
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    let runtime = AgentRuntime::new(Arc::clone(&bus), executor, model);
    let mut events = bus.subscribe();
    let handle = GoalSupervisor::spawn(
        runtime.clone(),
        Arc::clone(&bus),
        Arc::new(FixtureDeliveryAdapter::default()),
        OrchestrationSettings::default(),
    );
    let root =
        runtime.delegate_background(Role::Orchestrator, "HEAD-LOSS".into(), RunConfig::default());
    let goal_id = handle.create_goal(spec(), root);
    timeout(Duration::from_secs(2), async {
        while handle.snapshot(&goal_id).is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("goal creation timeout");
    for event in [
        OrchestratorEvent::DeliverableBranchBound {
            goal_id: goal_id.clone(),
            branch: "feature".into(),
            run_id: root.to_string(),
        },
        OrchestratorEvent::EvidenceRecorded {
            goal_id: goal_id.clone(),
            evidence: GateEvidence::PullRequest {
                repo: "turtton/evorch".into(),
                number: 101,
                url: "url".into(),
                base_ref: "main".into(),
                head_sha: HEAD_A.into(),
            },
        },
    ] {
        bus.emit(event_bus::Event::new(event));
    }
    gate.notify_one();

    // When: remote head を取得できないまま finish が評価される
    let rejected = wait_for_event(&mut events, |event| {
        matches!(event, OrchestratorEvent::FinishRejected { goal_id: id, .. } if id == &goal_id)
    })
    .await;

    // Then: RemoteHeadUnavailable で拒否され、goal は Active のまま
    assert!(
        matches!(rejected, OrchestratorEvent::FinishRejected { rejections, .. } if rejections.iter().any(|item| matches!(item, GateRejection::RemoteHeadUnavailable { .. })))
    );
    assert_eq!(
        handle.snapshot(&goal_id).expect("snapshot").state,
        GoalState::Active
    );
    gate.notify_waiters();
    runtime.cancel(root).expect("cancel root");
}
