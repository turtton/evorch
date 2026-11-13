use super::*;
#[path = "recovery_tests.rs"]
mod recovery;
use event_bus::{AgentRunPhase, EventBus, EventKind, LifecycleEvent};
use providers::{ContentBlock, Message, ToolSpec};
use std::sync::Arc;
type Request = (String, Vec<Message>, Vec<ToolSpec>);
#[derive(Default)]
struct Model {
    requests: Mutex<Vec<Request>>,
    admissions: Mutex<Vec<String>>,
    hold_admission: std::sync::atomic::AtomicBool,
    reject_admission: std::sync::atomic::AtomicBool,
    hold_execution: std::sync::atomic::AtomicBool,
    switch_lane: std::sync::atomic::AtomicBool,
    switched: Mutex<HashSet<String>>,
    lead: Mutex<Option<String>>,
    admission_entered: tokio::sync::Notify,
    release_admission: tokio::sync::Notify,
    execution_entered: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl crate::AgentModel for Model {
    async fn complete(
        &self,
        ctx: &crate::AgentInvocationContext,
        _: Role,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        self.requests
            .lock()
            .unwrap()
            .push((ctx.run_id.clone(), messages.to_vec(), tools.to_vec()));
        if self.hold_execution.load(std::sync::atomic::Ordering::SeqCst)
            && messages.iter().any(|m| m.role == providers::Role::User && m.content.iter().any(|b| matches!(b,ContentBlock::Text {text} if text.contains("Agent-authored lane brief")))) {
            self.execution_entered.notify_one();
            std::future::pending::<()>().await;
        }
        if self.switch_lane.load(std::sync::atomic::Ordering::SeqCst)
            && messages.iter().any(|m| m.content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text.contains("Agent-authored lane brief"))))
            && self.switched.lock().unwrap().insert(ctx.run_id.clone())
        {
            return Err(crate::RuntimeError::ModelSwitch { selected: "alternate/test".into(), resummarize: false });
        }
        if messages.last().is_some_and(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text == "list lanes"))
        }) {
            return Ok(providers::ChatResponse {
                message: Message {
                    role: providers::Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: format!("lane-list-{}", messages.len()),
                        name: "lane_list".into(),
                        input: serde_json::json!({}),
                    }],
                },
                finish_reason: providers::FinishReason::ToolUse,
                usage: Default::default(),
                usage_reported: true,
            });
        }
        Ok(providers::ChatResponse {
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "turn complete".into(),
                }],
            },
            finish_reason: providers::FinishReason::Stop,
            usage: Default::default(),
            usage_reported: true,
        })
    }
    fn requires_admission(&self) -> bool {
        true
    }
    async fn admit(
        &self,
        ctx: &crate::AgentInvocationContext,
        _: Role,
    ) -> Result<(), crate::RuntimeError> {
        self.admissions.lock().unwrap().push(ctx.run_id.clone());
        let is_lead = self.lead.lock().unwrap().as_deref() == Some(ctx.run_id.as_str());
        if !is_lead
            && self
                .reject_admission
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(crate::RuntimeError::Model {
                reason: "lane route unavailable".into(),
            });
        }
        if !is_lead
            && self
                .hold_admission
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.admission_entered.notify_one();
            self.release_admission.notified().await;
        }
        Ok(())
    }
    fn selected_model_for_run(&self, id: &str, _: Role, _: Option<&str>) -> String {
        if self.switched.lock().unwrap().contains(id) {
            "alternate/test"
        } else {
            "test"
        }
        .into()
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "test".into()
    }
}
struct Fixture {
    runtime: AgentRuntime,
    model: Arc<Model>,
    storage: storage::Storage,
    events: event_bus::EventReceiver,
    _dir: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("lanes.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let model = Arc::new(Model::default());
        let bus = Arc::new(EventBus::new(4096));
        let events = bus.subscribe();
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            model.clone(),
        )
        .with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
        Self {
            runtime,
            model,
            storage,
            events,
            _dir: dir,
        }
    }
    async fn phase(&mut self, run: RunId, phase: AgentRunPhase) {
        loop {
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to,
                reason,
                ..
            }) = self.events.recv().await.unwrap().kind
                && run_id == run.to_string()
            {
                assert_ne!(to, AgentRunPhase::Error, "{reason:?}");
                if to == phase {
                    return;
                }
            }
        }
    }
    async fn lead(&mut self) -> RunId {
        let lead = self
            .runtime
            .delegate_chat(
                "lead",
                Role::Orchestrator,
                "coordinate".into(),
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    conversation: true,
                    ..Default::default()
                },
            )
            .unwrap();
        self.phase(lead, AgentRunPhase::Waiting).await;
        *self.model.lead.lock().unwrap() = Some(lead.to_string());
        lead
    }
    fn config(&self, id: RunId) -> RunConfig {
        self.runtime
            .shared
            .runs
            .lock()
            .unwrap()
            .get(&id)
            .unwrap()
            .config
            .clone()
    }
    async fn cancel(&self, id: RunId) {
        self.runtime.cancel(id).unwrap();
        self.runtime.wait(id).await.unwrap();
    }
}
fn brief() -> LaneBrief {
    LaneBrief {
        verification_requirements: Default::default(),
        objective: "an independent deliverable".into(),
        inputs: vec![],
        deliverables: vec!["artifact".into()],
        acceptance_criteria: vec!["independent review".into()],
        constraints: vec![],
        workspace: Default::default(),
        report_policy: LanePolicy::Attached,
    }
}
#[tokio::test]
async fn independent_roots_retries_and_lead_stop_resume() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    let lane = f.runtime.open_lane(lead, "call", brief()).unwrap();
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    let retry = f.runtime.open_lane(lead, "call", brief()).unwrap();
    assert_eq!(retry.root_run_id, lane.root_run_id);
    assert!(!retry.created && !retry.started);
    assert_eq!(
        f.model
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(run, _, _)| run == &lane.root_run_id.to_string())
            .count(),
        1
    );
    let other = f.runtime.open_lane(lead, "another", brief()).unwrap();
    f.phase(other.root_run_id, AgentRunPhase::Waiting).await;
    assert_eq!(
        f.runtime
            .open_lane(lane.root_run_id, "nested", brief())
            .unwrap_err()
            .code,
        "lane_creation_denied"
    );
    for id in [lane.root_run_id, other.root_run_id] {
        assert!(
            f.runtime
                .list_agents()
                .iter()
                .any(|row| row.run_id == id && row.parent_run_id.is_none())
        );
    }
    let child = f
        .runtime
        .delegate_background_as_child(
            lane.root_run_id,
            Role::Worker,
            "child",
            RunConfig::default(),
        )
        .unwrap();
    f.runtime.wait(child).await.unwrap();
    assert!(
        f.runtime.shared.runs.lock().unwrap()[&child]
            .config
            .lane_scope
            .is_some()
    );
    assert_eq!(
        f.runtime
            .open_lane(child, "nested-child", brief())
            .unwrap_err()
            .code,
        "lane_creation_denied"
    );
    f.runtime.stop(lead, crate::StopScope::Subtree).unwrap();
    f.runtime.wait(lead).await.unwrap();
    assert_eq!(
        f.runtime.inspect_agent(lane.root_run_id).unwrap().phase,
        AgentRunPhase::Waiting
    );
    f.runtime.resume_chat(lead, RunConfig::default()).unwrap();
    f.phase(lead, AgentRunPhase::Waiting).await;
    f.cancel(lead).await;
    f.cancel(lane.root_run_id).await;
    f.cancel(other.root_run_id).await;
    let record = f.storage.handle().lane(&lane.lane_id).unwrap().unwrap();
    assert_eq!(record.root_run_id, Some(lane.root_run_id.to_string()));
    let requests = f.model.requests.lock().unwrap();
    let (_, messages, tools) = requests
        .iter()
        .find(|(run, _, _)| run == &lane.root_run_id.to_string())
        .unwrap();
    assert!(
        !tools
            .iter()
            .any(|s| s.name == "lane_open" || s.name == "lane_list")
    );
    assert!(tools.iter().any(|s| s.name == "lane_report"));
    let first_user = messages
        .iter()
        .find(|m| m.role == providers::Role::User)
        .unwrap();
    let text = first_user
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(text.contains(&record.brief.objective));
    assert!(text.contains("Agent-authored lane brief"));
}
#[tokio::test]
async fn unstarted_intent_survives_and_recovery_is_idempotent() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    let id = f.runtime.shared.run_ids.next();
    f.storage
        .handle()
        .create_lane(NewLane {
            lane_id: "crash-lane".into(),
            thread_id: "crash-thread".into(),
            origin_thread_id: "lead".into(),
            origin_run_id: lead.to_string(),
            creation_key: "crash-call".into(),
            brief: brief(),
            start_seed: serde_json::to_string(&StartSeed {
                run_id: id,
                project_root: None,
                requires_ownership: false,
            })
            .unwrap(),
            project_id: None,
            project_root: None,
            title: None,
            at_ns: 1,
        })
        .unwrap();
    assert_eq!(f.runtime.recover_unstarted_lanes(None).unwrap(), vec![id]);
    assert!(f.runtime.recover_unstarted_lanes(None).unwrap().is_empty());
    f.phase(id, AgentRunPhase::Waiting).await;
    assert!(f.storage.handle().unstarted_lanes(10).unwrap().is_empty());
    f.cancel(id).await;
    f.cancel(lead).await;
}
#[tokio::test]
async fn restored_lane_scope_and_schema_do_not_gain_open() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    let lane = f.runtime.open_lane(lead, "scope", brief()).unwrap();
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::SelfOnly)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    f.runtime
        .resume_chat(lane.root_run_id, RunConfig::default())
        .unwrap();
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    f.runtime
        .send_message(lane.root_run_id, "next".into())
        .unwrap();
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    assert_eq!(
        f.runtime
            .open_lane(lane.root_run_id, "escape", brief())
            .unwrap_err()
            .code,
        "lane_creation_denied"
    );
    let config = f.runtime.shared.runs.lock().unwrap()[&lane.root_run_id]
        .config
        .clone();
    assert_eq!(config.lane_scope, Some(LaneScope::Lane));
    assert_eq!(
        config.budget.max_tool_calls,
        crate::budget_tracker::ResourceLimit::Unlimited
    );
    f.cancel(lead).await;
    f.cancel(lane.root_run_id).await;
    let requests = f.model.requests.lock().unwrap();
    let turns: Vec<_> = requests
        .iter()
        .filter(|(run, _, _)| run == &lane.root_run_id.to_string())
        .collect();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].2, turns[1].2);
}

