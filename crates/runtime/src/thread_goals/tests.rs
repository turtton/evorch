use super::*;
use crate::{AgentInvocationContext, AgentModel, Role, RunConfig, RuntimeError, StopScope};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock, Weak},
};
use tokio::sync::Notify;

#[derive(Clone)]
enum Step {
    Create,
    Escalate,
    Stop,
    Finish,
    Check(bool),
    Review(bool),
    Hold(Arc<Notify>, Arc<Notify>),
}
type ObservedRequest = (String, Vec<Message>, Vec<ToolSpec>);
struct Model {
    root: Mutex<VecDeque<Step>>,
    reviewer: Mutex<VecDeque<Step>>,
    child: Mutex<VecDeque<Step>>,
    runtime: OnceLock<Weak<crate::runtime::Shared>>,
    requests: Mutex<Vec<ObservedRequest>>,
}
#[async_trait::async_trait]
impl AgentModel for Model {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
    async fn complete(
        &self,
        ctx: &AgentInvocationContext,
        role: Role,
        messages: &[Message],
        specs: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.requests
            .lock()
            .unwrap()
            .push((ctx.run_id.clone(), messages.to_vec(), specs.to_vec()));
        let step = if role == Role::Reviewer {
            self.reviewer.lock().unwrap().pop_front()
        } else if role == Role::Explorer {
            self.child.lock().unwrap().pop_front()
        } else {
            self.root.lock().unwrap().pop_front()
        }
        .expect("unexpected model request");
        let runtime = AgentRuntime::from_weak(self.runtime.get().unwrap()).unwrap();
        let epoch = runtime.thread_goal("thread").map_or(0, |goal| goal.epoch);
        let checks = json!([{"criterion":0,"met":true,"evidence":"Observed result in the requested research report"}]);
        let tool = match step {
            Step::Escalate => Some((
                "escalate",
                json!({"original_request":"Compare the requested options", "escalation_reason":"Need coordinated verification"}),
            )),
            Step::Create => Some((
                "create_goal",
                json!({"objective":"Compare the requested options","criteria":["Report supported differences"]}),
            )),
            Step::Finish => Some(("finish", json!({"result":"Result is ready"}))),
            Step::Check(met) => {
                let mut checks = checks;
                checks[0]["met"] = json!(met);
                Some(("submit_goal_check", json!({"epoch":epoch,"checks":checks})))
            }
            Step::Review(approved) => {
                for forbidden in [
                    "shell",
                    "write",
                    "edit",
                    "delegate",
                    "create_goal",
                    "submit_goal_check",
                ] {
                    assert!(
                        !specs.iter().any(|s| s.name == forbidden),
                        "review exposes {forbidden}"
                    );
                }
                assert!(specs.iter().any(|s| s.name == "submit_goal_review"));
                let mut checks = checks;
                checks[0]["met"] = json!(approved);
                Some((
                    "submit_goal_review",
                    json!({"epoch":epoch,"checks":checks,"findings":if approved {vec![]} else {vec!["Second option lacks supporting evidence"]}}),
                ))
            }
            Step::Hold(started, release) => {
                started.notify_one();
                release.notified().await;
                None
            }
            Step::Stop => None,
        };
        let (content, finish_reason) = match tool {
            Some((name, input)) => (
                vec![ContentBlock::ToolUse {
                    id: format!("call-{}", self.requests.lock().unwrap().len()),
                    name: name.into(),
                    input,
                }],
                FinishReason::ToolUse,
            ),
            None => (
                vec![ContentBlock::Text {
                    text: "Requested output and supporting sources".into(),
                }],
                FinishReason::Stop,
            ),
        };
        Ok(ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content,
            },
            finish_reason,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 2,
                ..Default::default()
            },
        })
    }
}
fn harness(
    root: Vec<Step>,
    reviewer: Vec<Step>,
) -> (AgentRuntime, Arc<Model>, event_bus::EventReceiver) {
    let model = Arc::new(Model {
        root: Mutex::new(root.into()),
        reviewer: Mutex::new(reviewer.into()),
        child: Mutex::new(VecDeque::new()),
        runtime: OnceLock::new(),
        requests: Mutex::new(vec![]),
    });
    let bus = Arc::new(event_bus::EventBus::new(4096));
    let events = bus.subscribe();
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(::tools::ToolExecutor::new(bus)),
        model.clone(),
    );
    model
        .runtime
        .set(Arc::downgrade(&runtime.shared))
        .ok()
        .unwrap();
    (runtime, model, events)
}
async fn phase(events: &mut event_bus::EventReceiver, run: RunId, wanted: AgentRunPhase) {
    loop {
        if let event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
            run_id,
            to,
            reason,
            ..
        }) = events.recv().await.unwrap().kind
            && run_id == run.to_string()
        {
            if to == wanted {
                return;
            }
            assert!(
                !matches!(
                    to,
                    AgentRunPhase::Done | AgentRunPhase::Error | AgentRunPhase::Stopped
                ),
                "unexpected terminal {to:?}: {reason:?}"
            );
        }
    }
}
fn start(runtime: &AgentRuntime, role: Role, keep_alive: bool) -> RunId {
    let root = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", root).unwrap();
    runtime.spawn_reserved(
        root,
        None,
        role,
        "Compare the requested options",
        RunConfig {
            interactive: keep_alive,
            keep_alive,
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn natural_stop_and_finish_both_self_check_without_pr_and_preserve_context() {
    for explicit in [false, true] {
        let stop = if explicit { Step::Finish } else { Step::Stop };
        let (runtime, model, _) = harness(
            vec![Step::Create, stop.clone(), Step::Check(true), stop],
            vec![],
        );
        let root = start(
            &runtime,
            if explicit {
                Role::Orchestrator
            } else {
                Role::Worker
            },
            false,
        );
        assert_eq!(runtime.wait(root).await.unwrap(), AgentRunPhase::Done);
        let goal = runtime.thread_goal("thread").unwrap();
        assert_eq!(goal.phase, ThreadGoalPhase::Complete);
        assert_eq!(goal.checks.len(), 1);
        assert!(!goal.review_enabled);
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        for pair in requests.windows(2) {
            assert_eq!(pair[0].2, pair[1].2, "tool schemas must remain stable");
            assert_eq!(
                pair[1].1[..pair[0].1.len()],
                pair[0].1,
                "provider history must retain its prefix"
            );
        }
    }
}

#[tokio::test]
async fn review_findings_repair_in_original_run_and_retry_to_completion() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (runtime, model, _) = harness(
        vec![
            Step::Create,
            Step::Hold(started.clone(), release.clone()),
            Step::Check(true),
            Step::Stop,
            Step::Stop,
            Step::Check(true),
            Step::Stop,
        ],
        vec![Step::Review(false), Step::Review(true)],
    );
    let root = start(&runtime, Role::Worker, false);
    started.notified().await;
    let goal = runtime.thread_goal("thread").unwrap();
    runtime
        .set_goal_review("thread", &goal.goal_id, true)
        .unwrap();
    release.notify_one();
    assert_eq!(runtime.wait(root).await.unwrap(), AgentRunPhase::Done);
    let goal = runtime.thread_goal("thread").unwrap();
    assert_eq!(goal.phase, ThreadGoalPhase::Complete);
    assert_eq!(goal.review_round, 2);
    assert_eq!(
        goal.usage.model_requests, 8,
        "goal starts after creation request and includes both reviews"
    );
    assert_eq!(
        model
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(run, _, _)| run == &root.to_string())
            .count(),
        7
    );
}

#[tokio::test]
async fn pause_keeps_work_alive_survives_user_input_and_resume_checks_idle() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (runtime, _, mut events) = harness(
        vec![
            Step::Create,
            Step::Hold(started.clone(), release.clone()),
            Step::Stop,
            Step::Stop,
            Step::Check(true),
            Step::Stop,
        ],
        vec![],
    );
    let root = start(&runtime, Role::Worker, true);
    started.notified().await;
    let goal = runtime.thread_goal("thread").unwrap();
    runtime
        .set_goal_checks_paused("thread", &goal.goal_id, true)
        .unwrap();
    assert_eq!(
        runtime.inspect_agent(root).unwrap().phase,
        AgentRunPhase::Running
    );
    release.notify_one();
    phase(&mut events, root, AgentRunPhase::Waiting).await;
    runtime
        .send_message(root, "Clarify the output".into())
        .unwrap();
    phase(&mut events, root, AgentRunPhase::Waiting).await;
    assert!(runtime.thread_goal("thread").unwrap().checks_paused);
    runtime
        .set_goal_checks_paused("thread", &goal.goal_id, false)
        .unwrap();
    phase(&mut events, root, AgentRunPhase::Waiting).await;
    assert_eq!(
        runtime.thread_goal("thread").unwrap().phase,
        ThreadGoalPhase::Complete
    );
    runtime.stop(root, StopScope::SelfOnly).unwrap();
    assert_eq!(runtime.wait(root).await.unwrap(), AgentRunPhase::Stopped);
}

