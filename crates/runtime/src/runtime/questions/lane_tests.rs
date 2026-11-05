use super::routing::LaneAnswerRequest;
use super::*;
#[path = "forwarding_tests.rs"]
mod forwarding_tests;
use crate::{Role, RunConfig, RunStore};
use event_bus::lane::{LaneDeliveryBinding, QuestionResponder};
use event_bus::{EventBus, EventKind, LaneBrief, LanePolicy, LifecycleEvent};
use std::sync::Arc;
use storage::lane_delivery::LaneQuestionCreated;

#[derive(Default)]
struct Idle {
    requests: std::sync::Mutex<Vec<Vec<providers::Message>>>,
    toolsets: std::sync::Mutex<Vec<Vec<providers::ToolSpec>>>,
}
#[async_trait::async_trait]
impl crate::AgentModel for Idle {
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "idle".into()
    }
    async fn complete(
        &self,
        _: &crate::AgentInvocationContext,
        _: Role,
        messages: &[providers::Message],
        tools: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, crate::RuntimeError> {
        self.requests.lock().unwrap().push(messages.to_vec());
        self.toolsets.lock().unwrap().push(tools.to_vec());
        Ok(providers::ChatResponse {
            message: message(providers::Role::Assistant, "idle"),
            finish_reason: providers::FinishReason::Stop,
            usage: Default::default(),
            usage_reported: true,
        })
    }
}
struct Fixture {
    runtime: AgentRuntime,
    model: Arc<Idle>,
    db: storage::Storage,
    events: event_bus::EventReceiver,
    _dir: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("questions.db"),
            ..Default::default()
        };
        let db = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(4096));
        let events = bus.subscribe();
        let model = Arc::new(Idle::default());
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            model.clone(),
        )
        .with_run_store(RunStore::open(&config, db.handle()).unwrap());
        Self {
            runtime,
            model,
            db,
            events,
            _dir: dir,
        }
    }
    async fn waiting(&mut self, run: RunId) {
        loop {
            if let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
                self.events.recv().await.unwrap().kind
                && run_id == run.to_string()
            {
                assert_ne!(to, AgentRunPhase::Error);
                if to == AgentRunPhase::Waiting {
                    break;
                }
            }
        }
    }
    async fn lead(&mut self, name: &str) -> RunId {
        let run = self
            .runtime
            .delegate_chat(
                name,
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
        self.waiting(run).await;
        run
    }
    async fn lane(
        &mut self,
        lead: RunId,
        key: &str,
        policy: LanePolicy,
    ) -> crate::lanes::LaneOpened {
        let lane = self
            .runtime
            .open_lane(
                lead,
                key,
                LaneBrief {
                    objective: key.into(),
                    inputs: vec![],
                    deliverables: vec!["artifact".into()],
                    acceptance_criteria: vec!["review".into()],
                    constraints: vec![],
                    workspace: Default::default(),
                    report_policy: policy,
                },
            )
            .unwrap();
        self.waiting(lane.root_run_id).await;
        lane
    }
    async fn stop(&self, run: RunId) {
        self.runtime.cancel(run).unwrap();
        self.runtime.wait(run).await.unwrap();
    }
}
fn created(outcome: LaneQuestionCreated) -> UserQuestion {
    match outcome {
        LaneQuestionCreated::Created(q) => *q,
        other => panic!("{other:?}"),
    }
}
fn request(question: &UserQuestion, op: &str, answer: Option<&str>) -> LaneAnswerRequest {
    LaneAnswerRequest {
        question_id: question.id.clone(),
        management_revision: question.management_revision.unwrap(),
        routing_revision: question.routing_revision,
        operation_id: op.into(),
        answer: answer.map(str::to_owned),
    }
}
#[tokio::test]
async fn lane_question_routing_is_fail_safe_and_does_not_route_children() {
    let mut f = Fixture::new();
    let lead = f.lead("lead").await;
    for (i, policy) in [
        LanePolicy::Attached,
        LanePolicy::Summary,
        LanePolicy::Detached,
    ]
    .into_iter()
    .enumerate()
    {
        let lane = f.lane(lead, &format!("lane-{i}"), policy).await;
        let unbound = f
            .runtime
            .request_user_question(lane.root_run_id, "unbound?".into(), vec![], false)
            .unwrap();
        assert_eq!(unbound.responder, Some(QuestionResponder::User));
        let bound = created(
            f.runtime
                .request_question_with_binding(
                    lane.root_run_id,
                    "bound?".into(),
                    vec![],
                    false,
                    LaneDeliveryBinding::Bound,
                )
                .unwrap(),
        );
        assert_eq!(
            matches!(bound.responder, Some(QuestionResponder::Lead { .. })),
            policy == LanePolicy::Attached
        );
        let child = f
            .runtime
            .spawn_reserved_child(
                lane.root_run_id,
                f.runtime.reserve_run_id(),
                Role::Worker,
                "child",
                RunConfig::default(),
            )
            .unwrap();
        f.runtime.wait_admission(child).await.unwrap();
        let question = created(
            f.runtime
                .request_question_with_binding(
                    child,
                    "child?".into(),
                    vec![],
                    false,
                    LaneDeliveryBinding::Bound,
                )
                .unwrap(),
        );
        assert_eq!(
            question.responder, None,
            "children still ask their parent, not the lane lead"
        );
        f.stop(child).await;
        f.stop(lane.root_run_id).await;
    }
    f.stop(lead).await;
}
#[tokio::test]
async fn lead_questions_are_independent_paginated_and_answers_are_untrusted() {
    let mut f = Fixture::new();
    let lead = f.lead("lead").await;
    let other = f.lead("other").await;
    for i in 0..32 {
        f.runtime
            .request_user_question(lead, format!("own {i}?"), vec![], false)
            .unwrap();
    }
    let mut lanes = Vec::new();
    for i in 0..10 {
        let lane = f
            .lane(lead, &format!("lane-{i}"), LanePolicy::Attached)
            .await;
        for j in 0..4 {
            created(
                f.runtime
                    .request_question_with_binding(
                        lane.root_run_id,
                        format!("question {j}?"),
                        vec![],
                        true,
                        LaneDeliveryBinding::Bound,
                    )
                    .unwrap(),
            );
        }
        lanes.push(lane);
    }
    assert_eq!(f.runtime.user_answers(lead).unwrap().len(), 32);
    assert!(!f.runtime.has_pending_user_questions(lead));
    let mut cursor = None;
    let mut questions = Vec::new();
    loop {
        let page = f.runtime.list_lane_questions(lead, cursor).unwrap();
        assert!(page.questions.len() <= 20);
        assert!(serde_json::to_vec(&page).unwrap().len() <= 16384);
        questions.extend(page.questions);
        if !page.has_more {
            break;
        }
        cursor = page.next_cursor;
    }
    assert_eq!(questions.len(), 40);
    let first = &questions[0];
    assert_eq!(
        f.runtime
            .resolve_lane_question(other, request(first, "wrong", Some("A")))
            .unwrap_err()
            .code,
        "not_current_lead"
    );
    let mut stale = request(first, "old", Some("A"));
    stale.management_revision += 1;
    assert_eq!(
        f.runtime
            .resolve_lane_question(lead, stale)
            .unwrap_err()
            .code,
        "stale_management"
    );
    let mut stale = request(first, "old", Some("A"));
    stale.routing_revision += 1;
    assert_eq!(
        f.runtime
            .resolve_lane_question(lead, stale)
            .unwrap_err()
            .code,
        "stale_routing"
    );
    assert!(f.runtime.answer_user_question(&first.id, "human").is_err());
    let lane_run = crate::meta::parse_run_id(&first.run_id).unwrap();
    let before = f.runtime.shared.review_runs.lock().unwrap()[&lane_run].visible_request_count();
    let answered = f
        .runtime
        .resolve_lane_question(lead, request(first, "answer", Some("agent answer")))
        .unwrap();
    assert!(matches!(
        answered.answer_origin,
        Some(AnswerOrigin::LaneLead { .. })
    ));
    assert_eq!(
        f.runtime
            .resolve_lane_question(lead, request(first, "answer", Some("agent answer")))
            .unwrap(),
        answered
    );
    assert_eq!(
        f.runtime.shared.review_runs.lock().unwrap()[&lane_run].visible_request_count(),
        before
    );
    // A competing operation uses the SAME revision from the original listing.
    let second = request(first, "second", Some("another"));
    assert_eq!(
        f.runtime
            .resolve_lane_question(lead, second)
            .unwrap_err()
            .code,
        "already_answered"
    );
    let tag = answered.answer_message();
    assert!(tag.starts_with("[lane-answer "));
    let history = vec![message(providers::Role::User, &tag)];
    assert!(AgentRuntime::answer_is_in_history(&history, &answered));
    assert!(!AgentRuntime::answer_is_in_history(
        &[message(providers::Role::Assistant, &tag)],
        &answered
    ));
    let routed = f
        .runtime
        .resolve_lane_question(lead, request(&questions[1], "route", None))
        .unwrap();
    assert_eq!(routed.id, questions[1].id);
    assert!(routed.is_user_visible());
    assert_eq!(
        f.runtime
            .resolve_lane_question(lead, request(&questions[1], "late", Some("late")))
            .unwrap_err()
            .code,
        "stale_routing"
    );
    let human = f
        .runtime
        .answer_user_question(&routed.id, "real user")
        .unwrap();
    assert!(matches!(
        human.answer_origin,
        Some(AnswerOrigin::Host { .. })
    ));
    assert_eq!(
        f.runtime
            .resolve_lane_question(
                lead,
                request(&questions[1], "late-after-human", Some("late"))
            )
            .unwrap_err()
            .code,
        "stale_routing"
    );
    assert_eq!(f.runtime.user_answers(lead).unwrap().len(), 32);
    for q in &questions[2..] {
        f.runtime
            .resolve_lane_question(
                lead,
                request(q, &format!("answer-{}", q.id), Some("answer")),
            )
            .unwrap();
    }
    assert!(
        f.runtime
            .list_lane_questions(lead, None)
            .unwrap()
            .questions
            .is_empty()
    );
    for lane in lanes {
        let notices =
            f.db.handle()
                .lane_inbox(lane.thread_id, String::new())
                .unwrap();
        assert!(!notices.is_empty());
        f.stop(lane.root_run_id).await;
    }
    f.stop(lead).await;
    f.stop(other).await;
}
#[tokio::test]
async fn lane_capacity_is_typed_bounded_and_does_not_create_or_retry() {
    let mut f = Fixture::new();
    let lead = f.lead("lead").await;
    let lane = f.lane(lead, "lane", LanePolicy::Attached).await;
    for i in 0..32 {
        f.runtime
            .request_user_question(lane.root_run_id, format!("{i}?"), vec![], false)
            .unwrap();
    }
    for attempt in 1..=2 {
        let outcome = f
            .runtime
            .request_user_question_outcome(lane.root_run_id, "excess?".into(), vec![], false)
            .unwrap();
        assert!(matches!(
            outcome,
            LaneQuestionCreated::CapacityBlocked(storage::lane_delivery::QuestionCapacityBlocked {
                capacity: storage::lane_delivery::QuestionCapacity::Requester,
                ..
            })
        ));
        let diag =
            f.db.handle()
                .lane_question_capacity_diagnostic(lane.lane_id.clone())
                .unwrap()
                .unwrap();
        assert_eq!(diag.attempts, attempt);
        assert!(serde_json::to_vec(&diag).unwrap().len() < 512);
        assert_eq!(f.runtime.user_answers(lane.root_run_id).unwrap().len(), 32);
    }
    // The same typed path handles the global 1024->1025 boundary. Synthetic
    // offline requesters isolate global capacity from the lane's 32-row limit.
    for i in 0..992 {
        let run = format!("run-capacity-{}", i / 32);
        f.db.handle()
            .create_user_question(&UserQuestion {
                responder: None,
                routing_revision: 0,
                management_revision: None,
                id: format!("question-capacity-{i}"),
                run_id: run.clone(),
                root_run_id: run,
                root_name: "offline capacity fixture".into(),
                recipient_run_ids: vec![],
                title: "capacity?".into(),
                options: vec![],
                blocking: false,
                answer: None,
                answer_origin: None,
            })
            .unwrap();
    }
    let second = f.lane(lead, "global-lane", LanePolicy::Attached).await;
    let blocked = f
        .runtime
        .request_user_question_outcome(second.root_run_id, "global excess?".into(), vec![], false)
        .unwrap();
    assert!(matches!(
        blocked,
        LaneQuestionCreated::CapacityBlocked(storage::lane_delivery::QuestionCapacityBlocked {
            capacity: storage::lane_delivery::QuestionCapacity::Global,
            ..
        })
    ));
    assert!(
        f.runtime
            .user_answers(second.root_run_id)
            .unwrap()
            .is_empty()
    );
    f.stop(second.root_run_id).await;
    f.stop(lane.root_run_id).await;
    f.stop(lead).await;
}