#[tokio::test]
async fn twenty_first_lane_is_not_a_quantity_limit_and_invalid_brief_leaves_no_intent() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    let mut invalid = brief();
    invalid.acceptance_criteria.clear();
    assert_eq!(
        f.runtime
            .open_lane(lead, "invalid", invalid)
            .unwrap_err()
            .code,
        "invalid_brief"
    );
    let mut lanes = Vec::new();
    for index in 0..21 {
        let lane = f
            .runtime
            .open_lane(lead, &format!("call-{index}"), brief())
            .unwrap();
        f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
        lanes.push(lane.root_run_id);
    }
    let page = f
        .storage
        .handle()
        .lane_list(storage::lanes::LaneListRequest {
            lead_thread_id: "lead".into(),
            policy: None,
            cursor: None,
            limit: 20,
        })
        .unwrap();
    assert_eq!(page.items.len(), 20);
    assert!(page.has_more);
    for lane in lanes {
        f.cancel(lane).await;
    }
    f.cancel(lead).await;
}

#[tokio::test]
async fn active_execution_and_pending_admission_do_not_join_lead_renewal_subtree() {
    for admission in [false, true] {
        let mut f = Fixture::new();
        let lead = f.lead().await;
        if admission {
            f.model
                .hold_admission
                .store(true, std::sync::atomic::Ordering::SeqCst);
        } else {
            f.model
                .hold_execution
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let lane = f.runtime.open_lane(lead, "held-lane", brief()).unwrap();
        if admission {
            f.model.admission_entered.notified().await;
        } else {
            f.model.execution_entered.notified().await;
        }
        f.runtime.stop(lead, crate::StopScope::Subtree).unwrap();
        f.runtime.wait(lead).await.unwrap();
        f.runtime
            .resume_chat(
                lead,
                RunConfig {
                    conversation: true,
                    ..Default::default()
                },
            )
            .unwrap();
        f.phase(lead, AgentRunPhase::Waiting).await;
        if admission {
            assert!(f.runtime.shared.lanes.started.lock().unwrap().is_empty());
        } else {
            assert_eq!(
                f.runtime.inspect_agent(lane.root_run_id).unwrap().phase,
                AgentRunPhase::Running
            );
        }
        if admission {
            f.runtime.cancel(lane.root_run_id).unwrap();
            f.model.release_admission.notify_one();
            assert!(matches!(
                f.runtime.wait(lane.root_run_id).await,
                Err(crate::RuntimeError::RunTerminated { .. })
            ));
        } else {
            f.cancel(lane.root_run_id).await;
        }
        f.cancel(lead).await;
    }
}
#[tokio::test]
async fn preview_questions_belong_to_lane_user_not_lead_under_every_policy() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    for (index, policy) in [
        LanePolicy::Attached,
        LanePolicy::Summary,
        LanePolicy::Detached,
    ]
    .into_iter()
    .enumerate()
    {
        let mut brief = brief();
        brief.report_policy = policy;
        let lane = f
            .runtime
            .open_lane(lead, &format!("policy-{index}"), brief)
            .unwrap();
        f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
        let question = f
            .runtime
            .request_user_question(lane.root_run_id, "clarify scope".into(), vec![], true)
            .unwrap();
        assert_eq!(question.run_id, lane.root_run_id.to_string());
        assert_eq!(question.root_run_id, lane.root_run_id.to_string());
        assert!(f.runtime.user_answers(lead).unwrap().is_empty());
        assert_eq!(
            f.runtime.inspect_agent(lead).unwrap().phase,
            AgentRunPhase::Waiting
        );
        f.cancel(lane.root_run_id).await;
    }
    f.cancel(lead).await;
}