#[tokio::test]
async fn pause_cancels_reviewer_and_new_user_input_cannot_complete_stale_epoch() {
    let root_started = Arc::new(Notify::new());
    let root_release = Arc::new(Notify::new());
    let review_started = Arc::new(Notify::new());
    let review_release = Arc::new(Notify::new());
    let (runtime, _, mut events) = harness(
        vec![
            Step::Create,
            Step::Hold(root_started.clone(), root_release.clone()),
            Step::Check(true),
            Step::Finish,
            Step::Stop,
        ],
        vec![Step::Hold(review_started.clone(), review_release)],
    );
    let root = start(&runtime, Role::Orchestrator, true);
    root_started.notified().await;
    let goal = runtime.thread_goal("thread").unwrap();
    runtime
        .set_goal_review("thread", &goal.goal_id, true)
        .unwrap();
    root_release.notify_one();
    review_started.notified().await;
    let epoch = runtime.thread_goal("thread").unwrap().epoch;
    runtime
        .set_goal_checks_paused("thread", &goal.goal_id, true)
        .unwrap();
    runtime
        .send_message(root, "A new requirement within this request".into())
        .unwrap();
    phase(&mut events, root, AgentRunPhase::Waiting).await;
    let goal = runtime.thread_goal("thread").unwrap();
    assert!(goal.epoch > epoch && goal.checks_paused);
    assert_ne!(goal.phase, ThreadGoalPhase::Complete);
    let reviewer = runtime
        .list_agents()
        .into_iter()
        .find(|run| run.role_name == Role::Reviewer.name())
        .unwrap();
    assert_eq!(
        runtime.wait(reviewer.run_id).await.unwrap(),
        AgentRunPhase::Error
    );
    runtime.stop(root, StopScope::SelfOnly).unwrap();
    runtime.wait(root).await.unwrap();
}