fn message(role: providers::Role, text: &str) -> providers::Message {
    providers::Message {
        role,
        content: vec![providers::ContentBlock::Text { text: text.into() }],
    }
}

#[cfg(unix)]
#[tokio::test]
async fn lane_answer_rejects_fenced_ownership_and_rebound_root() {
    let mut f = Fixture::new();
    let lead = f.lead("lead").await;
    let lane = f.lane(lead, "lane", LanePolicy::Attached).await;
    let question = created(
        f.runtime
            .request_question_with_binding(
                lane.root_run_id,
                "scope?".into(),
                vec![],
                false,
                LaneDeliveryBinding::Bound,
            )
            .unwrap(),
    );
    // Relative private directory keeps the Unix socket path below SUN_LEN even
    // when the runner's TMPDIR is long. No global cwd/environment mutation.
    let dir = tempfile::Builder::new()
        .prefix(".b-owner-")
        .tempdir_in(".")
        .unwrap();
    let owner_path = std::path::Path::new(".").join(dir.path().file_name().unwrap());
    let first = crate::ownership::OwnerHost::open(
        &owner_path,
        config::OwnershipConfig::default(),
        f.runtime.shared.bus.clone(),
    )
    .unwrap();
    let old = first.start("lead").unwrap();
    f.runtime
        .shared
        .runs
        .lock()
        .unwrap()
        .get_mut(&lead)
        .unwrap()
        .config
        .ownership = Some(old.clone());
    assert!(!first.quiesce().unwrap());
    let second = crate::ownership::OwnerHost::open(
        &owner_path,
        config::OwnershipConfig::default(),
        f.runtime.shared.bus.clone(),
    )
    .unwrap();
    let current = second.claim(&second.attach("lead").unwrap()).unwrap();
    assert!(current.lease.generation > old.lease.generation);
    assert!(
        f.runtime
            .resolve_lane_question(lead, request(&question, "stale-owner", Some("denied")))
            .is_err()
    );
    assert!(
        f.runtime
            .user_question(&question.id)
            .unwrap()
            .unwrap()
            .answer
            .is_none()
    );
    f.runtime
        .shared
        .runs
        .lock()
        .unwrap()
        .get_mut(&lead)
        .unwrap()
        .config
        .ownership = None;
    f.db.handle()
        .bind_lane_delivery_destination(
            "lead".into(),
            0,
            storage::lane_delivery::LaneDeliveryOwner {
                root_run_id: "run-new-root".into(),
                generation: 2,
                ownership_generation: current.lease.generation,
            },
            storage::lane_delivery::LaneDestinationAction::Bind,
        )
        .unwrap();
    assert_eq!(
        f.runtime
            .resolve_lane_question(lead, request(&question, "old-root", Some("denied")))
            .unwrap_err()
            .code,
        "not_current_lead"
    );
    assert!(
        f.runtime
            .user_question(&question.id)
            .unwrap()
            .unwrap()
            .answer
            .is_none()
    );
    f.stop(lane.root_run_id).await;
    f.stop(lead).await;
}