#[tokio::test]
async fn fork_and_new_root_rewind_cannot_drop_inherited_lane_scope() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    let lane = f.runtime.open_lane(lead, "fork-source", brief()).unwrap();
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
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
    let seed = crate::ChatForkSeed {
        source_run_id: lane.root_run_id.to_string(),
        context_len: crate::restore::conversation_len(&messages) as u64,
    };
    let config = || RunConfig {
        conversation: true,
        interactive: true,
        keep_alive: true,
        ..Default::default()
    };
    let fork = f
        .runtime
        .delegate_chat_seeded(
            "forked-lane",
            Role::Orchestrator,
            "fork follow-up".into(),
            config(),
            Some(seed),
        )
        .unwrap();
    f.phase(fork, AgentRunPhase::Waiting).await;
    assert_eq!(f.config(fork).lane_scope, Some(LaneScope::Lane));
    assert_eq!(
        f.runtime
            .open_lane(fork, "escape", brief())
            .unwrap_err()
            .code,
        "lane_creation_denied"
    );
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::Subtree)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    let rewind = f
        .runtime
        .delegate_chat(
            &lane.thread_id,
            Role::Orchestrator,
            "rewind follow-up".into(),
            config(),
        )
        .unwrap();
    f.phase(rewind, AgentRunPhase::Waiting).await;
    assert_ne!(rewind, lane.root_run_id);
    assert_eq!(f.config(rewind).lane_scope, Some(LaneScope::Lane));
    assert_eq!(
        f.storage
            .handle()
            .lane(&lane.lane_id)
            .unwrap()
            .unwrap()
            .root_run_id,
        Some(rewind.to_string())
    );
    for id in [fork, rewind, lead] {
        f.cancel(id).await;
    }
}