#[tokio::test]
async fn restore_keeps_pause_and_usage_without_spawning_and_stop_resume_does_not_restart_work() {
    let (runtime, model, _) = harness(vec![], vec![]);
    let root = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", root).unwrap();
    let goal = runtime
        .create_thread_goal("thread", root, "Research".into(), vec!["Evidence".into()])
        .unwrap();
    assert!(
        runtime
            .create_thread_goal("thread", root, "Duplicate".into(), vec!["Done".into()])
            .is_err()
    );
    runtime
        .set_goal_checks_paused("thread", &goal.goal_id, true)
        .unwrap();
    runtime.goal_model_request(root, crate::RunPurpose::General, None);
    let saved = runtime.thread_goal("thread").unwrap();
    let (restored, _, _) = harness(vec![], vec![]);
    restored.restore_thread_goal(saved.clone()).unwrap();
    let recovered = restored.thread_goal("thread").unwrap();
    assert!(recovered.checks_paused && recovered.work_stopped);
    assert_eq!(recovered.usage, saved.usage);
    restored
        .set_goal_checks_paused("thread", &goal.goal_id, false)
        .unwrap();
    assert!(restored.thread_goal("thread").unwrap().work_stopped);
    assert!(restored.list_agents().is_empty());
    assert!(model.requests.lock().unwrap().is_empty());
    let mut exhausted = saved;
    exhausted.max_model_requests = 1;
    restored.restore_thread_goal(exhausted).unwrap();
    restored.goal_model_request(root, crate::RunPurpose::General, None);
    assert_eq!(
        restored.thread_goal("thread").unwrap().phase,
        ThreadGoalPhase::Blocked
    );
}