#[tokio::test]
async fn lane_lead_answer_is_injected_once_across_explicit_restore() {
    let mut f = Fixture::new();
    let lead = f.lead("lead").await;
    let lane = f.lane(lead, "lane", LanePolicy::Attached).await;
    let q = created(
        f.runtime
            .request_question_with_binding(
                lane.root_run_id,
                "restore?".into(),
                vec![],
                true,
                LaneDeliveryBinding::Bound,
            )
            .unwrap(),
    );
    let answered = f
        .runtime
        .resolve_lane_question(lead, request(&q, "answer", Some("agent scope")))
        .unwrap();
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::SelfOnly)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    f.runtime
        .resume_chat(
            lane.root_run_id,
            RunConfig {
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    f.waiting(lane.root_run_id).await;
    f.runtime
        .stop(lane.root_run_id, crate::StopScope::SelfOnly)
        .unwrap();
    f.runtime.wait(lane.root_run_id).await.unwrap();
    f.runtime
        .resume_chat(
            lane.root_run_id,
            RunConfig {
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    f.waiting(lane.root_run_id).await;
    f.runtime
        .send_message(lane.root_run_id, "followup after a second restore".into())
        .unwrap();
    f.waiting(lane.root_run_id).await;
    let last = f.model.requests.lock().unwrap().last().unwrap().clone();
    let answer_text = answered.answer_message();
    assert_eq!(
        last.iter()
            .filter(|m| m.role == providers::Role::User)
            .flat_map(|m| &m.content)
            .filter(|b| matches!(b,providers::ContentBlock::Text{text} if text==&answer_text))
            .count(),
        1
    );
    assert!(last.iter().any(|m|m.content.iter().any(|b|matches!(b,providers::ContentBlock::Text{text} if text.contains("followup after a second restore")))));
    f.stop(lane.root_run_id).await;
    f.stop(lead).await;
}

fn escaped_question_at_page_bytes(mut question: UserQuestion, bytes: usize) -> UserQuestion {
    question.responder = Some(QuestionResponder::Lead {
        thread_id: "lead".into(),
    });
    question.management_revision = Some(1);
    question.routing_revision = 1;
    question.title = format!("Q{}", "x".repeat(2047));
    question.options = vec![format!("A{}", "x".repeat(255)); 3];
    let base = serde_json::to_vec(&(Vec::<UserQuestion>::new(), &question))
        .unwrap()
        .len();
    let extra = bytes.checked_sub(base).unwrap();
    let controls = extra / 5;
    let quotes = extra % 5;
    let mut fields = vec!['x'; 2047 + 3 * 255];
    assert!(controls + quotes <= fields.len());
    fields[..controls].fill('\0');
    fields[controls..controls + quotes].fill('"');
    question.title = format!("Q{}", fields[..2047].iter().collect::<String>());
    question.options = fields[2047..]
        .chunks(255)
        .map(|chars| format!("A{}", chars.iter().collect::<String>()))
        .collect();
    assert_eq!(question.title.len(), 2048);
    assert!(question.options.iter().all(|s| s.len() == 256));
    assert_eq!(
        serde_json::to_vec(&(Vec::<UserQuestion>::new(), &question))
            .unwrap()
            .len(),
        bytes
    );
    question
}

#[tokio::test]
async fn escaped_questions_stay_listable_through_inheritance_and_reject_oversize() {
    let mut f = Fixture::new();
    let lead = f.lead("lead").await;
    let lane = f.lane(lead, "lane", LanePolicy::Attached).await;
    let template = f
        .runtime
        .request_user_question(lane.root_run_id, "template?".into(), vec![], false)
        .unwrap();
    let page_budget = storage::lane_delivery::LANE_DELIVERY_PAGE_BYTES - 1024;
    let boundary = escaped_question_at_page_bytes(template.clone(), page_budget);
    let accepted = created(
        f.runtime
            .request_question_with_binding(
                lane.root_run_id,
                boundary.title,
                boundary.options,
                false,
                LaneDeliveryBinding::Bound,
            )
            .unwrap(),
    );
    assert_eq!(
        serde_json::to_vec(&(Vec::<UserQuestion>::new(), &accepted))
            .unwrap()
            .len(),
        page_budget
    );
    let page = f.runtime.list_lane_questions(lead, None).unwrap();
    assert_eq!(page.questions.len(), 1);
    assert_eq!(page.questions[0].id, accepted.id);
    assert!(serde_json::to_vec(&page).unwrap().len() <= 16384);
    f.db.handle()
        .bind_user_questions(
            &lane.root_run_id.to_string(),
            "continuation-one",
            std::slice::from_ref(&accepted.id),
        )
        .unwrap();
    f.db.handle()
        .bind_user_questions(
            "continuation-one",
            "continuation-two",
            std::slice::from_ref(&accepted.id),
        )
        .unwrap();
    let inherited = f.runtime.user_question(&accepted.id).unwrap().unwrap();
    assert_eq!(
        inherited.recipient_run_ids,
        ["continuation-one", "continuation-two"]
    );
    assert!(
        serde_json::to_vec(&(Vec::<UserQuestion>::new(), &inherited))
            .unwrap()
            .len()
            > page_budget
    );
    let continued_page = f.runtime.list_lane_questions(lead, None).unwrap();
    assert_eq!(
        continued_page.questions, page.questions,
        "lead projections must not grow with continuation inheritance"
    );
    let excess = escaped_question_at_page_bytes(template, page_budget + 1);
    let error = f
        .runtime
        .request_question_with_binding(
            lane.root_run_id,
            excess.title,
            excess.options,
            false,
            LaneDeliveryBinding::Bound,
        )
        .unwrap_err();
    assert!(error.contains("question_exceeds_page_bytes"));
    let error = f
        .runtime
        .request_question_with_binding(
            lane.root_run_id,
            format!("Q{}", "\0".repeat(2047)),
            vec![format!("A{}", "\0".repeat(255)); 3],
            true,
            LaneDeliveryBinding::Bound,
        )
        .unwrap_err();
    assert!(error.contains("question_exceeds_page_bytes"));
    assert_eq!(
        f.runtime.user_answers(lane.root_run_id).unwrap().len(),
        2,
        "rejected questions must not remain as hidden blockers"
    );
    assert_eq!(
        f.db.handle()
            .lane_inbox("lead".into(), String::new())
            .unwrap()
            .len(),
        1,
        "no outbox notices for rejected questions"
    );
    let after = created(
        f.runtime
            .request_question_with_binding(
                lane.root_run_id,
                "after escaping boundary?".into(),
                vec![],
                true,
                LaneDeliveryBinding::Bound,
            )
            .unwrap(),
    );
    let first_page = f.runtime.list_lane_questions(lead, None).unwrap();
    let mut all = first_page.questions;
    if let Some(cursor) = first_page.next_cursor {
        all.extend(
            f.runtime
                .list_lane_questions(lead, Some(cursor))
                .unwrap()
                .questions,
        );
    }
    assert_eq!(all.len(), 2);
    assert!(all.iter().any(|q| q.id == after.id));
    assert!(all.iter().any(|q| q.id == accepted.id));
    assert!(all.iter().all(|q| q.recipient_run_ids.is_empty()));
    assert_eq!(
        f.runtime
            .user_question(&accepted.id)
            .unwrap()
            .unwrap()
            .recipient_run_ids,
        inherited.recipient_run_ids,
        "listing must not discard durable continuation links"
    );
    f.stop(lane.root_run_id).await;
    f.stop(lead).await;
}