#[tokio::test]
async fn failed_admission_does_not_earn_credit_or_duplicate_intent_on_retry() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    f.model
        .reject_admission
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let lane = f
        .runtime
        .open_lane(lead, "failed-admission", brief())
        .unwrap();
    assert!(f.runtime.wait(lane.root_run_id).await.is_err());
    assert!(f.runtime.shared.lanes.started.lock().unwrap().is_empty());
    assert_eq!(
        f.runtime
            .open_lane(lead, "failed-admission", brief())
            .unwrap_err()
            .code,
        "lane_creation_denied"
    );
    assert_eq!(f.storage.handle().unstarted_lanes(20).unwrap().len(), 1);
    f.cancel(lead).await;
}
#[tokio::test]
async fn independent_lane_lease_remains_authoritative_after_lead_stops() {
    use crate::ownership::{Lease, OwnerPermit, Registry, ThreadOwner};
    let mut f = Fixture::new();
    let path = f._dir.path().join("owners.db");
    let mut registry = Registry::open(&path).unwrap();
    let owner = ThreadOwner::new(
        "owned-lead".into(),
        Lease {
            owner_id: "test-host".into(),
            generation: 1,
            expires_at: u64::MAX,
        },
    );
    registry.start(&owner).unwrap();
    let permit = OwnerPermit {
        registry_path: path,
        thread_id: owner.thread_id.clone(),
        lease: owner.lease.clone(),
        run_id: None,
    };
    let lead = f
        .runtime
        .delegate_chat(
            "owned-lead",
            Role::Orchestrator,
            "coordinate".into(),
            RunConfig {
                ownership: Some(permit.clone()),
                conversation: true,
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    f.phase(lead, AgentRunPhase::Waiting).await;
    let lane = f.runtime.open_lane(lead, "owned-lane", brief()).unwrap();
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    let lane_permit = f.config(lane.root_run_id).ownership.clone().unwrap();
    assert_eq!(lane_permit.thread_id, lane.thread_id);
    assert_ne!(lane_permit.thread_id, permit.thread_id);
    assert_eq!(registry.attach("owned-lead").unwrap().lease, owner.lease);
    f.runtime.stop(lead, crate::StopScope::Subtree).unwrap();
    f.runtime.wait(lead).await.unwrap();
    lane_permit.validate_generation().unwrap();
    assert_eq!(
        f.runtime.inspect_agent(lane.root_run_id).unwrap().phase,
        AgentRunPhase::Waiting
    );
    f.cancel(lane.root_run_id).await;
}

#[tokio::test]
async fn creation_event_can_project_committed_root_and_never_promotes_brief_to_human_evidence() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    let mut events = f.runtime.shared.bus.subscribe();
    let lane = f.runtime.open_lane(lead, "projectable", brief()).unwrap();
    loop {
        let event = events.recv().await.unwrap();
        if let EventKind::Ledger(event_bus::LedgerEvent::LaneCreated {
            lane_id,
            thread_id,
            origin_run_id,
            origin_thread_id,
            policy,
            ..
        }) = event.kind
        {
            assert_eq!(lane_id, lane.lane_id);
            assert_eq!(thread_id, lane.thread_id);
            assert_eq!(origin_run_id, lead.to_string());
            assert_eq!(origin_thread_id, "lead");
            assert_eq!(policy, LanePolicy::Attached);
            let committed = f.storage.handle().lane(&lane_id).unwrap().unwrap();
            assert_eq!(committed.root_run_id, Some(lane.root_run_id.to_string()));
            break;
        }
    }
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    assert!(
        f.runtime
            .shared
            .review_runs
            .lock()
            .unwrap()
            .get(&lane.root_run_id)
            .unwrap()
            .requests()
            .0
            .is_empty()
    );
    assert_eq!(
        f.model
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _, _)| id == &lead.to_string())
            .count(),
        1
    );
    for id in [lane.root_run_id, lead] {
        f.cancel(id).await;
    }
}