#[tokio::test]
async fn later_turn_goal_captures_current_host_request_not_initial_greeting() {
    let (runtime, _, mut events) = harness(
        vec![
            Step::Stop,
            Step::Create,
            Step::Stop,
            Step::Check(true),
            Step::Stop,
        ],
        vec![],
    );
    let root = runtime
        .delegate_chat(
            "thread",
            Role::Worker,
            "Hello".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    phase(&mut events, root, AgentRunPhase::Waiting).await;
    runtime
        .send_message(root, "Investigate the two storage choices".into())
        .unwrap();
    phase(&mut events, root, AgentRunPhase::Waiting).await;
    let goal = runtime.thread_goal("thread").unwrap();
    assert_eq!(goal.original_request, "Investigate the two storage choices");
    assert_eq!(goal.phase, ThreadGoalPhase::Complete);
    runtime.stop(root, StopScope::SelfOnly).unwrap();
    runtime.wait(root).await.unwrap();
}

#[tokio::test]
async fn rejected_new_goal_does_not_rebind_existing_root() {
    let (runtime, _, _) = harness(vec![], vec![]);
    let root = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", root).unwrap();
    runtime
        .create_thread_goal("thread", root, "Original".into(), vec!["Criterion".into()])
        .unwrap();
    runtime.goal_work_stopped(root);
    let before = runtime.thread_goal("thread").unwrap();
    let error = runtime.delegate_chat(
        "thread",
        Role::Worker,
        "Replacement".into(),
        RunConfig {
            initial_thread_goal: Some(("Replacement".into(), vec!["New criterion".into()])),
            ..Default::default()
        },
    );
    assert!(error.is_err());
    assert_eq!(runtime.thread_goal("thread").unwrap(), before);
    assert!(runtime.list_agents().is_empty());
}

#[test]
fn evidence_requires_every_criterion_once_and_a_nonempty_observation() {
    let criteria = vec!["a".into(), "b".into()];
    let check = |criterion, evidence: &str| ThreadGoalCheck {
        criterion,
        met: true,
        evidence: evidence.into(),
    };
    assert!(validate_checks(&criteria, &[check(0, "observed")], true).is_err());
    assert!(
        validate_checks(
            &criteria,
            &[check(0, "observed"), check(0, "observed")],
            true
        )
        .is_err()
    );
    assert!(validate_checks(&criteria, &[check(0, "observed"), check(1, "")], true).is_err());
    assert!(
        validate_checks(
            &criteria,
            &[check(0, "observed"), check(1, "observed")],
            true
        )
        .is_ok()
    );
}

#[tokio::test]
async fn child_cannot_create_or_bind_a_thread_goal() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (runtime, model, _) = harness(
        vec![Step::Hold(started.clone(), release.clone()), Step::Stop],
        vec![],
    );
    model
        .child
        .lock()
        .unwrap()
        .extend([Step::Create, Step::Stop]);
    let root = start(&runtime, Role::Worker, false);
    started.notified().await;
    let child = runtime.reserve_child_run_id(root).unwrap();
    runtime.spawn_reserved_child(
        root,
        child,
        Role::Explorer,
        "Attempt goal creation",
        RunConfig::default(),
    );
    assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    assert!(runtime.bind_thread_root("other-thread", child).is_err());
    assert!(runtime.thread_goal("thread").is_none());
    {
        let requests = model.requests.lock().unwrap();
        for (_, _, specs) in requests
            .iter()
            .filter(|(run, _, _)| run == &child.to_string())
        {
            assert!(!specs.iter().any(|spec| spec.name == "create_goal"));
        }
    }
    release.notify_one();
    runtime.wait(root).await.unwrap();
}