#[tokio::test]
async fn lane_unlimited_quantities_preserve_configured_runtime_runaway_thresholds() {
    use crate::budget_tracker::{BudgetSettings, ResourceLimit};
    let mut f = Fixture::new();
    let mut config = config::Config::default();
    config.budget.max_no_progress_rounds = 7;
    config.budget.max_identical_tool_call_repeats = 3;
    config.budget.max_file_rereads = 11;
    f.runtime = f.runtime.with_model_resolution(&config, None, None);
    let lead = f.lead().await;
    let lane = f
        .runtime
        .open_lane(lead, "configured-budget", brief())
        .unwrap();
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    let expected = BudgetSettings::from(&config.budget);
    assert_eq!(f.config(lead).budget, expected);
    assert_eq!(
        f.config(lane.root_run_id).budget,
        expected.with_unlimited_resources()
    );
    assert_eq!(
        f.config(lead).budget.max_tool_calls,
        ResourceLimit::Bounded(400)
    );
    for id in [lane.root_run_id, lead] {
        f.cancel(id).await;
    }
}

impl Fixture {
    fn activity_time(&self, at_ns: i64) {
        *self.runtime.shared.lanes.activity_time.lock().unwrap() = Some(at_ns);
    }
    async fn list(&mut self, lead: RunId) -> serde_json::Value {
        self.runtime
            .send_message(lead, "list lanes".into())
            .unwrap();
        self.phase(lead, AgentRunPhase::Waiting).await;
        let requests = self.model.requests.lock().unwrap();
        let (_, messages, _) = requests
            .iter()
            .rev()
            .find(|(id, _, _)| id == &lead.to_string())
            .unwrap();
        let result = messages
            .iter()
            .rev()
            .flat_map(|m| m.content.iter())
            .find_map(|b| {
                if let ContentBlock::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                } = b
                    && tool_call_id.starts_with("lane-list-")
                {
                    assert!(!is_error, "{content:?}");
                    return Some(
                        content
                            .iter()
                            .filter_map(|c| {
                                if let providers::ToolResultContent::Text { text } = c {
                                    Some(text.as_str())
                                } else {
                                    None
                                }
                            })
                            .collect::<String>(),
                    );
                }
                None
            })
            .unwrap();
        serde_json::from_str(&result).unwrap()
    }
}

#[tokio::test]
async fn real_turn_without_report_reorders_lanes_but_list_reads_do_not_touch_activity() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    f.activity_time(100);
    let a = f.runtime.open_lane(lead, "a", brief()).unwrap();
    f.phase(a.root_run_id, AgentRunPhase::Waiting).await;
    f.activity_time(200);
    let b = f.runtime.open_lane(lead, "b", brief()).unwrap();
    f.phase(b.root_run_id, AgentRunPhase::Waiting).await;
    let first = f.list(lead).await;
    assert_eq!(first["items"][0]["lane_id"], b.lane_id);
    f.activity_time(300);
    f.runtime
        .send_message(a.root_run_id, "normal input without a report".into())
        .unwrap();
    f.phase(a.root_run_id, AgentRunPhase::Waiting).await;
    let changed = f.list(lead).await;
    assert_eq!(changed["items"][0]["lane_id"], a.lane_id);
    assert_eq!(changed["items"][0]["last_activity_at_ns"], 300);
    assert_eq!(changed["items"][1]["last_activity_at_ns"], 200);
    assert!(
        changed["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["latest_report"].is_null())
    );
    f.activity_time(400);
    let reread = f.list(lead).await;
    assert_eq!(reread["items"], changed["items"]);
    assert_eq!(
        f.storage
            .handle()
            .lane(&a.lane_id)
            .unwrap()
            .unwrap()
            .last_activity_at_ns,
        300
    );
    assert_eq!(
        f.storage
            .handle()
            .lane(&b.lane_id)
            .unwrap()
            .unwrap()
            .last_activity_at_ns,
        200
    );
    for id in [a.root_run_id, b.root_run_id, lead] {
        f.cancel(id).await;
    }
}

#[tokio::test]
async fn lane_child_message_restore_preserves_scope_and_rejects_nested_open() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    let lane = f.runtime.open_lane(lead, "child-restore", brief()).unwrap();
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    let child = f
        .runtime
        .delegate_background_as_child(lane.root_run_id, Role::Worker, "work", RunConfig::default())
        .unwrap();
    assert_eq!(f.runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    f.runtime
        .send_agent_message(
            lane.root_run_id,
            child,
            event_bus::AgentMessageKind::Send,
            "restore",
            None,
        )
        .unwrap();
    assert_eq!(f.runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(f.config(child).lane_scope, Some(LaneScope::Lane));
    assert_eq!(
        f.runtime
            .open_lane(child, "escape", brief())
            .unwrap_err()
            .code,
        "lane_creation_denied"
    );
    let turns: Vec<_> = f
        .model
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(id, _, _)| id == &child.to_string())
        .cloned()
        .collect();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].2, turns[1].2);
    assert!(
        turns
            .iter()
            .all(|(_, _, tools)| !tools.iter().any(|t| t.name == "lane_open"))
    );
    for id in [lane.root_run_id, lead] {
        f.cancel(id).await;
    }
}

#[tokio::test]
async fn lane_model_switch_keeps_fixed_schema_and_rejects_nested_open() {
    let mut f = Fixture::new();
    let lead = f.lead().await;
    f.model
        .switch_lane
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let lane = f.runtime.open_lane(lead, "model-switch", brief()).unwrap();
    f.phase(lane.root_run_id, AgentRunPhase::Waiting).await;
    assert!(
        f.model
            .switched
            .lock()
            .unwrap()
            .contains(&lane.root_run_id.to_string())
    );
    assert_eq!(
        f.runtime
            .open_lane(lane.root_run_id, "escape", brief())
            .unwrap_err()
            .code,
        "lane_creation_denied"
    );
    let turns: Vec<_> = f
        .model
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(id, _, _)| id == &lane.root_run_id.to_string())
        .cloned()
        .collect();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].2, turns[1].2);
    assert!(
        turns
            .iter()
            .all(|(_, _, tools)| !tools.iter().any(|t| t.name == "lane_open"))
    );
    for id in [lane.root_run_id, lead] {
        f.cancel(id).await;
    }
}