#[tokio::test]
async fn unsettled_children_delay_review_and_their_requests_count_toward_goal_budget() {
    let root_started = Arc::new(Notify::new());
    let root_release = Arc::new(Notify::new());
    let held_started = Arc::new(Notify::new());
    let held_release = Arc::new(Notify::new());
    let child_started = Arc::new(Notify::new());
    let child_release = Arc::new(Notify::new());
    let (runtime, model, _) = harness(
        vec![
            Step::Create,
            Step::Hold(root_started.clone(), root_release.clone()),
            Step::Hold(held_started.clone(), held_release.clone()),
            Step::Stop,
            Step::Check(true),
            Step::Stop,
        ],
        vec![Step::Review(true)],
    );
    model
        .child
        .lock()
        .unwrap()
        .extend([Step::Hold(child_started.clone(), child_release.clone())]);
    let root = start(&runtime, Role::Worker, false);
    root_started.notified().await;
    let goal = runtime.thread_goal("thread").unwrap();
    runtime
        .set_goal_review("thread", &goal.goal_id, true)
        .unwrap();
    let child = runtime.reserve_child_run_id(root).unwrap();
    runtime.spawn_reserved_child(
        root,
        child,
        Role::Explorer,
        "Read evidence",
        RunConfig::default(),
    );
    child_started.notified().await;
    root_release.notify_one();
    held_started.notified().await;
    assert_eq!(runtime.thread_goal("thread").unwrap().review_round, 0);
    assert!(
        !runtime
            .list_agents()
            .iter()
            .any(|run| run.role_name == Role::Reviewer.name())
    );
    assert_eq!(
        runtime.thread_goal("thread").unwrap().usage.model_requests,
        3
    );
    child_release.notify_one();
    runtime.wait(child).await.unwrap();
    held_release.notify_one();
    assert_eq!(runtime.wait(root).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(
        runtime.thread_goal("thread").unwrap().phase,
        ThreadGoalPhase::Complete
    );
}

#[tokio::test]
async fn token_budget_includes_review_and_cannot_reset_on_restore() {
    let (runtime, _, _) = harness(vec![], vec![]);
    let root = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", root).unwrap();
    runtime
        .create_thread_goal("thread", root, "Research".into(), vec!["Evidence".into()])
        .unwrap();
    assert!(runtime.goal_model_request(root, crate::RunPurpose::General, Some(20)));
    runtime.goal_usage(
        root,
        crate::RunPurpose::General,
        Usage {
            input_tokens: 10,
            output_tokens: 2,
            ..Default::default()
        },
    );
    let reviewer = RunId::new(100);
    let purpose = crate::RunPurpose::ThreadGoalReview {
        root_run_id: root,
        epoch: 0,
    };
    assert!(runtime.goal_model_request(reviewer, purpose, None));
    runtime.goal_usage(
        reviewer,
        purpose,
        Usage {
            input_tokens: 8,
            output_tokens: 2,
            ..Default::default()
        },
    );
    let saved = runtime.thread_goal("thread").unwrap();
    assert_eq!(saved.phase, ThreadGoalPhase::Blocked);
    assert_eq!(saved.usage.input_tokens + saved.usage.output_tokens, 22);
    let (restored, _, _) = harness(vec![], vec![]);
    restored.restore_thread_goal(saved).unwrap();
    restored.bind_thread_root("thread", root).unwrap();
    assert!(!restored.goal_model_request(root, crate::RunPurpose::General, None));
    assert_eq!(restored.thread_goal("thread").unwrap().max_tokens, Some(20));
}

#[tokio::test]
async fn stale_ui_controls_cannot_relabel_completed_goal_as_reviewed() {
    let (runtime, _, _) = harness(
        vec![Step::Create, Step::Stop, Step::Check(true), Step::Stop],
        vec![],
    );
    let root = start(&runtime, Role::Worker, false);
    runtime.wait(root).await.unwrap();
    let completed = runtime.thread_goal("thread").unwrap();
    assert_eq!(completed.phase, ThreadGoalPhase::Complete);
    assert!(
        runtime
            .set_goal_review("thread", &completed.goal_id, true)
            .is_err()
    );
    assert!(
        runtime
            .set_goal_checks_paused("thread", &completed.goal_id, true)
            .is_err()
    );
    assert!(
        runtime
            .set_goal_review("thread", &completed.goal_id, false)
            .is_ok()
    );
    assert_eq!(runtime.thread_goal("thread").unwrap(), completed);
}

#[tokio::test]
async fn blocked_goal_requires_explicit_host_replacement_to_start_new_budget() {
    let (runtime, model, _) = harness(vec![], vec![]);
    let root = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", root).unwrap();
    runtime
        .create_thread_goal(
            "thread",
            root,
            "Original request".into(),
            vec!["Evidence".into()],
        )
        .unwrap();
    let mut exhausted = runtime.thread_goal("thread").unwrap();
    exhausted.max_model_requests = 0;
    runtime.restore_thread_goal(exhausted).unwrap();
    assert!(!runtime.goal_model_request(root, crate::RunPurpose::General, None));
    let blocked = runtime.thread_goal("thread").unwrap();
    assert!(blocked.reason.as_ref().unwrap().contains("/goal"));
    assert!(
        runtime
            .create_agent_thread_goal(
                "thread",
                root,
                "Reset myself".into(),
                vec!["Evidence".into()]
            )
            .is_err()
    );
    assert_eq!(runtime.thread_goal("thread").unwrap(), blocked);
    assert!(
        runtime
            .create_thread_goal("thread", root, " ".into(), vec!["Evidence".into()])
            .is_err()
    );
    assert_eq!(runtime.thread_goal("thread").unwrap(), blocked);
    let next = runtime
        .create_thread_goal(
            "thread",
            root,
            "New explicit request".into(),
            vec!["New evidence".into()],
        )
        .unwrap();
    assert_ne!(next.goal_id, blocked.goal_id);
    assert_eq!(next.original_request, "New explicit request");
    assert_eq!(next.usage, ThreadGoalUsage::default());
    assert_eq!(next.phase, ThreadGoalPhase::Working);
    assert!(runtime.list_agents().is_empty());
    assert!(model.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn trusted_root_handoff_preserves_goal_pause_request_and_cumulative_usage() {
    let (runtime, _, _) = harness(vec![], vec![]);
    let source = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", source).unwrap();
    runtime.goal_user_input(source, "Original human request");
    let goal = runtime
        .create_thread_goal(
            "thread",
            source,
            "Objective".into(),
            vec!["Evidence".into()],
        )
        .unwrap();
    runtime
        .set_goal_checks_paused("thread", &goal.goal_id, true)
        .unwrap();
    assert!(runtime.goal_model_request(source, crate::RunPurpose::General, Some(100)));
    let previous = runtime.thread_goal("thread").unwrap();
    let target = runtime.reserve_run_id();
    runtime.transfer_thread_goal_root(source, target).unwrap();
    let current = runtime.thread_goal("thread").unwrap();
    assert_eq!(current.goal_id, previous.goal_id);
    assert_eq!(current.root_run_id, target.to_string());
    assert_eq!(current.usage, previous.usage);
    assert_eq!(current.max_tokens, Some(100));
    assert!(current.checks_paused);
    assert!(current.epoch > previous.epoch);
    assert!(runtime.goal_thread(source).is_none());
    assert_eq!(
        runtime
            .goal_lock()
            .requests
            .get(&target)
            .map(String::as_str),
        Some("Original human request")
    );
}

#[tokio::test]
async fn surviving_old_root_child_remains_accounted_and_fenced_after_handoff() {
    let root_started = Arc::new(Notify::new());
    let root_release = Arc::new(Notify::new());
    let child_started = Arc::new(Notify::new());
    let child_release = Arc::new(Notify::new());
    let (runtime, model, _) = harness(
        vec![Step::Create, Step::Hold(root_started.clone(), root_release)],
        vec![],
    );
    model
        .child
        .lock()
        .unwrap()
        .extend([Step::Hold(child_started.clone(), child_release.clone())]);
    let source = start(&runtime, Role::Worker, false);
    root_started.notified().await;
    let child = runtime.reserve_child_run_id(source).unwrap();
    runtime.spawn_reserved_child(
        source,
        child,
        Role::Explorer,
        "Background evidence",
        RunConfig::default(),
    );
    child_started.notified().await;
    let target = runtime.reserve_run_id();
    runtime.transfer_thread_goal_root(source, target).unwrap();
    runtime.cancel(source).unwrap();
    runtime.wait(source).await.unwrap();
    assert_eq!(runtime.goal_owner_for_run(child), Some(target));
    assert_eq!(runtime.active_goal_children(target), vec![child]);
    let before = runtime.thread_goal("thread").unwrap().usage.model_requests;
    assert!(runtime.goal_model_request(child, crate::RunPurpose::General, None));
    assert_eq!(
        runtime.thread_goal("thread").unwrap().usage.model_requests,
        before + 1
    );
    {
        let mut goals = runtime.goal_lock();
        let entry = goals.goals.get_mut("thread").unwrap();
        entry.snapshot.phase = ThreadGoalPhase::Checking;
        entry.snapshot.checks = vec![ThreadGoalCheck {
            criterion: 0,
            met: true,
            evidence: "Earlier evidence".into(),
        }];
    }
    let epoch = runtime.thread_goal("thread").unwrap().epoch;
    runtime.goal_tool_activity(child, "write");
    assert!(runtime.thread_goal("thread").unwrap().epoch > epoch);
    runtime
        .goal_lock()
        .goals
        .get_mut("thread")
        .unwrap()
        .snapshot
        .phase = ThreadGoalPhase::Blocked;
    runtime
        .create_thread_goal(
            "thread",
            target,
            "Explicit replacement".into(),
            vec!["New evidence".into()],
        )
        .unwrap();
    assert_eq!(runtime.goal_owner_for_run(child), Some(target));
    assert_eq!(
        runtime.active_goal_children(target),
        vec![child],
        "the replacement completion gate must still wait for old background work"
    );
    child_release.notify_one();
    runtime.wait(child).await.unwrap();
    assert!(runtime.active_goal_children(target).is_empty());
    let saved = runtime.thread_goal("thread").unwrap();
    assert_eq!(saved.related_root_run_ids, vec![source.to_string()]);
    let (restored, _, _) = harness(vec![], vec![]);
    restored.restore_thread_goal(saved).unwrap();
    assert!(
        restored.active_goal_children(target).is_empty(),
        "historical roots are not live after restart"
    );
}

#[tokio::test]
async fn real_escalation_keeps_paused_chat_alive_and_fences_root_and_reviewer() {
    use crate::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
    let directory = tempfile::tempdir().unwrap();
    let registry_path = directory.path().join("owners.db");
    let mut registry = Registry::open(&registry_path).unwrap();
    let lease = Lease {
        owner_id: "host".into(),
        generation: 1,
        expires_at: u64::MAX,
    };
    registry
        .start(&ThreadOwner::new("thread".into(), lease.clone()))
        .unwrap();
    let permit = OwnerPermit {
        registry_path,
        thread_id: "thread".into(),
        lease,
        run_id: None,
    };
    let review_started = Arc::new(Notify::new());
    let review_release = Arc::new(Notify::new());
    let (runtime, _, mut events) = harness(
        vec![Step::Escalate, Step::Stop, Step::Check(true), Step::Stop],
        vec![Step::Hold(review_started.clone(), review_release.clone())],
    );
    let source = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", source).unwrap();
    let goal = runtime
        .create_thread_goal(
            "thread",
            source,
            "Compare options".into(),
            vec!["Supported differences".into()],
        )
        .unwrap();
    runtime
        .set_goal_review("thread", &goal.goal_id, true)
        .unwrap();
    runtime
        .set_goal_checks_paused("thread", &goal.goal_id, true)
        .unwrap();
    runtime.spawn_reserved(
        source,
        None,
        Role::Worker,
        "Compare options",
        RunConfig {
            interactive: true,
            keep_alive: true,
            ownership: Some(permit),
            ..Default::default()
        },
    );
    let target = loop {
        if let event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::EscalationRequested {
            new_run_id,
            ..
        }) = events.recv().await.unwrap().kind
        {
            break crate::meta::parse_run_id(&new_run_id).unwrap();
        }
    };
    phase(&mut events, target, AgentRunPhase::Waiting).await;
    assert_eq!(runtime.wait(source).await.unwrap(), AgentRunPhase::Done);
    assert!(runtime.thread_goal("thread").unwrap().checks_paused);
    assert_eq!(
        runtime.thread_goal("thread").unwrap().root_run_id,
        target.to_string()
    );
    runtime
        .set_goal_checks_paused("thread", &goal.goal_id, false)
        .unwrap();
    review_started.notified().await;
    let reviewer = runtime
        .list_agents()
        .into_iter()
        .find(|run| run.role_name == Role::Reviewer.name())
        .unwrap();
    assert!(
        registry
            .attach("thread")
            .unwrap()
            .active_runs
            .contains(&reviewer.run_id.to_string()),
        "the independent reviewer must inherit the owner lease with its own run ID"
    );
    registry
        .update("thread", |owner| {
            owner.lease.generation += 1;
            Ok(())
        })
        .unwrap();
    assert!(
        matches!(
            runtime.send_message(target, "stale host follow-up".into()),
            Err(RuntimeError::StaleOwnership { .. })
        ),
        "the escalated root must retain owner-generation fencing"
    );
    review_release.notify_one();
    assert_eq!(
        runtime.wait(reviewer.run_id).await.unwrap(),
        AgentRunPhase::Error
    );
    assert_eq!(runtime.wait(target).await.unwrap(), AgentRunPhase::Error);
    assert_ne!(
        runtime.thread_goal("thread").unwrap().phase,
        ThreadGoalPhase::Complete
    );
}
